"""`scripts/shard-feature-sweep` builds shard-feature test harnesses one at a time (#2692)."""

import json
import os
import stat
import subprocess
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from scratch_git import scratch_tempdir  # noqa: E402

SCRIPT = Path(__file__).resolve().parents[1] / "shard-feature-sweep"
PREFIX = "SHARD_SWEEP_RECEIPT "

# A cargo stand-in: records every call, then answers like the real JSON message stream.
# `test-shard-bad` fails to compile, `test-shard-warn` compiles with one warning.
STUB = '''#!{python}
import json, os, sys
args = sys.argv[1:]
with open(os.environ["STUB_LOG"], "a") as log:
    log.write(json.dumps(args) + "\\n")
if args[0] == "clean":
    sys.exit(int(os.environ.get("STUB_CLEAN_EXIT", "0")))
features = args[args.index("--features") + 1] if "--features" in args else ""
package_id = "path+file:///x/crates/rsid#0.1.0"
def emit(value):
    print(json.dumps(value), flush=True)
emit({{"reason": "compiler-artifact", "package_id": package_id,
      "target": {{"kind": ["lib"]}}, "profile": {{"test": True}}}})
if "test-shard-warn" in features:
    emit({{"reason": "compiler-message", "package_id": package_id, "message": {{
        "level": "warning", "message": "unused variable: `x`", "code": {{"code": "unused_variables"}},
        "spans": [{{"is_primary": True, "file_name": "crates/rsid/src/lib.rs", "line_start": 3,
                    "column_start": 5}}], "rendered": "warning: unused variable\\n"}}}})
if "test-shard-bad" in features:
    emit({{"reason": "compiler-message", "package_id": package_id, "message": {{
        "level": "error", "message": "cannot find value", "code": None, "spans": [],
        "rendered": "error: cannot find value\\n"}}}})
    sys.exit(101)
'''


