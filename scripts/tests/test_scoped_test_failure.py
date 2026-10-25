"""Signal-only failures remain red but do not consume a verification attempt."""

import contextlib
import io
import json
from pathlib import Path
import runpy
import tempfile
import unittest


SCRIPTS = Path(__file__).resolve().parents[1]
FAILURE = runpy.run_path(str(SCRIPTS / "scoped-test-failure.py"))
FIXTURES = Path(__file__).with_name("fixtures") / "scoped-test-failure"


class ScopedTestFailure(unittest.TestCase):
    def fixture(self, name):
        return (FIXTURES / (name + ".txt")).read_text()

    def test_signal_only_fixture(self):
        self.assertTrue(FAILURE["signal_only"](self.fixture("signal-only").splitlines()))

    def test_signal_and_compiler_error_fixture(self):
        self.assertFalse(FAILURE["signal_only"](
            self.fixture("signal-and-compiler-error").splitlines()))

    def test_test_failure_fixture_with_and_without_signal(self):
        for prefix in ("", self.fixture("signal-only")):
            with self.subTest(prefix=bool(prefix)):
                self.assertFalse(FAILURE["signal_only"](
                    (prefix + self.fixture("test-failure")).splitlines()))

    def test_signals_and_untyped_compiler_errors(self):
        for sig in (2, 9, 15):
            for log in (f"process didn't exit successfully (signal: {sig})",
                        "SCOPED_PROCESS_SIGNAL " + {2: "SIGINT", 9: "SIGKILL", 15: "SIGTERM"}[sig]):
                self.assertTrue(FAILURE["signal_only"]([log]))
                for error in ("error: linking failed", "\x1b[31merror[E0308]: bad type\x1b[0m",
                              "SCOPED_TEST_EXIT 1", "process exited (exit status: 1)"):
                    self.assertFalse(FAILURE["signal_only"]([error, log]), error)
        self.assertFalse(FAILURE["signal_only"](["process exited (signal: 11, SIGSEGV)"]))

    def test_receipt_emission_and_other_reds(self):
        runner = runpy.run_path(str(SCRIPTS / "scoped-test"))
        receipt = runner["RECEIPT"]
        with tempfile.TemporaryDirectory() as directory:
            log = Path(directory) / "rsid.log"
            log.write_text(self.fixture("signal-only"))
            base = {"emit": True, "log_dir": directory, "head": "a" * 40,
                    "packages": [{"package": "rsid", "status": "failed", "completed": 0}]}

            def emit(extra=None, code=1):
                receipt.clear()
                receipt.update(base | (extra or {}))
                output = io.StringIO()
                with contextlib.redirect_stdout(output):
                    runner["emit_receipt"](code)
                return json.loads(output.getvalue().split("SCOPED_TEST_RECEIPT ")[1])

            classified = emit()
            self.assertEqual(classified["failure_class"], "infrastructure_interrupted")
            self.assertEqual(classified["ok"], False)
            self.assertEqual(classified["head"], base["head"])
            for extra in ({"failures": [{"test": "tests::failed"}]},
                          {"static_guards": {"whitespace": {"status": "fail"}}},
                          {"python": [{"status": "failed"}]},
                          {"desktop": [{"status": "failed"}]},
                          {"warnings": {"status": "new_warnings"}},
                          {"unmatched_filters": ["rsid=missing"]},
                          {"packages": base["packages"] + [{"package": "other", "status": "running"}]},
                          {"packages": [{"package": "rsid", "status": "timed out"}]}):
                with self.subTest(extra=extra):
                    self.assertIsNone(emit(extra).get("failure_class"))
            self.assertIsNone(emit(code=124).get("failure_class"))
            self.assertIsNone(emit(code=0).get("failure_class"))
            # Full logs matter: a real error can be far outside the daemon's tail.
            log.write_text("error: linking failed\n" + "noise\n" * 10000 + self.fixture("signal-only"))
            self.assertIsNone(emit().get("failure_class"))
            log.write_text(self.fixture("test-failure") + self.fixture("signal-only"))
            self.assertIsNone(emit().get("failure_class"))
            log.unlink()
            self.assertIsNone(emit().get("failure_class"))

    def test_direct_process_signal_is_logged(self):
        runner = runpy.run_path(str(SCRIPTS / "scoped-test"))
        from unittest.mock import Mock, patch
        process = Mock()
        process.wait.return_value = -15
        # An EOF without launching or killing a real process.
        with patch("subprocess.Popen", return_value=process), patch("os.read", return_value=b""):
            lines = []
            self.assertEqual(runner["stream"](["test-binary"], lines.append), -15)
        self.assertEqual(lines, ["SCOPED_PROCESS_SIGNAL SIGTERM\n"])


if __name__ == "__main__":
    unittest.main()
