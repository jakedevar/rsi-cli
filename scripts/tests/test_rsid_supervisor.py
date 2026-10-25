#!/usr/bin/env python3
"""Behavioral checks for the watchdog-only rsid supervisor."""

import json
import os
from pathlib import Path
import re
import shlex
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest import mock


SUPERVISOR = Path(__file__).resolve().parents[1] / "rsid-supervisor.sh"

# The supervisor reads these from its environment: its own refresh handoff
# (every RSID_SUPERVISOR_* key, e.g. RSID_SUPERVISOR_PATH, which overrides $0)
# and the systemd/holder listener handoff. A daemon-managed shell inherits the
# live supervisor's values, so a fixture must start from a scrubbed copy and
# state its intended handoff explicitly (#230). Authentication and custody
# variables (RSI_SESSION_TOKEN, RSI_SOCKET, ...) are deliberately not listed.
SUPERVISOR_HANDOFF_PREFIX = "RSID_SUPERVISOR_"
SUPERVISOR_HANDOFF_KEYS = ("RSI_LISTEN_FD", "LISTEN_PID", "LISTEN_FDS", "LISTEN_FDNAMES")


def is_supervisor_handoff_key(key: str) -> bool:
    return key.startswith(SUPERVISOR_HANDOFF_PREFIX) or key in SUPERVISOR_HANDOFF_KEYS


def without_supervisor_handoff(environ: "os._Environ[str] | dict[str, str]") -> dict[str, str]:
    """A copy of `environ` with every supervisor handoff variable removed."""
    return {key: value for key, value in environ.items() if not is_supervisor_handoff_key(key)}


def fast_poll_env() -> dict[str, str]:
    """The current environment with a 0.1 s supervisor poll, for timing tests.

    Bash before 4.3 (macOS /bin/bash 3.2, #2936) has no `wait -n`, so the
    supervisor notices a daemon exit only at its next poll; a short poll keeps
    those tests' timing bounds about the restart logic, not the poll interval.
    """
    return dict(os.environ, RSID_SUPERVISOR_POLL_SECONDS="0.1")


