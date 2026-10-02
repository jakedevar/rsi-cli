#!/usr/bin/env python3
"""Behavioral checks for the watchdog-only rsid supervisor."""

import os
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

    def deploy_layout(
        self, directory: Path, prev_body: str | None, inflight: bool = True
    ):
        """`bin/rsid` symlinks to `real/rsid` (as ~/.local/bin does); the first
        run exits 75, every later run of the "new" binary exits 1 at once."""
        real = directory / "real"
        real.mkdir()
        link_dir = directory / "bin"
        link_dir.mkdir()
        daemon = real / "rsid"
        daemon.write_text(
            "#!/usr/bin/env bash\n"
            'echo new >> "$(dirname "$(readlink -f "$0")")/starts"\n'
            'flag="$(dirname "$(readlink -f "$0")")/flag"\n'
            'if [[ ! -e "$flag" ]]; then touch "$flag"; exit 75; fi\n'
            "exit 1\n"
        )
        daemon.chmod(0o755)
        (link_dir / "rsid").symlink_to(daemon)
        if prev_body is not None:
            prev = real / "rsid.prev"
            prev.write_text("#!/usr/bin/env bash\n" + prev_body)
            prev.chmod(0o755)
        if inflight:
            (real / "rsid.deploy-inflight").write_text("deploy in flight\n")
        return link_dir / "rsid", real

    def test_binary_that_cannot_start_after_restart_falls_back_to_prev_once(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            link, real = self.deploy_layout(
                directory, 'echo prev >> "$(dirname "$(readlink -f "$0")")/starts"\nexit 0\n'
            )
            result = subprocess.run(
                [str(SUPERVISOR), str(link)],
                capture_output=True,
                text=True,
                timeout=10,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("restoring", result.stderr)
            # new (exit 75), new again (exit 1, falls back), then the old binary.
            self.assertEqual(
                (real / "starts").read_text().splitlines(), ["new", "new", "prev"]
            )
            self.assertTrue((real / "rsid.failed").exists())
            self.assertFalse((real / "rsid.prev").exists())
            self.assertFalse((real / "rsid.deploy-inflight").exists())

    def test_fallback_is_single_and_a_failing_prev_is_returned(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            link, real = self.deploy_layout(
                directory, 'echo prev >> "$(dirname "$(readlink -f "$0")")/starts"\nexit 3\n'
            )
            result = subprocess.run(
                [str(SUPERVISOR), str(link)],
                capture_output=True,
                text=True,
                timeout=10,
                check=False,
            )
            self.assertEqual(result.returncode, 3, result.stderr)
            self.assertEqual(
                (real / "starts").read_text().splitlines(), ["new", "new", "prev"]
            )
            self.assertEqual(result.stderr.count("restoring"), 1)

    def test_no_fallback_without_prev_or_after_the_fast_failure_window(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            link, real = self.deploy_layout(directory, None)
            result = subprocess.run(
                [str(SUPERVISOR), str(link)],
                capture_output=True,
                text=True,
                timeout=10,
                check=False,
            )
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertNotIn("restoring", result.stderr)
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            link, real = self.deploy_layout(directory, "exit 0\n")
            env = {**os.environ, "RSID_SUPERVISOR_FAST_FAILURE_SECONDS": "0"}
            result = subprocess.run(
                [str(SUPERVISOR), str(link)],
                capture_output=True,
                text=True,
                timeout=10,
                check=False,
                env=env,
            )
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertTrue((real / "rsid.prev").exists())

    def test_no_fallback_without_the_deploy_inflight_marker(self) -> None:
        # A stale `.prev` from an earlier, verified deploy must never be restored
        # after an unrelated fast crash following a watchdog restart.
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            link, real = self.deploy_layout(
                directory, 'echo prev >> "$(dirname "$(readlink -f "$0")")/starts"\nexit 0\n',
                inflight=False,
            )
            result = subprocess.run(
                [str(SUPERVISOR), str(link)],
                capture_output=True,
                text=True,
                timeout=10,
                check=False,
            )
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertNotIn("restoring", result.stderr)
            self.assertEqual((real / "starts").read_text().splitlines(), ["new", "new"])
            self.assertTrue((real / "rsid.prev").exists())
            self.assertFalse((real / "rsid.failed").exists())

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
