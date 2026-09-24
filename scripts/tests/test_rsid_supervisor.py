#!/usr/bin/env python3
"""Behavioral checks for the watchdog-only rsid supervisor."""

from pathlib import Path
import signal
import subprocess
import tempfile
import time
import unittest


SUPERVISOR = Path(__file__).resolve().parents[1] / "rsid-supervisor.sh"


class SupervisorTests(unittest.TestCase):
    def make_daemon(self, directory: Path, body: str) -> Path:
        daemon = directory / "fake-rsid"
        daemon.write_text("#!/usr/bin/env bash\nset -u\n" + body)
        daemon.chmod(0o755)
        return daemon

    def test_watchdog_exit_restarts_once_then_stops_on_normal_exit(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            daemon = self.make_daemon(
                directory,
                'count_file="$(dirname "$0")/count"\n'
                'count=$(cat "$count_file" 2>/dev/null || echo 0)\n'
                'count=$((count + 1))\n'
                'echo "$count" > "$count_file"\n'
                'if [[ $count -eq 1 ]]; then exit 75; fi\n'
                'exit 0\n',
            )
            result = subprocess.run(
                [str(SUPERVISOR), str(daemon)],
                capture_output=True,
                text=True,
                timeout=5,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual((directory / "count").read_text().strip(), "2")

    def test_non_watchdog_failure_is_returned_without_restart(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            daemon = self.make_daemon(
                directory,
                'echo started >> "$(dirname "$0")/starts"\nexit 42\n',
            )
            result = subprocess.run(
                [str(SUPERVISOR), str(daemon)],
                capture_output=True,
                text=True,
                timeout=5,
                check=False,
            )
            self.assertEqual(result.returncode, 42, result.stderr)
            self.assertEqual((directory / "starts").read_text().splitlines(), ["started"])

    def test_persistent_watchdog_exit_stops_after_bounded_backoff(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            daemon = self.make_daemon(
                directory,
                'echo started >> "$(dirname "$0")/starts"\nexit 75\n',
            )
            result = subprocess.run(
                [str(SUPERVISOR), str(daemon)],
                capture_output=True,
                text=True,
                timeout=12,
                check=False,
            )
            self.assertEqual(result.returncode, 75, result.stderr)
            self.assertEqual(len((directory / "starts").read_text().splitlines()), 4)
            self.assertIn("restart budget exhausted", result.stderr)

    def test_term_during_backoff_does_not_relaunch(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            daemon = self.make_daemon(
                directory,
                'echo started >> "$(dirname "$0")/starts"\nexit 75\n',
            )
            process = subprocess.Popen(
                [str(SUPERVISOR), str(daemon)],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            try:
                deadline = time.monotonic() + 5
                while not (directory / "starts").exists():
                    self.assertIsNone(process.poll(), "supervisor exited before first child")
                    self.assertLess(time.monotonic(), deadline, "first child did not start")
                    time.sleep(0.01)
                time.sleep(0.1)
                process.send_signal(signal.SIGTERM)
                _, stderr = process.communicate(timeout=5)
                self.assertEqual(process.returncode, 0, stderr)
                self.assertEqual((directory / "starts").read_text().splitlines(), ["started"])
            finally:
                if process.poll() is None:
                    process.kill()
                    process.communicate(timeout=5)

    def test_term_is_forwarded_and_child_is_reaped(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            daemon = self.make_daemon(
                directory,
                'trap \'echo stopped > "$(dirname "$0")/stopped"; exit 0\' TERM\n'
                'echo ready > "$(dirname "$0")/ready"\n'
                'while :; do sleep 0.1; done\n',
            )
            process = subprocess.Popen(
                [str(SUPERVISOR), str(daemon)],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            try:
                deadline = time.monotonic() + 5
                while not (directory / "ready").exists():
                    self.assertIsNone(process.poll(), "supervisor exited before child was ready")
                    self.assertLess(time.monotonic(), deadline, "child did not become ready")
                    time.sleep(0.01)
                process.send_signal(signal.SIGTERM)
                _, stderr = process.communicate(timeout=5)
                self.assertEqual(process.returncode, 0, stderr)
                self.assertEqual((directory / "stopped").read_text().strip(), "stopped")
            finally:
                if process.poll() is None:
                    process.kill()
                    process.communicate(timeout=5)


if __name__ == "__main__":
    unittest.main()
