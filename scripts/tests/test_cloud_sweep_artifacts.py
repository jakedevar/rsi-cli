"""Cloud QA artifacts must account for every executed lane and seed state."""

from contextlib import redirect_stdout
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))
SCRIPT = ROOT / "scripts/cloud-sweep-json.py"
SPEC = importlib.util.spec_from_file_location("cloud_sweep_json", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)
SHARDS = [f"{group}-{index:02d}" for group, count in
          (("store", 4), ("session", 5), ("memory", 2), ("other", 5))
          for index in range(1, count + 1)]
LANES = ("rsid-integrations", "rsid-bins", "rsid-doctests", "rsi", "rsi-common",
         "other-workspace", "other-doctests")


class CloudSweepArtifactsTest(unittest.TestCase):
    def write_lane(self, root, label, exit_code=0, log=""):
        (root / "status" / f"{label}.tsv").write_text(f"{label}\t{exit_code}\n")
        (root / "logs" / f"{label}.log").write_text(log)

    def json_report(self, root):
        def command(*args):
            if args == ("git", "rev-parse", "HEAD"):
                return "a" * 40
            if args == ("git", "rev-parse", "a" * 40 + "^{tree}"):
                return "b" * 40
            if args == ("rustc", "-Vv"):
                return "rustc 1.94\nhost: x86_64-unknown-linux-gnu"
            if args == ("cargo", "-V"):
                return "cargo 1.94"
            if args == ("cargo", "nextest", "--version"):
                return "nextest 0.9"
            if args[-2:] == ("scripts/check-rsid-test-shards.py", "--list-shards"):
                return "\n".join(SHARDS)
            raise AssertionError(args)

        output = io.StringIO()
        argv = [str(SCRIPT), "a" * 40, "2026-09-27T00:00:00Z",
                "2026-09-27T00:01:00Z", str(root), "nextest"]
        with mock.patch.object(MODULE, "command", side_effect=command), \
             mock.patch.object(MODULE, "blob", return_value="c" * 40), \
             mock.patch.object(sys, "argv", argv), redirect_stdout(output):
            MODULE.main()
        return json.loads(output.getvalue())

    def test_json_covers_extra_executed_lanes(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "status").mkdir()
            (root / "logs").mkdir()
            for shard in SHARDS:
                self.write_lane(root, f"rsid-{shard}", log=(
                    "Summary [ 00:01 ] 1 tests run: 1 passed, 0 failed, 0 skipped\n"))
            for lane in LANES:
                self.write_lane(root, lane)
            self.write_lane(root, "other-workspace", 100,
                            "FAIL [ 00:01 ] (1/1) test other::fails\n")

            report = self.json_report(root)
            self.assertTrue(report["complete"])
            self.assertIn("@", report["shards"][0]["fingerprint"]["inputs"]["host_class"])
            self.assertEqual(report["extra_lanes"], [
                {"name": "other-workspace", "exit_code": 100,
                 "failing_tests": ["other::fails"]},
                {"name": "other-doctests", "exit_code": 0, "failing_tests": []},
            ])
            (root / "status" / "other-workspace.tsv").unlink()
            self.assertFalse(self.json_report(root)["complete"])

    def test_absent_seed_does_not_label_failures_new(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "status").mkdir()
            (root / "logs").mkdir()
            self.write_lane(root, "other-workspace", 100,
                            "FAIL [ 00:01 ] (1/1) test other::fails\n")
            result = subprocess.run(
                [sys.executable, str(ROOT / "scripts/cloud-sweep-report.py"),
                 "a" * 40, "2026-09-27T00:00:00Z", "2026-09-27T00:01:00Z",
                 str(root), str(root / "missing-seed.txt")],
                check=True, capture_output=True, text=True,
            )
            self.assertIn("Seed set: absent; classification unavailable", result.stdout)
            self.assertIn("Unclassified failing names", result.stdout)
            self.assertIn("other::fails", result.stdout)
            self.assertNotIn("NEW failing test names", result.stdout)
            seed = root / "seed.txt"
            seed.write_text("other::fails\n")
            seeded = subprocess.run(
                [sys.executable, str(ROOT / "scripts/cloud-sweep-report.py"),
                 "a" * 40, "2026-09-27T00:00:00Z", "2026-09-27T00:01:00Z",
                 str(root), str(seed)],
                check=True, capture_output=True, text=True,
            )
            self.assertIn("Names absent from seed set: 0", seeded.stdout)
            self.assertIn("- `other::fails`", seeded.stdout)


if __name__ == "__main__":
    unittest.main()