class SupervisorTests(unittest.TestCase):
    def setUp(self) -> None:
        # The supervisor writes a shutdown marker and incident file; never let a
        # test touch the operator's real ~/.rsi (#2561).
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.state = Path(scratch.name)
        # Scrub the inherited handoff before applying fixture-owned values; the
        # patch restores the process environment exactly when the test ends.
        scrubbed = mock.patch.dict(os.environ, {})
        scrubbed.start()
        self.addCleanup(scrubbed.stop)
        for key in [key for key in os.environ if is_supervisor_handoff_key(key)]:
            del os.environ[key]
        patched = mock.patch.dict(os.environ, {
            "RSID_SUPERVISOR_SHUTDOWN_MARKER": str(self.state / "shutdown-begun"),
            "RSID_SUPERVISOR_INCIDENT_FILE": str(self.state / "forced-shutdown-incident"),
            "RSID_SUPERVISOR_RESTART_BUDGET_FILE": str(self.state / "deploy-restart-budget"),
            "RSID_SUPERVISOR_INTENDED_STOP_FILE": str(self.state / "intended-stop"),
            # Tests that exercise the settle window set it themselves (#3433).
            "RSID_SUPERVISOR_UNANNOUNCED_SETTLE_SECONDS": "0",
        })
        patched.start()
        self.addCleanup(patched.stop)

    @unittest.skipUnless(sys.platform.startswith("linux"), "systemd activation is Linux-only")
    def test_managed_invocation_retains_listener_and_lease_through_refresh_and_rollback(self) -> None:
        # Execute the shipped ExecStart with a systemd-shaped fd/env handoff.
        # No live user unit, database, socket or installed binary is touched.
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            install = directory / ".rsi/install"
            install.mkdir(parents=True)
            links = directory / ".local/bin"
            links.mkdir(parents=True)
            supervisor = self.make_supervisor(install)
            daemon = install / "rsid"
            previous = install / "rsid.prev"
            for path, role in [(daemon, "new"), (previous, "prev")]:
                # A real rsid carries the RSI_LISTEN_FD literal; the fallback gate
                # (#1801) looks for it in the previous binary.
                path.write_text(f'#!/bin/sh\n# reads RSI_LISTEN_FD\nexec {shlex.quote(sys.executable)} '
                                f'{shlex.quote(str(directory / "daemon.py"))} {role} "$@"\n')
                path.chmod(0o755)
            previous_contents = previous.read_bytes()
            candidate_contents = daemon.read_bytes()
            (links / "rsid").symlink_to(daemon)
            database = directory / "rsi.db"
            database.write_text("candidate writes")
            (install / "rsid.deploy-inflight").write_text(f"database={database}")
            (directory / "daemon.py").write_text('''
import fcntl, json, os, pathlib, socket, sys
root = pathlib.Path(__file__).parent
role = sys.argv[1]
assert os.environ["RSI_LISTEN_FD"] == "3"
assert all(key not in os.environ for key in ("LISTEN_PID", "LISTEN_FDS", "LISTEN_FDNAMES"))
listener = socket.fromfd(3, socket.AF_UNIX, socket.SOCK_STREAM)
assert listener.getsockname() == str(root / "daemon.sock")
assert listener.getsockopt(socket.SOL_SOCKET, socket.SO_ACCEPTCONN) == 1
lease = (root / "rsi.db.lock").open("a")
fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
recovery = len(sys.argv) > 2 and sys.argv[2] == "--restore-pre-migration-db"
stage = "recover" if recovery else role
with (root / "starts").open("a") as log:
    log.write(json.dumps([stage, os.getpid(), os.getppid(), (root / "daemon.sock").stat().st_ino]) + "\\n")
if recovery:
    assert sys.argv[3:] == [str(root / "rsi.db"), str(root / ".rsi/install/rsid.prev")]
    (root / "rsi.db").write_text("restored")
    sys.exit(0)
if role == "new":
    if not (root / "started").exists():
        (root / "started").touch()
        # A deploy can replace the supervisor too. Its exec must retain fd 3.
        with (root / ".rsi/install/rsid-supervisor.sh").open("a") as script:
            script.write("\\n# next supervisor incarnation\\n")
        sys.exit(75)
    sys.exit(1)
assert (root / "rsi.db").read_text() == "restored"
listener.settimeout(3)
client, _ = listener.accept()
with client:
    client.sendall(b"rolled back")
''')
            service = (SUPERVISOR.parent / "systemd/rsid.service").read_text()
            start = next(line.removeprefix("ExecStart=") for line in service.splitlines()
                         if line.startswith("ExecStart="))
            command = [arg.replace("%h", root) for arg in shlex.split(start)]
            self.assertEqual(command, [str(supervisor), str(links / "rsid")])
            # Emulate systemd's process-addressed activation without preexec_fn.
            activation = '''
import os, sys
os.dup2(int(sys.argv[1]), 3, inheritable=True)
os.environ["LISTEN_PID"] = str(os.getpid())
os.environ["LISTEN_FDS"] = "1"
os.environ["LISTEN_FDNAMES"] = "rsid"
os.execv(sys.argv[2], sys.argv[2:])
'''
            env = without_supervisor_handoff(os.environ)
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener:
                listener.bind(str(directory / "daemon.sock"))
                os.chmod(directory / "daemon.sock", 0o600)
                listener.listen()
                before = (directory / "daemon.sock").stat()
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
                    client.settimeout(10)
                    client.connect(str(directory / "daemon.sock"))
                    process = subprocess.Popen(
                        [sys.executable, "-c", activation, str(listener.fileno()), *command],
                        pass_fds=(listener.fileno(),), env=env, text=True,
                        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                    )
                    try:
                        _, stderr = process.communicate(timeout=10)
                        self.assertEqual(process.returncode, 0, stderr)
                        self.assertEqual(client.recv(100), b"rolled back")
                    finally:
                        if process.poll() is None:
                            process.kill()
                            process.communicate(timeout=5)
                after = (directory / "daemon.sock").stat()
            starts = [json.loads(line) for line in (directory / "starts").read_text().splitlines()]
            self.assertEqual([row[0] for row in starts], ["new", "new", "recover", "prev"])
            self.assertEqual([row[2] for row in starts], [process.pid] * 4)
            self.assertEqual(len({row[1] for row in starts}), 4)
            self.assertEqual([row[3] for row in starts], [before.st_ino] * 4)
            self.assertEqual((after.st_ino, after.st_mode, after.st_uid),
                             (before.st_ino, before.st_mode, before.st_uid))
            self.assertIn("refreshing rsid supervisor", stderr)
            self.assertIn("restoring", stderr)
            self.assertEqual((links / "rsid").read_bytes(), previous_contents)
            self.assertEqual((install / "rsid.failed").read_bytes(), candidate_contents)
            # Every daemon/recovery process released the single-writer lease.
            import fcntl
            with (directory / "rsi.db.lock").open("a") as lease:
                fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)

    def test_owned_activation_requires_one_fd_and_preserves_explicit_holder_handoff(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            daemon = self.make_daemon(directory, 'echo "${RSI_LISTEN_FD:-none}"\nexit 0\n')
            command = ["bash", "-c", 'export LISTEN_PID=$$; exec "$@"',
                       "activation", str(SUPERVISOR), str(daemon)]
            env = without_supervisor_handoff(os.environ)
            for count in ["2", "invalid"]:
                result = subprocess.run(command, env={**env, "LISTEN_FDS": count},
                                        capture_output=True, text=True, timeout=5)
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertIn("exactly one", result.stderr)
            result = subprocess.run(command, env={**env, "LISTEN_FDS": "2", "RSI_LISTEN_FD": "9"},
                                    capture_output=True, text=True, timeout=5)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout.strip(), "9")
            result = subprocess.run([str(SUPERVISOR), str(daemon)],
                                    env={**env, "LISTEN_PID": "1", "LISTEN_FDS": "2"},
                                    capture_output=True, text=True, timeout=5)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout.strip(), "none")

    def run_until_exhausted(self, argv, env=None, timeout=30):
        """Run the supervisor until it reports an exhausted restart budget, check
        it is still alive (it backs off, it does not exit, #3492), then stop it.
        Returns (returncode, stderr, seconds until the budget was reported)."""
        began = time.monotonic()
        process = subprocess.Popen(
            argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env,
        )
        lines: list[str] = []
        seen = threading.Event()
        reported: list[float] = []

        def pump() -> None:
            for line in process.stderr:
                lines.append(line)
                if "budget exhausted" in line and not seen.is_set():
                    reported.append(time.monotonic() - began)
                    seen.set()

        thread = threading.Thread(target=pump, daemon=True)
        thread.start()
        try:
            self.assertTrue(seen.wait(timeout), "".join(lines))
            time.sleep(0.3)
            self.assertIsNone(process.poll(), "the supervisor exited: " + "".join(lines))
            process.send_signal(signal.SIGTERM)
            process.wait(timeout=10)
        finally:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=5)
        thread.join(timeout=5)
        process.stderr.close()
        process.stdout.close()
        return process.returncode, "".join(lines), reported[0]

    def make_supervisor(self, directory: Path) -> Path:
        supervisor = directory / "rsid-supervisor.sh"
        supervisor.write_text(SUPERVISOR.read_text())
        supervisor.chmod(0o755)
        return supervisor

    def test_reexec_preserves_pid_and_restart_budget_across_every_refresh(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            supervisor = self.make_supervisor(directory)
            # Change the script on every daemon run. Resetting history at exec
            # would let this watchdog loop continue past the four-run budget.
            daemon = self.make_daemon(
                directory,
                'cd "$(dirname "$0")"\n'
                'echo "$$ $PPID" >> starts\n'
                'cp rsid-supervisor.sh rsid-supervisor.sh.next\n'
                'echo "# next incarnation $(wc -l < starts)" >> rsid-supervisor.sh.next\n'
                'mv rsid-supervisor.sh.next rsid-supervisor.sh\n'
                'exit 75\n',
            )
            returncode, stderr, _ = self.run_until_exhausted([str(supervisor), str(daemon)])
            self.assertEqual(returncode, 0, stderr)
            starts = (directory / "starts").read_text().splitlines()
            self.assertEqual(len(starts), 4)
            self.assertEqual(len({int(line.split()[1]) for line in starts}), 1, starts)
            self.assertEqual(stderr.count("refreshing rsid supervisor"), 3)
            self.assertIn("restart budget exhausted", stderr)
            self.assertTrue(supervisor.with_name("rsid-supervisor.sh.last-good").is_file())
            self.assertEqual(list(directory.glob(".rsid-supervisor.*")), [])

    def test_syntax_invalid_refresh_keeps_the_running_supervisor(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            supervisor = self.make_supervisor(directory)
            daemon = self.make_daemon(
                directory,
                'cd "$(dirname "$0")"\n'
                'echo started >> starts\n'
                'if [[ $(wc -l < starts) -eq 1 ]]; then\n'
                '  echo "if then" > rsid-supervisor.sh.next\n'
                '  mv rsid-supervisor.sh.next rsid-supervisor.sh\n'
                '  exit 75\n'
                'fi\nexit 0\n',
            )
            result = subprocess.run(
                [str(supervisor), str(daemon)], capture_output=True,
                text=True, timeout=5,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual((directory / "starts").read_text().splitlines(), ["started"] * 2)
            self.assertIn("supervisor refresh refused", result.stderr)
            self.assertEqual(list(directory.glob(".rsid-supervisor.*")), [])

    def test_refresh_preserves_fast_failure_fallback_state(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            supervisor = self.make_supervisor(directory)
            link, real = self.deploy_layout(
                directory,
                'echo prev >> "$(dirname "$(readlink -f "$0")")/starts"\nexit 0\n',
            )
            # Swap supervisor together with rsid on the initial deploy exit.
            script = link.resolve()
            body = script.read_text().replace(
                'touch "$flag"; exit 75;',
                'touch "$flag"; '
                f'cp "{supervisor}" "{supervisor}.next"; '
                f'echo "# refreshed" >> "{supervisor}.next"; '
                f'mv "{supervisor}.next" "{supervisor}"; exit 75;',
            )
            script.write_text(body)
            result = subprocess.run(
                [str(supervisor), str(link)], capture_output=True,
                text=True, timeout=10,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual((real / "starts").read_text().splitlines(), ["new", "new", "prev"])
            self.assertIn("refreshing rsid supervisor", result.stderr)
            self.assertIn("restoring", result.stderr)

    def test_refresh_preserves_the_single_fallback_limit(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            supervisor = self.make_supervisor(directory)
            link, real = self.deploy_layout(
                directory,
                'cd "$(dirname "$(readlink -f "$0")")"\n'
                'echo prev >> starts\n'
                'if [[ ! -e old-started ]]; then\n'
                '  touch old-started\n'
                f'  cp "{supervisor}" "{supervisor}.next"\n'
                f'  echo "# refresh after fallback" >> "{supervisor}.next"\n'
                f'  mv "{supervisor}.next" "{supervisor}"\n'
                '  cp rsid rsid.prev\n'
                '  echo "deploy in flight" > rsid.deploy-inflight\n'
                '  exit 75\n'
                'fi\nexit 3\n',
            )
            result = subprocess.run(
                [str(supervisor), str(link)], capture_output=True,
                text=True, timeout=12,
            )
            self.assertEqual(result.returncode, 3, result.stderr)
            self.assertEqual((real / "starts").read_text().splitlines(), ["new", "new", "prev", "prev"])
            self.assertEqual(result.stderr.count("restoring"), 1)
            self.assertIn("refreshing rsid supervisor", result.stderr)

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

    def test_persistent_watchdog_exit_waits_out_the_budget_after_bounded_backoff(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            daemon = self.make_daemon(
                directory,
                'echo started >> "$(dirname "$0")/starts"\nexit 75\n',
            )
            # The asserted backoff is 1 + 2 + 4 = 7 s; the helper allows 30 s (#136).
            returncode, stderr, _ = self.run_until_exhausted([str(SUPERVISOR), str(daemon)])
            self.assertEqual(returncode, 0, stderr)
            self.assertEqual(len((directory / "starts").read_text().splitlines()), 4)
            self.assertIn("restart budget exhausted", stderr)
            # #3492: it waits for room instead of exiting and leaving no rsid.
            self.assertIn("not exiting: restarting the daemon in", stderr)
            # A crash-style exit 75 (no deploy marker) keeps the 1, 2, 4 s backoff.
            for seconds in (1, 2, 4):
                self.assertIn(f"waiting {seconds}s", stderr)
            self.assertNotIn("no backoff", stderr)

    def test_deploy_requested_exit_75_restarts_without_backoff_and_keeps_the_budget(self) -> None:
        # #2355: the daemon writes the deploy marker, then exits 75. The restart
        # counts against the budget but never sleeps, including the 3rd in a window.
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            daemon = self.make_daemon(
                directory,
                'echo started >> "$(dirname "$0")/starts"\n'
                'echo "database=/nonexistent" > "$(dirname "$0")/fake-rsid.deploy-inflight"\n'
                'exit 75\n',
            )
            returncode, stderr, elapsed = self.run_until_exhausted(
                [str(SUPERVISOR), str(daemon)], env=fast_poll_env()
            )
            self.assertEqual(returncode, 0, stderr)
            self.assertEqual(len((directory / "starts").read_text().splitlines()), 4)
            self.assertIn("restart budget exhausted", stderr)
            self.assertEqual(stderr.count("deploy restart"), 3, stderr)
            self.assertNotIn("waiting", stderr)
            # Three backoffs would cost 1 + 2 + 4 s; the budget is unchanged.
            self.assertLess(elapsed, 2.0, stderr)

    def test_stale_deploy_marker_does_not_waive_the_backoff(self) -> None:
        # A marker older than this child's start was not written by it.
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            daemon = self.make_daemon(directory, 'echo started >> "$(dirname "$0")/starts"\nexit 75\n')
            marker = directory / "fake-rsid.deploy-inflight"
            marker.write_text("database=/nonexistent")
            os.utime(marker, (time.time() - 3600, time.time() - 3600))
            process = subprocess.Popen(
                [str(SUPERVISOR), str(daemon)], stdout=subprocess.PIPE,
                stderr=subprocess.PIPE, text=True, env=fast_poll_env(),
            )
            try:
                time.sleep(0.6)
                process.send_signal(signal.SIGTERM)
                _, stderr = process.communicate(timeout=5)
            finally:
                if process.poll() is None:
                    process.kill()
                    process.communicate(timeout=5)
            self.assertIn("waiting 1s", stderr)
            self.assertNotIn("no backoff", stderr)

    def test_operator_restart_setting_drives_watchdog_budget_with_rollback_reserve(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            budget_file = directory / "deploy-restart-budget"
            budget_file.write_text("2\n")
            daemon = self.make_daemon(
                directory,
                'echo started >> "$(dirname "$0")/starts"\n'
                f'if [[ ! -e "$(dirname "$0")/updated" ]]; then touch "$(dirname "$0")/updated"; printf \'%s\\n\' 4 > {shlex.quote(str(budget_file))}; fi\n'
                'exit 75\n',
            )
            env = os.environ.copy()
            env["RSID_SUPERVISOR_RESTART_BUDGET_FILE"] = str(budget_file)
            fake_bin = directory / "bin"
            fake_bin.mkdir()
            fake_sleep = fake_bin / "sleep"
            fake_sleep.write_text("#!/bin/sh\nexit 0\n")
            fake_sleep.chmod(0o755)
            env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"
            returncode, stderr, _ = self.run_until_exhausted(
                [str(SUPERVISOR), str(daemon)], env=env, timeout=8
            )
            self.assertEqual(returncode, 0, stderr)
            self.assertEqual(len((directory / "starts").read_text().splitlines()), 6)
            self.assertIn("restart budget exhausted (5 in 3600 seconds)", stderr)

    def deploy_layout(
        self, directory: Path, prev_body: str | None, inflight: bool = True, recovery_status: int = 0
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
            'if [[ ${1:-} == --restore-pre-migration-db ]]; then\n'
            'echo recover >> "$(dirname "$(readlink -f "$0")")/starts"\n'
            f"exit {recovery_status}\nfi\n"
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

    def test_fallback_onto_binary_without_listen_fd_support_is_refused_by_name(self) -> None:
        # #1801: a pre-holder .prev ignores RSI_LISTEN_FD and would bind the
        # socket path itself. The supervisor must name the refusal and leave the
        # database, binaries and deploy marker untouched.
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            link, real = self.deploy_layout(
                directory, 'echo prev >> "$(dirname "$(readlink -f "$0")")/starts"\nexit 0\n'
            )
            (real / "rsid.deploy-inflight").write_text(f"database={directory / 'rsi.db'}")
            env = {**os.environ, "RSI_LISTEN_FD": "3"}
            result = subprocess.run([str(SUPERVISOR), str(link)], capture_output=True, text=True,
                                    timeout=10, env=env)
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertIn("rsid fallback refused", result.stderr)
            self.assertIn("RSI_LISTEN_FD", result.stderr)
            self.assertEqual((real / "starts").read_text().splitlines(), ["new", "new"])
            self.assertTrue((real / "rsid.prev").exists())
            self.assertTrue((real / "rsid.deploy-inflight").exists())
            self.assertFalse((real / "rsid.failed").exists())

    def test_fallback_onto_binary_with_listen_fd_support_is_allowed(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            link, real = self.deploy_layout(
                directory,
                '# reads RSI_LISTEN_FD\necho prev >> "$(dirname "$(readlink -f "$0")")/starts"\nexit 0\n',
            )
            env = {**os.environ, "RSI_LISTEN_FD": "3"}
            result = subprocess.run([str(SUPERVISOR), str(link)], capture_output=True, text=True,
                                    timeout=10, env=env)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual((real / "starts").read_text().splitlines(), ["new", "new", "prev"])

    def test_install_docs_publish_the_supervisor_before_the_unit(self) -> None:
        # #1801: rsid.service ExecStart needs ~/.rsi/install/rsid-supervisor.sh;
        # a unit installed or reloaded before the script exists fails 203/EXEC.
        service = (SUPERVISOR.parent / "systemd/rsid.service").read_text()
        self.assertIn("ExecStart=%h/.rsi/install/rsid-supervisor.sh ", service)
        doc = (SUPERVISOR.parents[1] / "docs/operations/rsid-service.md").read_text()
        block = doc.split("```bash", 1)[1].split("```", 1)[0]
        lines = [line for line in block.splitlines() if line.strip()]
        script = next(i for i, line in enumerate(lines)
                      if "scripts/rsid-supervisor.sh" in line and "~/.rsi/install/rsid-supervisor.sh" in line)
        unit = next(i for i, line in enumerate(lines) if "systemd/rsid.service" in line)
        reload = next(i for i, line in enumerate(lines) if "daemon-reload" in line)
        self.assertLess(script, unit)
        self.assertLess(unit, reload)
        self.assertTrue(lines[script].startswith("install -m 755 "), lines[script])
        self.assertTrue(any(line.startswith("mkdir -p") and "~/.rsi/install" in line
                            for line in lines[:script]))

    def test_fast_failure_restores_database_before_starting_previous_binary(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            link, real = self.deploy_layout(
                directory, 'echo prev >> "$(dirname "$(readlink -f "$0")")/starts"\nexit 0\n'
            )
            (real / "rsid.deploy-inflight").write_text(f"database={directory / 'rsi.db'}")
            result = subprocess.run([str(SUPERVISOR), str(link)], capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual((real / "starts").read_text().splitlines(), ["new", "new", "recover", "prev"])

    def test_failed_database_restore_refuses_binary_fallback(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            link, real = self.deploy_layout(directory, "exit 0\n", recovery_status=9)
            (real / "rsid.deploy-inflight").write_text(f"database={directory / 'rsi.db'}")
            result = subprocess.run([str(SUPERVISOR), str(link)], capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertEqual((real / "starts").read_text().splitlines(), ["new", "new", "recover"])
            self.assertTrue((real / "rsid.prev").exists())
            self.assertTrue((real / "rsid.deploy-inflight").exists())

    def test_verifier_request_is_recovered_before_first_supervisor_launch(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            daemon = self.make_daemon(directory, 'echo old >> "$(dirname "$0")/starts"\nexit 0\n')
            recovery = daemon.with_name("fake-rsid.failed")
            recovery.write_text('#!/usr/bin/env bash\necho recover >> "$(dirname "$0")/starts"\nexit 0\n')
            recovery.chmod(0o755)
            daemon.with_name("fake-rsid.db-rollback").write_text(str(directory / "rsi.db"))
            result = subprocess.run([str(SUPERVISOR), str(daemon)], capture_output=True, text=True, timeout=5)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual((directory / "starts").read_text().splitlines(), ["recover", "old"])

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

    # ---- #2561: shutdown backstop -------------------------------------------

    def backstop_env(self, grace: int) -> dict[str, str]:
        """Snapshot line 1 is the restart budget, line 2 the shutdown grace."""
        Path(os.environ["RSID_SUPERVISOR_RESTART_BUDGET_FILE"]).write_text(f"2\n{grace}\n")
        env = os.environ.copy()
        env["RSID_SUPERVISOR_POLL_SECONDS"] = "0.2"
        env["RSID_SUPERVISOR_TERM_WAIT_SECONDS"] = "1"
        return env

    # A stub rsid that begins its shutdown (touches the marker the supervisor
    # named), ignores SIGTERM and never exits; a child in its group sleeps on.
    WEDGED = (
        'dir="$(dirname "$0")"\n'
        'echo "$$" >> "$dir/starts"\n'
        'if [[ -e "$dir/wedged-once" ]]; then exit 0; fi\n'
        'touch "$dir/wedged-once"\n'
        'sleep 300 &\n'
        'echo "$!" > "$dir/grandchild"\n'
        'trap "" TERM\n'
        'echo "pid=$$ reason=sigterm" > "$RSID_SHUTDOWN_MARKER"\n'
        'while :; do sleep 1; done\n'
    )

    @unittest.skipIf(sys.platform == "win32", "process-group kill is Unix-only (#2561)")
    def test_wedged_shutdown_is_killed_within_the_grace_period_and_restarted(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            daemon = self.make_daemon(directory, self.WEDGED)
            began = time.monotonic()
            result = subprocess.run(
                [str(SUPERVISOR), str(daemon)], capture_output=True, text=True,
                timeout=30, check=False, env=self.backstop_env(grace=2),
            )
            elapsed = time.monotonic() - began
            self.assertEqual(result.returncode, 0, result.stderr)
            # Killed after the 2 s grace + 1 s second chance (+ the 1 s restart
            # backoff), nowhere near the stub's endless loop; then restarted.
            self.assertLess(elapsed, 15, result.stderr)
            self.assertEqual(len((directory / "starts").read_text().splitlines()), 2)
            self.assertIn("sending SIGTERM, then SIGKILL", result.stderr)
            incident = (self.state / "forced-shutdown-incident").read_text()
            self.assertIn("trigger=daemon_shutdown", incident)
            self.assertIn("grace_seconds=2", incident)
            self.assertIn("signal=KILL", incident)
            first_pid = (directory / "starts").read_text().splitlines()[0]
            self.assertIn(f"pid={first_pid}", incident)
            # The whole process group went, not only the daemon.
            grandchild = int((directory / "grandchild").read_text())
            with self.assertRaises(ProcessLookupError):
                os.kill(grandchild, 0)

    @unittest.skipIf(sys.platform == "win32", "process-group kill is Unix-only (#2561)")
    def test_shutdown_that_finishes_within_the_grace_period_is_left_alone(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            daemon = self.make_daemon(
                directory,
                'dir="$(dirname "$0")"\n'
                'echo started >> "$dir/starts"\n'
                'if [[ -e "$dir/done" ]]; then exit 0; fi\n'
                'touch "$dir/done"; echo "pid=$$ reason=sigterm" > "$RSID_SHUTDOWN_MARKER"\n'
                'sleep 1\n'
                'exit 75\n',
            )
            result = subprocess.run(
                [str(SUPERVISOR), str(daemon)], capture_output=True, text=True,
                timeout=30, check=False, env=self.backstop_env(grace=8),
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(len((directory / "starts").read_text().splitlines()), 2)
            self.assertNotIn("SIGKILL", result.stderr)
            self.assertFalse((self.state / "forced-shutdown-incident").exists())

    @unittest.skipIf(sys.platform == "win32", "process-group kill is Unix-only (#2561)")
    def test_a_running_daemon_is_never_killed_without_a_shutdown(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            daemon = self.make_daemon(
                directory,
                'echo started >> "$(dirname "$0")/starts"\nsleep 4\nexit 0\n',
            )
            result = subprocess.run(
                [str(SUPERVISOR), str(daemon)], capture_output=True, text=True,
                timeout=30, check=False, env=self.backstop_env(grace=1),
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(len((directory / "starts").read_text().splitlines()), 1)
            self.assertFalse((self.state / "forced-shutdown-incident").exists())

    @unittest.skipIf(sys.platform == "win32", "process-group kill is Unix-only (#2561)")
    def test_a_marker_written_by_a_non_child_pid_does_not_start_the_grace_clock(self) -> None:
        # #2594: a test or worker that wrote the real marker must not get the
        # healthy daemon killed. The marker names pid 1; the daemon is not pid 1.
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            daemon = self.make_daemon(
                directory,
                'echo started >> "$(dirname "$0")/starts"\n'
                'echo "pid=1 reason=sigterm" > "$RSID_SHUTDOWN_MARKER"\n'
                'sleep 5\nexit 0\n',
            )
            result = subprocess.run(
                [str(SUPERVISOR), str(daemon)], capture_output=True, text=True,
                timeout=30, check=False, env=self.backstop_env(grace=1),
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(len((directory / "starts").read_text().splitlines()), 1)
            self.assertNotIn("shutdown began", result.stderr)
            self.assertIn("ignoring shutdown marker", result.stderr)
            self.assertFalse((self.state / "shutdown-begun").exists())
            self.assertFalse((self.state / "forced-shutdown-incident").exists())

    @unittest.skipIf(sys.platform == "win32", "process-group kill is Unix-only (#2561)")
    def test_a_stale_marker_from_a_previous_daemon_is_cleared_at_launch(self) -> None:
        (self.state / "shutdown-begun").write_text("pid=1 reason=sigterm\n")
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            daemon = self.make_daemon(
                directory,
                'echo started >> "$(dirname "$0")/starts"\nsleep 3\nexit 0\n',
            )
            result = subprocess.run(
                [str(SUPERVISOR), str(daemon)], capture_output=True, text=True,
                timeout=30, check=False, env=self.backstop_env(grace=1),
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(len((directory / "starts").read_text().splitlines()), 1)
            self.assertFalse((self.state / "forced-shutdown-incident").exists())

    def test_the_daemon_environment_is_scrubbed_of_the_marker_variable(self) -> None:
        # #2594 guard: the shipped daemon source reads the variable once at the
        # top of main and removes it, so no spawn can inherit it.
        main = (SUPERVISOR.parents[1] / "crates/rsid/src/main.rs").read_text()
        body = main[main.index("fn main() -> Result<()>"):]
        self.assertLess(
            body.index("supervisor_shutdown::capture_marker_path()"),
            body.index("cli_action("),
        )
        shutdown = (SUPERVISOR.parents[1] / "crates/rsid/src/supervisor_shutdown.rs").read_text()
        self.assertIn("std::env::remove_var(SHUTDOWN_MARKER_ENV)", shutdown)
        # #2601: every RSID_SUPERVISOR_* variable the script touches is covered
        # by the daemon's prefix scrub.
        prefix = re.search(r'SUPERVISOR_INTERNAL_ENV_PREFIX: &str = "([A-Z_]+)"', shutdown).group(1)
        names = set(re.findall(r"\bRSID_SUPERVISOR_[A-Z_]+", SUPERVISOR.read_text()))
        self.assertIn("RSID_SUPERVISOR_PATH", names)
        for name in names:
            self.assertTrue(name.startswith(prefix), name)

    def test_the_service_stop_timeout_covers_the_supervisor_grace_and_term_wait(self) -> None:
        # #2590: systemd kills the unit at TimeoutStopSec; it must outlast the
        # largest operator grace plus the supervisor's SIGTERM wait.
        deploy = (SUPERVISOR.parents[1] / "crates/rsi-common/src/agent_deploy.rs").read_text()
        grace_max = int(re.search(r"SUPERVISOR_SHUTDOWN_GRACE_MAX_SECS: u64 = (\d+);", deploy).group(1))
        script = SUPERVISOR.read_text()
        term_wait = int(re.search(r"RSID_SUPERVISOR_TERM_WAIT_SECONDS:-(\d+)", script).group(1))
        unit = (SUPERVISOR.parent / "systemd/rsid.service").read_text()
        timeout = int(re.search(r"^TimeoutStopSec=(\d+)\s*$", unit, re.MULTILINE).group(1))
        self.assertGreater(timeout, grace_max + term_wait)
        # #2936: launchd's ExitTimeOut is the macOS agent's equivalent.
        plist = (SUPERVISOR.parent / "launchd/dev.rsi.rsid.plist.in").read_text()
        exit_timeout = int(re.search(r"<key>ExitTimeOut</key>\s*<integer>(\d+)</integer>", plist).group(1))
        self.assertGreater(exit_timeout, grace_max + term_wait)

    def test_every_array_expansion_is_legal_for_an_empty_array_on_bash_3_2(self) -> None:
        # #2936: macOS runs the supervisor with /bin/bash 3.2, where `set -u`
        # makes a bare "${a[@]}" or "${a[*]}" of an empty array a fatal unbound
        # error (the supervisor died at its first daemon exit). Each expansion
        # must be `${a[@]+"${a[@]}"}` or `${a[*]-}`; `${#a[@]}` is always legal.
        script = SUPERVISOR.read_text()
        guarded = re.sub(r'\$\{(\w+)\[@\]\+"\$\{\1\[@\]\}"\}', "", script)
        guarded = re.sub(r"\$\{\w+\[\*\]-\}", "", guarded)
        guarded = re.sub(r"\$\{#\w+\[[@*]\]\}", "", guarded)
        self.assertEqual(re.findall(r"\$\{\w+\[[@*]\][^}]*\}", guarded), [])

    @unittest.skipIf(sys.platform == "win32", "process-group kill is Unix-only (#2561)")
    def test_stopping_the_supervisor_ends_a_wedged_daemon_and_does_not_restart(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            daemon = self.make_daemon(
                directory,
                'dir="$(dirname "$0")"\n'
                'echo "$$" >> "$dir/starts"\n'
                'trap "" TERM\n'
                'touch "$dir/ready"\n'
                'while :; do sleep 1; done\n',
            )
            process = subprocess.Popen(
                [str(SUPERVISOR), str(daemon)], stdout=subprocess.PIPE,
                stderr=subprocess.PIPE, text=True, env=self.backstop_env(grace=2),
            )
            try:
                deadline = time.monotonic() + 5
                while not (directory / "ready").exists():
                    self.assertIsNone(process.poll(), "supervisor exited before child was ready")
                    self.assertLess(time.monotonic(), deadline, "child did not become ready")
                    time.sleep(0.01)
                process.send_signal(signal.SIGTERM)
                _, stderr = process.communicate(timeout=20)
            finally:
                if process.poll() is None:
                    process.kill()
                    process.communicate(timeout=5)
            self.assertEqual(process.returncode, 0, stderr)
            self.assertEqual(len((directory / "starts").read_text().splitlines()), 1)
            incident = (self.state / "forced-shutdown-incident").read_text()
            self.assertIn("trigger=supervisor_stop", incident)

    @unittest.skipIf(sys.platform == "win32", "process-group kill is Unix-only (#2561)")
    def test_wedged_shutdown_of_an_unverified_deploy_rolls_back_to_the_previous_binary(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            daemon = directory / "rsid"
            daemon.write_text(
                "#!/usr/bin/env bash\n"
                'dir="$(dirname "$0")"\n'
                'echo "$(basename "$0")" >> "$dir/starts"\n'
                'if grep -q "ROLE=[p]rev" "$0"; then exit 0; fi\n'
                'if [[ ! -e "$dir/first" ]]; then touch "$dir/first" "$dir/rsid.deploy-inflight"; exit 75; fi\n'
                'trap "" TERM\n'
                'echo "pid=$$ reason=sigterm" > "$RSID_SHUTDOWN_MARKER"\n'
                'while :; do sleep 1; done\n'
            )
            daemon.chmod(0o755)
            previous = directory / "rsid.prev"
            previous.write_text("#!/usr/bin/env bash\n# ROLE=prev\n"
                                'echo prev >> "$(dirname "$0")/starts"\nexit 0\n')
            previous.chmod(0o755)
            result = subprocess.run(
                [str(SUPERVISOR), str(daemon)], capture_output=True, text=True,
                timeout=40, check=False, env=self.backstop_env(grace=2),
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("restoring", result.stderr)
            self.assertEqual((directory / "starts").read_text().split(), ["rsid", "rsid", "prev"])
            # The wedged binary is kept as .failed and the previous one runs.
            self.assertIn("ROLE=prev", daemon.read_text())
            self.assertTrue((directory / "rsid.failed").exists())
            self.assertIn("trigger=daemon_shutdown", (self.state / "forced-shutdown-incident").read_text())

    # ---- #3433: a signal nobody announced is not an intended stop ------------

    # A stub rsid that receives a SIGINT: the marker the real daemon writes on a
    # signal (reason=ctrl_c, with the sender), then a clean exit 0. The second
    # run exits 0 with no marker so the supervisor finally stops.
    SIGNALLED = (
        'dir="$(dirname "$0")"\n'
        'echo "$$" >> "$dir/starts"\n'
        'if [[ -e "$dir/signalled-once" ]]; then exit 0; fi\n'
        'touch "$dir/signalled-once"\n'
        '%(announce)s'
        'echo "pid=$$ reason=%(reason)s sender_pid=4242" > "$RSID_SHUTDOWN_MARKER"\n'
        'exit 0\n'
    )

    def run_signalled(self, directory: Path, reason: str = "ctrl_c", announce: str = "") -> subprocess.CompletedProcess:
        daemon = self.make_daemon(
            directory, self.SIGNALLED % {"reason": reason, "announce": announce}
        )
        return subprocess.run(
            [str(SUPERVISOR), str(daemon)], capture_output=True, text=True,
            timeout=30, check=False, env=fast_poll_env(),
        )

    @unittest.skipIf(sys.platform == "win32", "signals are Unix-only")
    def test_an_unannounced_sigint_is_restarted_and_leaves_an_incident(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            result = self.run_signalled(directory)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(len((directory / "starts").read_text().splitlines()), 2)
            self.assertIn("unannounced SIGINT (sender pid 4242", result.stderr)
            incident = (self.state / "forced-shutdown-incident").read_text()
            self.assertIn("trigger=unannounced_signal", incident)
            self.assertIn("signal=INT", incident)
            first_pid = (directory / "starts").read_text().splitlines()[0]
            self.assertIn(f"pid={first_pid}", incident)

    @unittest.skipIf(sys.platform == "win32", "signals are Unix-only")
    def test_an_unannounced_sigterm_is_restarted_too(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            result = self.run_signalled(directory, reason="sigterm")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(len((directory / "starts").read_text().splitlines()), 2)
            self.assertIn("unannounced SIGTERM", result.stderr)

    @unittest.skipIf(sys.platform == "win32", "signals are Unix-only")
    def test_an_announced_stop_is_not_restarted(self) -> None:
        # install-release.sh appends `pid=<daemon pid> reason=...` before it
        # signals the daemon directly; that announcement makes the SIGINT intended.
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            announce = (
                '(umask 077; echo "pid=$$ reason=install_release" >> '
                f'"{self.state / "intended-stop"}")\n'
            )
            result = self.run_signalled(directory, announce=announce)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(len((directory / "starts").read_text().splitlines()), 1)
            self.assertNotIn("unannounced", result.stderr)
            self.assertFalse((self.state / "forced-shutdown-incident").exists())
            self.assertFalse((self.state / "intended-stop").exists(), "consumed once")

    @unittest.skipIf(sys.platform == "win32", "signals are Unix-only")
    def test_an_announcement_for_another_pid_does_not_excuse_the_stop(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            (self.state / "intended-stop").write_text("pid=1 reason=install_release\n")
            (self.state / "intended-stop").chmod(0o600)
            time.sleep(1.1)  # the file's mtime is older than the launch
            result = self.run_signalled(directory)
            self.assertEqual(len((directory / "starts").read_text().splitlines()), 2)
            self.assertIn("unannounced SIGINT", result.stderr)

    @unittest.skipIf(sys.platform == "win32", "signals are Unix-only")
    def test_an_operator_drain_exit_is_not_a_signal_stop(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            result = self.run_signalled(directory, reason="drain_restart")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(len((directory / "starts").read_text().splitlines()), 1)
            self.assertFalse((self.state / "forced-shutdown-incident").exists())

    @unittest.skipIf(sys.platform == "win32", "signals are Unix-only")
    def test_a_stop_that_also_signals_the_supervisor_is_not_restarted(self) -> None:
        # `systemctl stop` signals the whole unit: the daemon can exit before the
        # supervisor's own SIGTERM lands. The settle window waits for it.
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            daemon = self.make_daemon(
                directory,
                'dir="$(dirname "$0")"\n'
                'echo "$$" >> "$dir/starts"\n'
                'echo "pid=$$ reason=sigterm sender_pid=1" > "$RSID_SHUTDOWN_MARKER"\n'
                'exit 0\n',
            )
            env = fast_poll_env()
            env["RSID_SUPERVISOR_UNANNOUNCED_SETTLE_SECONDS"] = "5"
            process = subprocess.Popen(
                [str(SUPERVISOR), str(daemon)], stdout=subprocess.PIPE,
                stderr=subprocess.PIPE, text=True, env=env,
            )
            try:
                deadline = time.monotonic() + 5
                while not (directory / "starts").exists():
                    self.assertIsNone(process.poll(), "supervisor exited before the daemon ran")
                    self.assertLess(time.monotonic(), deadline, "daemon did not start")
                    time.sleep(0.01)
                time.sleep(0.5)
                process.send_signal(signal.SIGTERM)
                _, stderr = process.communicate(timeout=10)
            finally:
                if process.poll() is None:
                    process.kill()
                    process.communicate(timeout=5)
            self.assertEqual(process.returncode, 0, stderr)
            self.assertEqual(len((directory / "starts").read_text().splitlines()), 1)
            self.assertNotIn("restarting it", stderr)
            self.assertFalse((self.state / "forced-shutdown-incident").exists())

    @unittest.skipIf(sys.platform == "win32", "signals are Unix-only")
    def test_repeated_unannounced_signals_wait_out_the_restart_budget_with_an_incident(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            # One deploy restart per hour plus the reserve: two restarts allowed.
            Path(os.environ["RSID_SUPERVISOR_RESTART_BUDGET_FILE"]).write_text("1\n120\n")
            daemon = self.make_daemon(
                directory,
                'dir="$(dirname "$0")"\n'
                'echo "$$" >> "$dir/starts"\n'
                'echo "pid=$$ reason=ctrl_c sender_pid=9" > "$RSID_SHUTDOWN_MARKER"\n'
                'exit 0\n',
            )
            returncode, stderr, _ = self.run_until_exhausted(
                [str(SUPERVISOR), str(daemon)], env=fast_poll_env()
            )
            self.assertEqual(returncode, 0, stderr)
            self.assertEqual(len((directory / "starts").read_text().splitlines()), 3)
            self.assertIn("restart budget exhausted", stderr)
            incident = (self.state / "forced-shutdown-incident").read_text()
            self.assertIn("trigger=restart_budget_exhausted", incident)

    # ---- #3492: one restart budget for crashes, watchdog exits and deploys ----

    # A stub rsid whose Nth run behaves as line N of `kinds`: `deploy` writes a
    # fresh deploy marker then exits 75, `crash` clears the marker and exits 75,
    # `stop` exits 0.
    SEQUENCED = (
        'dir="$(dirname "$0")"\n'
        'n=$(cat "$dir/n" 2>/dev/null || echo 0); n=$((n + 1)); echo "$n" > "$dir/n"\n'
        'kind=$(sed -n "${n}p" "$dir/kinds")\n'
        'case $kind in\n'
        '  deploy) echo "database=/nonexistent" > "$dir/fake-rsid.deploy-inflight"; exit 75 ;;\n'
        '  crash) rm -f "$dir/fake-rsid.deploy-inflight"; exit 75 ;;\n'
        '  *) exit 0 ;;\n'
        'esac\n'
    )

    def sequenced_run(self, directory: Path, kinds: list[str], until_exhausted: bool):
        (directory / "kinds").write_text("\n".join(kinds) + "\n")
        daemon = self.make_daemon(directory, self.SEQUENCED)
        # Budget 2 deploys per hour: the supervisor allows 3 restarts per window.
        Path(os.environ["RSID_SUPERVISOR_RESTART_BUDGET_FILE"]).write_text("2\n")
        fake_bin = directory / "fakebin"
        fake_bin.mkdir()
        fake_sleep = fake_bin / "sleep"
        fake_sleep.write_text("#!/bin/sh\nexit 0\n")
        fake_sleep.chmod(0o755)
        env = fast_poll_env()
        env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"
        if until_exhausted:
            return self.run_until_exhausted([str(SUPERVISOR), str(daemon)], env=env, timeout=10)
        result = subprocess.run(
            [str(SUPERVISOR), str(daemon)], capture_output=True, text=True,
            timeout=15, check=False, env=env,
        )
        return result.returncode, result.stderr, 0.0

    def published(self) -> dict:
        text = (self.state / "supervisor-restarts").read_text()
        values: dict = {"restart": []}
        for line in text.splitlines():
            key, _, value = line.partition("=")
            if key == "restart":
                values["restart"].append(int(value))
            else:
                values[key] = int(value)
        return values

    def test_a_crash_then_a_deploy_spend_one_budget_and_are_published(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            returncode, stderr, _ = self.sequenced_run(directory, ["crash", "deploy", "stop"], False)
            self.assertEqual(returncode, 0, stderr)
            self.assertIn("watchdog restart 1/3", stderr)
            self.assertIn("deploy restart 2/3", stderr)
            published = self.published()
            # The crash and the deploy both sit in the history the daemon reads.
            self.assertEqual((published["limit"], published["window"]), (3, 3600))
            self.assertEqual(len(published["restart"]), 2, published)
            self.assertNotIn("backoff_until", published)

    def test_deploys_then_a_crash_use_the_last_slot_and_the_next_crash_backs_off(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            returncode, stderr, _ = self.sequenced_run(
                directory, ["deploy", "deploy", "crash", "crash"], True
            )
            self.assertEqual(returncode, 0, stderr)
            # Two deploys and one crash took all three restarts; the fourth exit
            # waits (the supervisor stays up) instead of leaving the host with no rsid.
            self.assertIn("deploy restart 1/3", stderr)
            self.assertIn("deploy restart 2/3", stderr)
            self.assertIn("watchdog restart 3/3", stderr)
            self.assertIn("not exiting", stderr)
            self.assertEqual((directory / "n").read_text().strip(), "4")
            published = self.published()
            self.assertEqual(len(published["restart"]), 3, published)
            # The wait is published for deploy admission and the TUI.
            self.assertGreater(published["backoff_until"], int(time.time()))
            incident = (self.state / "forced-shutdown-incident").read_text()
            self.assertIn("trigger=restart_budget_exhausted", incident)

    def test_an_exhausted_budget_is_waited_out_and_the_daemon_restarts(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            daemon = self.make_daemon(
                directory,
                'dir="$(dirname "$0")"\n'
                'echo started >> "$dir/starts"\n'
                'if [[ $(wc -l < "$dir/starts") -ge 2 ]]; then exit 0; fi\n'
                'exit 75\n',
            )
            env = fast_poll_env()
            now = int(time.time())
            # Three restarts already inside the window: the budget is spent.
            env["RSID_SUPERVISOR_RESTART_TIMES"] = f"{now - 30} {now - 20} {now - 10}"
            env["RSID_SUPERVISOR_EXHAUSTED_BACKOFF_MAX_SECONDS"] = "2"
            began = time.monotonic()
            result = subprocess.run(
                [str(SUPERVISOR), str(daemon)], capture_output=True, text=True,
                timeout=20, check=False, env=env,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(len((directory / "starts").read_text().splitlines()), 2)
            self.assertIn("restarting the daemon in 2s", result.stderr)
            self.assertGreaterEqual(time.monotonic() - began, 2.0)

    def test_stopping_the_supervisor_ends_an_exhausted_backoff(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            daemon = self.make_daemon(directory, 'echo started >> "$(dirname "$0")/starts"\nexit 75\n')
            env = fast_poll_env()
            now = int(time.time())
            env["RSID_SUPERVISOR_RESTART_TIMES"] = f"{now - 30} {now - 20} {now - 10}"
            env["RSID_SUPERVISOR_EXHAUSTED_BACKOFF_MAX_SECONDS"] = "900"
            began = time.monotonic()
            returncode, stderr, _ = self.run_until_exhausted([str(SUPERVISOR), str(daemon)], env=env)
            self.assertEqual(returncode, 0, stderr)
            self.assertLess(time.monotonic() - began, 10)
            self.assertEqual(len((directory / "starts").read_text().splitlines()), 1)

    # ---- #3507: the intended-stop announcement must be owner-private ---------

    def announced_restart(self, directory: Path, prepare: str, env=None):
        announce = prepare + f'echo "pid=$$ reason=install_release" >> "{self.state / "intended-stop"}"\n'
        daemon = self.make_daemon(
            directory, self.SIGNALLED % {"reason": "ctrl_c", "announce": announce}
        )
        return subprocess.run(
            [str(SUPERVISOR), str(daemon)], capture_output=True, text=True,
            timeout=30, check=False, env=env or fast_poll_env(),
        )

    @unittest.skipIf(sys.platform == "win32", "signals are Unix-only")
    def test_a_loose_mode_announcement_is_ignored_and_the_restart_happens(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            result = self.announced_restart(directory, "umask 022\n")
            self.assertEqual(len((directory / "starts").read_text().splitlines()), 2, result.stderr)
            self.assertIn("looser than 0600", result.stderr)
            self.assertIn("unannounced SIGINT", result.stderr)

    @unittest.skipIf(sys.platform == "win32", "signals are Unix-only")
    def test_a_symlinked_announcement_is_ignored_and_the_restart_happens(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            target = directory / "real-announcement"
            target.write_text("")
            target.chmod(0o600)
            (self.state / "intended-stop").symlink_to(target)
            result = self.announced_restart(directory, "umask 077\n")
            # The announcement is real and well-formed behind the link; the link itself is refused.
            self.assertIn("pid=", target.read_text())
            self.assertEqual(len((directory / "starts").read_text().splitlines()), 2, result.stderr)
            self.assertIn("not a regular file", result.stderr)
            self.assertIn("unannounced SIGINT", result.stderr)

    @unittest.skipIf(sys.platform == "win32", "signals are Unix-only")
    @unittest.skipUnless(hasattr(os, "geteuid") and os.geteuid() == 0, "a foreign owner needs root to create")
    def test_a_foreign_owned_announcement_is_ignored_and_the_restart_happens(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            daemon_prepare = f'chown 12345 "{self.state / "intended-stop"}" 2>/dev/null || true\n'
            announce = (
                '(umask 077; echo "pid=$$ reason=install_release" >> '
                f'"{self.state / "intended-stop"}")\n' + daemon_prepare
            )
            daemon = self.make_daemon(
                directory, self.SIGNALLED % {"reason": "ctrl_c", "announce": announce}
            )
            result = subprocess.run(
                [str(SUPERVISOR), str(daemon)], capture_output=True, text=True,
                timeout=30, check=False, env=fast_poll_env(),
            )
            self.assertEqual(len((directory / "starts").read_text().splitlines()), 2, result.stderr)
            self.assertIn("not owned by this user", result.stderr)

    @unittest.skipIf(sys.platform == "win32", "signals are Unix-only")
    def test_an_owner_private_announcement_in_the_runtime_dir_is_honoured_without_home(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            runtime = directory / "runtime"
            runtime.mkdir(mode=0o700)
            announcement = runtime / "rsi" / "intended-stop"
            env = fast_poll_env()
            for key in ("HOME", "RSID_SUPERVISOR_INTENDED_STOP_FILE"):
                env.pop(key, None)
            env["XDG_RUNTIME_DIR"] = str(runtime)
            prepare = f'umask 077\nmkdir -p "{announcement.parent}"\n'
            announce = prepare + f'echo "pid=$$ reason=install_release" >> "{announcement}"\n'
            daemon = self.make_daemon(
                directory, self.SIGNALLED % {"reason": "ctrl_c", "announce": announce}
            )
            result = subprocess.run(
                [str(SUPERVISOR), str(daemon)], capture_output=True, text=True,
                timeout=30, check=False, env=env,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(len((directory / "starts").read_text().splitlines()), 1, result.stderr)
            self.assertNotIn("unannounced", result.stderr)

    def test_no_shared_tmp_path_is_ever_the_announcement_location(self) -> None:
        self.assertNotIn("/tmp/rsi-intended-stop", SUPERVISOR.read_text())

    def test_snapshot_without_a_grace_line_keeps_the_default(self) -> None:
        # A snapshot written before #2561 has one line; the supervisor must read
        # it, and an invalid second line must fall back to the default grace.
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            daemon = self.make_daemon(directory, 'exit 0\n')
            for content in ("2\n", "2\nnot-a-number\n"):
                Path(os.environ["RSID_SUPERVISOR_RESTART_BUDGET_FILE"]).write_text(content)
                result = subprocess.run(
                    [str(SUPERVISOR), str(daemon)], capture_output=True, text=True,
                    timeout=10, check=False,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertNotIn("invalid deploy restart budget", result.stderr)

    def test_unsupported_platform_note_names_the_missing_process_group_kill(self) -> None:
        # The Windows note is in the script (no Windows lane yet, #1229).
        script = SUPERVISOR.read_text()
        self.assertIn("MINGW* | MSYS* | CYGWIN*) shutdown_backstop=0", script)
        self.assertIn("backstop is unsupported on this platform", script)


class InheritedHandoffTests(unittest.TestCase):
    """A daemon-managed shell inherits the live supervisor's handoff (#230).

    The hostile values are scratch sentinels for the supervisor-internal keys
    only; no authentication or custody variable is touched.
    """

    def hostile(self, scratch: Path) -> dict[str, str]:
        decoy = scratch / "installed-supervisor.sh"
        decoy.write_text(f"#!/usr/bin/env bash\ntouch {shlex.quote(str(scratch / 'decoy-ran'))}\n")
        decoy.chmod(0o755)
        return {
            "RSID_SUPERVISOR_PATH": str(decoy),
            "RSID_SUPERVISOR_SNAPSHOT": str(scratch / "no-such-snapshot"),
            "RSID_SUPERVISOR_RESTART_TIMES": "1 1 1 1 1 1 1 1",
            "RSID_SUPERVISOR_AFTER_RESTART": "1",
            "RSID_SUPERVISOR_LAST_FALLBACK": "hostile-sentinel",
            "RSID_SUPERVISOR_FAST_FAILURE_SECONDS": "0",
            "RSID_SUPERVISOR_POLL_SECONDS": "99",
            "RSID_SUPERVISOR_TERM_WAIT_SECONDS": "99",
            "RSID_SUPERVISOR_SHUTDOWN_MARKER": str(scratch / "hostile-marker"),
            "RSID_SUPERVISOR_INCIDENT_FILE": str(scratch / "hostile-incident"),
            "RSID_SUPERVISOR_RESTART_BUDGET_FILE": str(scratch / "hostile-budget"),
            "RSI_LISTEN_FD": "99",
            "LISTEN_PID": "1",
            "LISTEN_FDS": "7",
            "LISTEN_FDNAMES": "hostile",
        }

    def test_helper_removes_exactly_the_handoff_keys(self) -> None:
        kept = {"HOME": "/h", "PATH": "/bin", "RSI_SESSION_ID": "s", "RSI_SOCKET": "x",
                "RSID_SHUTDOWN_MARKER": "m", "LISTEN_ELSEWHERE": "1"}
        with tempfile.TemporaryDirectory() as root:
            environ = {**kept, **self.hostile(Path(root))}
            self.assertEqual(without_supervisor_handoff(environ), kept)

    def test_fixture_setup_scrubs_inherited_handoff_and_restores_it(self) -> None:
        seen: dict[str, object] = {}

        class Probe(SupervisorTests):
            def runTest(self) -> None:
                seen["handoff"] = sorted(key for key in os.environ if is_supervisor_handoff_key(key))

        with tempfile.TemporaryDirectory() as root:
            hostile = self.hostile(Path(root))
            with mock.patch.dict(os.environ, hostile):
                before = dict(os.environ)
                result = unittest.TestResult()
                Probe().run(result)
                self.assertTrue(result.wasSuccessful(), result.errors + result.failures)
                self.assertEqual(dict(os.environ), before)
            self.assertEqual(seen["handoff"], [
                "RSID_SUPERVISOR_INCIDENT_FILE",
                "RSID_SUPERVISOR_INTENDED_STOP_FILE",
                "RSID_SUPERVISOR_RESTART_BUDGET_FILE",
                "RSID_SUPERVISOR_SHUTDOWN_MARKER",
                "RSID_SUPERVISOR_UNANNOUNCED_SETTLE_SECONDS",
            ])

    def test_refresh_and_fallback_assertions_pass_under_hostile_inherited_handoff(self) -> None:
        names = [
            "test_reexec_preserves_pid_and_restart_budget_across_every_refresh",
            "test_refresh_preserves_fast_failure_fallback_state",
            "test_refresh_preserves_the_single_fallback_limit",
            "test_syntax_invalid_refresh_keeps_the_running_supervisor",
        ]
        with tempfile.TemporaryDirectory() as root:
            scratch = Path(root)
            suite = unittest.TestSuite(SupervisorTests(name) for name in names)
            result = unittest.TestResult()
            with mock.patch.dict(os.environ, self.hostile(scratch)):
                suite.run(result)
            self.assertEqual(result.testsRun, len(names))
            self.assertEqual((result.errors, result.failures, result.skipped), ([], [], []))
            # The installed-script decoy must never have been refreshed into.
            self.assertFalse((scratch / "decoy-ran").exists())
            self.assertFalse((scratch / "hostile-marker").exists())
            self.assertFalse((scratch / "hostile-incident").exists())


if __name__ == "__main__":
    unittest.main()