class ShardFeatureSweepTests(unittest.TestCase):
    def setUp(self):
        holder = scratch_tempdir("shard-sweep-")
        self.addCleanup(holder.cleanup)
        self.root = Path(holder.name)
        self.repo = self.root / "repo"
        self.repo.mkdir()
        self.log = self.root / "cargo.log"
        stub = self.root / "bin" / "cargo"
        stub.parent.mkdir()
        stub.write_text(STUB.format(python=sys.executable))
        stub.chmod(stub.stat().st_mode | stat.S_IXUSR)
        self.env = {
            "PATH": f"{stub.parent}{os.pathsep}{os.environ['PATH']}",
            "STUB_LOG": str(self.log),
            "HOME": str(self.root),
        }

    def sweep(self, *args, env=None):
        done = subprocess.run(
            [sys.executable, "-I", str(SCRIPT), "--repo", str(self.repo), "--base", "no-such-ref", *args],
            env={**self.env, **(env or {})}, capture_output=True, text=True, cwd=self.root,
        )
        lines = [line for line in done.stdout.splitlines() if line.startswith(PREFIX)]
        receipt = json.loads(lines[-1][len(PREFIX):]) if lines else None
        return done, receipt

    def calls(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()]

    def test_a_sweep_builds_every_shard_in_order_and_cleans_the_package_after_each(self):
        done, receipt = self.sweep("--package", "rsid", "--shard", "store-01", "--shard", "other-02")
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertTrue(receipt["ok"])
        self.assertEqual([e["shard"] for e in receipt["shards"]], ["store-01", "other-02"])
        self.assertEqual([e["exit_code"] for e in receipt["shards"]], [0, 0])
        self.assertEqual(self.calls(), [
            ["test", "-p", "rsid", "--lib", "--no-run", "--message-format=json",
             "--no-default-features", "--features", "test-shard-store-01"],
            ["clean", "-p", "rsid", "--profile", "test"],
            ["test", "-p", "rsid", "--lib", "--no-run", "--message-format=json",
             "--no-default-features", "--features", "test-shard-other-02"],
            ["clean", "-p", "rsid", "--profile", "test"],
        ])
        self.assertEqual(receipt["shards"][0]["warnings"], {"rsid": 0})
        self.assertTrue(done.stdout.splitlines()[-1].startswith(PREFIX), "the receipt is the last line")

    def test_a_failed_shard_does_not_stop_the_sweep_and_is_named_with_its_exit_code(self):
        done, receipt = self.sweep("--package", "rsid", "--shard", "bad", "--shard", "warn", "--shard", "store-01")
        self.assertEqual(done.returncode, 1)
        self.assertFalse(receipt["ok"])
        self.assertEqual([e["exit_code"] for e in receipt["shards"]], [101, 0, 0])
        self.assertEqual(receipt["shards"][0]["errors"], ["cannot find value"])
        self.assertEqual([e["warnings"] for e in receipt["shards"]], [{"rsid": 0}, {"rsid": 1}, {"rsid": 0}])
        builds = [call for call in self.calls() if call[0] == "test"]
        self.assertEqual(len(builds), 3)

    def test_a_shard_with_a_warning_above_the_baseline_fails_the_warning_gate(self):
        baseline = self.repo / "tools" / "warning-baseline.json"
        baseline.parent.mkdir()
        baseline.write_text(json.dumps({"schema": 1, "packages": {"rsid": {"configs": ["lib test"], "warnings": {}}}}))
        done, receipt = self.sweep("--package", "rsid", "--shard", "warn")
        self.assertEqual(done.returncode, 1)
        self.assertEqual(receipt["shards"][0]["exit_code"], 0)
        self.assertFalse(receipt["ok"])
        self.assertEqual(receipt["warnings"]["status"], "new_warnings")
        self.assertEqual(receipt["warnings"]["packages"]["rsid"]["new_total"], 1)
        self.assertIn("new warning x1", done.stdout)

    def test_a_sweep_within_the_baseline_passes_the_warning_gate(self):
        baseline = self.repo / "tools" / "warning-baseline.json"
        baseline.parent.mkdir()
        key = "crates/rsid/src/lib.rs|unused_variables|unused variable: `x`"
        baseline.write_text(json.dumps(
            {"schema": 1, "packages": {"rsid": {"configs": ["lib test"], "warnings": {key: 1}}}}))
        done, receipt = self.sweep("--package", "rsid", "--shard", "warn")
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertTrue(receipt["ok"])
        self.assertEqual(receipt["warnings"]["status"], "ok")
        self.assertEqual(receipt["warnings"]["packages"]["rsid"]["observed"], 1)

    def test_a_failed_clean_fails_the_job_because_the_disk_bound_did_not_hold(self):
        done, receipt = self.sweep("--package", "rsid", "--shard", "store-01", env={"STUB_CLEAN_EXIT": "1"})
        self.assertEqual(done.returncode, 1)
        self.assertFalse(receipt["ok"])
        self.assertEqual(receipt["shards"][0]["clean_exit_code"], 1)

    def test_explicit_features_are_one_build_with_the_requested_flags(self):
        done, receipt = self.sweep(
            "--package", "rsid-store", "--no-default-features", "--feature", "test-shard-store-01")
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(len(receipt["shards"]), 1)
        self.assertEqual(self.calls()[0], [
            "test", "-p", "rsid-store", "--lib", "--no-run", "--message-format=json",
            "--no-default-features", "--features", "test-shard-store-01"])

    def test_keep_artifacts_skips_the_clean(self):
        done, _ = self.sweep("--package", "rsid", "--shard", "store-01", "--keep-artifacts")
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual([call[0] for call in self.calls()], ["test"])

    def test_shards_and_features_together_and_non_name_tokens_are_refused_before_cargo_runs(self):
        for args in (
            ["--shard", "a", "--feature", "test-shard-a"],
            ["--shard", "--features"],
            ["--shard", "a b"],
            ["--shard", "../x"],
        ):
            done, receipt = self.sweep("--package", "rsid", *args)
            self.assertEqual(done.returncode, 2, args)
            self.assertIsNone(receipt)
        self.assertFalse(self.log.exists())


if __name__ == "__main__":
    unittest.main()
