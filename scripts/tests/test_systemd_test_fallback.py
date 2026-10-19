"""Regression checks for the authorized systemd test fallback (#1717)."""

import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
import uuid


SCRIPT = Path(__file__).resolve().parents[1] / "systemd-test-fallback"


@unittest.skipUnless(shutil.which("bash"), "requires Bash")
class SystemdTestFallback(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="systemd-test-fallback-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.log = self.root / "test.log"
        self.exit_file = self.root / "test.exit"

    def execute(self, *command):
        return subprocess.run(
            ["bash", str(SCRIPT), "--execute", str(self.log), str(self.exit_file),
             "--", *command], capture_output=True, text=True, timeout=10)

    def test_success_records_zero_and_both_output_streams(self):
        result = self.execute("bash", "-c", "echo output; echo error >&2")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.exit_file.read_text(), "0\n")
        self.assertEqual(self.log.read_text(), "output\nerror\n")

    def test_command_failure_survives_successful_tee(self):
        result = self.execute("bash", "-c", "echo failed; exit 7")
        self.assertEqual(result.returncode, 7, result.stderr)
        self.assertEqual(self.exit_file.read_text(), "7\n")
        self.assertEqual(self.log.read_text(), "failed\n")

    def test_log_failure_cannot_record_green(self):
        self.log = self.root / "missing" / "test.log"
        result = self.execute("true")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(self.exit_file.read_text(), "1\n")

    @unittest.skipUnless(sys.platform.startswith("linux") and shutil.which("systemd-run"),
                         "requires Linux systemd")
    def test_real_unit_preserves_bash_variables_and_failure(self):
        if subprocess.run(["systemctl", "--user", "show-environment"],
                          capture_output=True).returncode:
            self.skipTest("requires a running systemd user manager")
        unit = "rsi-test-fallback-" + uuid.uuid4().hex
        self.addCleanup(lambda: subprocess.run(
            ["systemctl", "--user", "stop", unit], capture_output=True, timeout=10))
        # Neither systemd nor the outer shell may expand this Bash expression.
        command = ('value=preserved; printf "%s\\n" "$value" "$CARGO_TARGET_DIR"; '
                   '(exit 7) | cat; result=${PIPESTATUS[0]}; exit "$result"')
        environment = dict(os.environ, CARGO_TARGET_DIR=str(self.root / "target"))
        result = subprocess.run(
            ["bash", str(SCRIPT), unit, str(self.log), str(self.exit_file), "--",
             "bash", "-c", command], capture_output=True, text=True,
            env=environment, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        deadline = time.monotonic() + 10
        while not self.exit_file.exists() and time.monotonic() < deadline:
            time.sleep(0.05)
        self.assertEqual(self.exit_file.read_text(), "7\n")
        self.assertEqual(self.log.read_text(), f"preserved\n{self.root / 'target'}\n")


if __name__ == "__main__":
    unittest.main()
