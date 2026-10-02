"""Failure extraction and shard environment for the rolling QA report."""

import importlib.util
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch


sys.dont_write_bytecode = True
SCRIPT = Path(__file__).resolve().parents[1] / "run-rolling-qa.py"
SPEC = importlib.util.spec_from_file_location("run_rolling_qa", SCRIPT)
QA = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(QA)


class FailureExtractionTest(unittest.TestCase):
    def test_captures_nextest_failures_and_signal_aborts(self):
        output = """\
        FAIL [   1.950s] (110/161) rsid::start_chained_workflow_rpc valid_master_improve_params_register_chain
     SIGABRT [   3.352s] (214/302) rsid rpc::tests::recursive_dag_second_live_dogfood_two_task_graph_runs_one_at_a_time
     SIGABRT [   3.352s] (214/302) rsid rpc::tests::recursive_dag_second_live_dogfood_two_task_graph_runs_one_at_a_time
"""
        self.assertEqual(QA.failures(output), [
            "rsid rpc::tests::recursive_dag_second_live_dogfood_two_task_graph_runs_one_at_a_time",
            "rsid::start_chained_workflow_rpc valid_master_improve_params_register_chain",
        ])

    def test_captures_cargo_test_failures(self):
        output = """\
test manual::tests::html_is_current ... FAILED
test every_research_json_sidecar_validates ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored
"""
        self.assertEqual(QA.failures(output), [
            "every_research_json_sidecar_validates", "manual::tests::html_is_current",
        ])


class ShardEnvironmentTest(unittest.TestCase):
    """#994: a shard records the spec env and TMPDIR class the lander keys on."""

    def test_shard_spec_env_is_the_gate_default(self):
        self.assertEqual(QA.SHARD_SPEC_ENV, {"CARGO_BUILD_JOBS": "4",
                                             "CARGO_PROFILE_DEV_DEBUG": "line-tables-only"})

    def test_tmpdir_class_separates_tmpfs_from_disk(self):
        if Path("/dev/shm").is_dir():
            self.assertEqual(QA.tmpdir_class("/dev/shm"), "tmpfs")
        with patch.object(QA.subprocess, "check_output", return_value="ext2/ext3\n"):
            self.assertEqual(QA.tmpdir_class("/srv/rsi/sweeps/tmp"), "disk")
        with patch.object(QA.subprocess, "check_output", return_value="ramfs\n"):
            self.assertEqual(QA.tmpdir_class("/run/ram"), "tmpfs")

    def test_run_passes_and_records_the_shard_environment(self):
        scratch = QA.ROOT / "target"
        scratch.mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch) as directory, \
             patch.dict(os.environ, {"TMPDIR": "/dev/shm", "CARGO_BUILD_JOBS": "16"}), \
             patch.object(QA, "tmpdir_class", side_effect=lambda path: f"class-of:{path}"), \
             patch("sys.stdout"):
            result = QA.run("store-01", [sys.executable, "-c",
                                         "import os; print(os.environ['CARGO_BUILD_JOBS'],"
                                         " os.environ['CARGO_PROFILE_DEV_DEBUG'])"],
                            Path(directory), QA.SHARD_SPEC_ENV)
            log = (QA.ROOT / result["log"]).read_text()
        self.assertEqual(result["exit"], 0)
        self.assertEqual(log.strip(), "4 line-tables-only")
        self.assertEqual(result["environment"], {"spec_env": QA.SHARD_SPEC_ENV,
                                                 "tmpdir_class": "class-of:/dev/shm"})

    def test_lanes_without_a_spec_env_record_none(self):
        scratch = QA.ROOT / "target"
        scratch.mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch) as directory, patch("sys.stdout"):
            result = QA.run("rsi", [sys.executable, "-c", "pass"], Path(directory))
        self.assertNotIn("environment", result)


if __name__ == "__main__":
    unittest.main()
