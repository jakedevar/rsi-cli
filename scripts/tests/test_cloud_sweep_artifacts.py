"""Cloud QA artifacts must account for every executed lane and seed state."""

from contextlib import redirect_stdout
import gzip
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
            self.assertEqual(report["shards"][0]["environment"],
                             {"spec_env": {"CARGO_BUILD_JOBS": "4"}, "tmpdir_class": "disk"})
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

    def test_failed_lane_log_excerpts_survive_collect(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "status").mkdir()
            (root / "logs").mkdir()
            self.write_lane(root, "other-05", 100,
                            "noise\nFAIL [ 00:01 ] (1/1) sandbox::t::shared\n"
                            "thread 'x' panicked at src/t.rs:1:1:\nassertion failed\n")
            self.write_lane(root, "session-01", 0, "all green\n")
            packed = subprocess.run(
                ["bash", str(ROOT / "scripts/cloud-sweep-excerpts.sh"), str(root)],
                check=True, capture_output=True,
            ).stdout
            out = root / "unpacked"
            out.mkdir()
            subprocess.run(["tar", "-xf", "-", "-C", str(out)], input=packed, check=True)
            self.assertEqual(sorted(p.name for p in out.iterdir()), ["other-05.log.gz", "other-05.txt"])
            excerpt = (out / "other-05.txt").read_text()
            self.assertIn("lane=other-05 exit=100", excerpt)
            self.assertIn("sandbox::t::shared", excerpt)
            self.assertIn("assertion failed", excerpt)
            with gzip.open(out / "other-05.log.gz", "rt") as full:
                self.assertIn("sandbox::t::shared", full.read())

    def collected_sweep(self, root, lanes, failures=(), harness=(), log=""):
        """A collected result dir: QA.md as the host report prints it, plus the
        failure-logs collect brings back."""
        (root / "failure-logs").mkdir(parents=True)
        names = list(failures) + [f"{lane}:build_or_harness_failure" for lane in harness]
        body = ["# Cloud QA sweep", "", "Tip SHA: `" + "a" * 40 + "`  ", f"Lanes: {lanes}  ", "",
                "Seed set: absent; classification unavailable",
                "Unclassified failing names (seed names alone do not verify failure signatures):"]
        body += [f"- `{name}`" for name in names] or ["- none"]
        body += ["FLAKE lines:", "- none observed", ""]
        (root / "QA.md").write_text("\n".join(body))
        if failures:
            with gzip.open(root / "failure-logs/other-05.log.gz", "wt") as handle:
                handle.write(log)

    def fake_classifier(self, root, known=()):
        """A classifier stub: KNOWN #7 for `known` names, NEW for the rest."""
        script = root / "fake-classifier"
        script.write_text(
            "#!/bin/sh\nlog=$3\n"
            "grep -o 'FAIL .*) test .*' \"$log\" | sed 's/.*) test //' | while read t; do\n"
            "  case \" " + " ".join(known) + " \" in *\" $t \"*) echo \"KNOWN #7 flake $t\";;"
            " *) echo \"NEW $t\";; esac\ndone\n")
        script.chmod(0o755)
        (root / "snapshot.json").write_text("{}")
        return script

    def run_verdict(self, root, classifier=None, snapshot=None):
        argv = [sys.executable, str(ROOT / "scripts/cloud-sweep-verdict.py"), str(root),
                "--snapshot", str(snapshot or root / "snapshot.json"),
                "--classifier", str(classifier or root / "missing-classifier")]
        run = subprocess.run(argv, check=True, capture_output=True, text=True,
                             env={"PATH": "/usr/bin:/bin", "HOME": str(root)})
        return (root / "QA.md").read_text(), run.stdout

    FAIL_LOG = ("FAIL [ 00:01 ] (1/2) test a::known\n"
                "FAIL [ 00:01 ] (2/2) test b::other\n")

    def test_verdict_green_when_every_red_is_known(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.collected_sweep(root, 7, ["a::known", "b::other"], log=self.FAIL_LOG)
            qa, _ = self.run_verdict(root, self.fake_classifier(root, ["a::known", "b::other"]))
            self.assertIn("- KNOWN #7 `a::known`", qa)
            self.assertEqual(qa.strip().splitlines()[-1], f"VERDICT GREEN {'a' * 40} new=0")
            self.assertIn("Known only: #7", qa)

    def test_verdict_red_counts_unknown_reds(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.collected_sweep(root, 7, ["a::known", "b::other"], log=self.FAIL_LOG)
            qa, _ = self.run_verdict(root, self.fake_classifier(root, ["a::known"]))
            self.assertIn("- NEW `b::other`", qa)
            self.assertEqual(qa.strip().splitlines()[-1], f"VERDICT RED {'a' * 40} new=1")

    def test_verdict_green_with_no_reds(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.collected_sweep(root, 7)
            qa, _ = self.run_verdict(root, self.fake_classifier(root))
            self.assertEqual(qa.strip().splitlines()[-1], f"VERDICT GREEN {'a' * 40} new=0")

    def test_verdict_incomplete_on_build_failure_or_no_lanes(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.collected_sweep(root, 7, harness=["other-05"])
            qa, _ = self.run_verdict(root, self.fake_classifier(root))
            self.assertIn("other-05:build_or_harness_failure", qa)
            self.assertEqual(qa.strip().splitlines()[-1], f"VERDICT INCOMPLETE {'a' * 40} new=0")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.collected_sweep(root, 0)
            qa, _ = self.run_verdict(root, self.fake_classifier(root))
            self.assertEqual(qa.strip().splitlines()[-1], f"VERDICT INCOMPLETE {'a' * 40} new=0")

    def test_verdict_states_missing_classifier_and_snapshot(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.collected_sweep(root, 7, ["a::known"], log=self.FAIL_LOG)
            qa, _ = self.run_verdict(root)
            self.assertIn("Classification unavailable: rsi-known-failure CLI not found", qa)
            self.assertIn("- NEW `a::known`", qa)
            self.assertEqual(qa.strip().splitlines()[-1], f"VERDICT RED {'a' * 40} new=1")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.collected_sweep(root, 7, ["a::known"], log=self.FAIL_LOG)
            classifier = self.fake_classifier(root, ["a::known"])
            qa, _ = self.run_verdict(root, classifier, snapshot=root / "absent.json")
            self.assertIn("Classification unavailable: snapshot", qa)
            self.assertIn("VERDICT RED", qa.strip().splitlines()[-1])

    # The nextest line shapes of session-04 in the sweep of 9450c30bb (#1120).
    CRASH_LOG = (
        "        PASS [   2.022s] ( 1/409) rsid session::tests::passed\n"
        "     SIGABRT [   0.512s] (177/409) rsid session::h2_rotation_successor_"
        "interleaving_matrix_preserves_one_authority_projection\n"
        "     TIMEOUT [ 244.093s] (178/409) rsid session::tests::timed_out\n"
        "     Summary [ 300.000s] 409 tests run: 407 passed, 1 timed out, 0 skipped\n"
        "error: test run failed\n"
    )

    def test_crashed_and_timed_out_tests_are_named_reds_not_incomplete(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "status").mkdir()
            (root / "logs").mkdir()
            self.write_lane(root, "rsid-session-04", 100, self.CRASH_LOG)
            self.write_lane(root, "rsid-store-01", 0, "all green\n")
            report = subprocess.run(
                [sys.executable, str(ROOT / "scripts/cloud-sweep-report.py"),
                 "a" * 40, "2026-09-27T00:00:00Z", "2026-09-27T00:01:00Z",
                 str(root), str(root / "missing-seed.txt")],
                check=True, capture_output=True, text=True).stdout
            crashed = "session::h2_rotation_successor_interleaving_matrix_preserves_one_authority_projection"
            self.assertIn(f"- `{crashed}`\n", report)
            self.assertIn(f"- `{crashed}` [crash]", report)
            self.assertIn("- `session::tests::timed_out` [timeout]", report)
            self.assertNotIn("build_or_harness_failure", report)
            (root / "QA.md").write_text(report)
            (root / "failure-logs").mkdir()
            with gzip.open(root / "failure-logs/rsid-session-04.log.gz", "wt") as handle:
                handle.write(self.CRASH_LOG)
            qa, _ = self.run_verdict(root)
            self.assertIn(f"- NEW `{crashed}` [crash]", qa)
            self.assertIn("- NEW `session::tests::timed_out` [timeout]", qa)
            self.assertIn("Crashed or timed out (NEW): 2", qa)
            self.assertEqual(qa.strip().splitlines()[-1], f"VERDICT RED {'a' * 40} new=2")
            # The nextest SIGABRT line is a failing name for the JSON report too.
            self.assertEqual(MODULE.failures(self.CRASH_LOG), sorted([
                crashed, "session::tests::timed_out"]))

    def test_unnamed_signal_crash_is_a_crash_red_not_a_harness_failure(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "status").mkdir()
            (root / "logs").mkdir()
            self.write_lane(root, "other-doctests", 101,
                            "error: process didn't exit successfully: `x` "
                            "(signal: 6, SIGABRT: process abort signal)\n")
            report = subprocess.run(
                [sys.executable, str(ROOT / "scripts/cloud-sweep-report.py"),
                 "a" * 40, "2026-09-27T00:00:00Z", "2026-09-27T00:01:00Z",
                 str(root), str(root / "missing-seed.txt")],
                check=True, capture_output=True, text=True).stdout
            self.assertIn("- `other-doctests:crash`", report)
            self.assertNotIn("build_or_harness_failure", report)


if __name__ == "__main__":
    unittest.main()
