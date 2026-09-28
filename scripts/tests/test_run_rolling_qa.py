"""Failure extraction for the rolling QA report."""

import importlib.util
from pathlib import Path
import sys
import unittest


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


if __name__ == "__main__":
    unittest.main()
