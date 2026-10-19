"""#1862: `install-release.sh --now` starts rsid when none is running and exits
nonzero when no healthy daemon answers afterwards.

Hermetic: HOME is a temp dir, the build is a stub, and `pgrep`, `uname`,
`systemctl`, `systemd-run` and `launchctl` are fakes first on PATH, so the live
daemon, its holder unit or launchd agent and its supervisor are never seen,
started or signalled (also when this suite runs on macOS, #2936). The fake
holder passes a temp socket listener to a fake daemon child; readiness checks
the sender of that child's GetHealthStatus reply.

Child ownership (#232): the fake launchers detach from the installer, so the
fixture cannot `wait` on them. Instead the fixture owns a lifeline pipe: it
keeps the read end, and the write end is inherited by every fake launcher and
daemon (the fakes pass it on explicitly). End of file on the read end, after
the fixture closes its own write end, proves every fixture-owned process has
exited, including a child that has not yet published a PID marker. Cleanup
asks the fake holder to stop; the holder owns the daemon child and stops and
reaps it, so the parent-child shutdown order is preserved. The fixture relies
on POSIX pipes, `pass_fds` and Unix sockets, like the rest of this module.
"""

import errno
import os
import plistlib
import select
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parents[1]
RELEASE_BINS = [
    "rsi",
    "rsid",
    "rsi-agent-mcp",
    "rsi-build-rustc",
    "rsi-contract-validate",
    "rsi-turn-shim",
]


def write_executable(path, text):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)
    path.chmod(0o755)


class StartFixture(unittest.TestCase):
    """Shared hermetic fixture; carries no tests of its own."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="rsi-install-start-")
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name)
        self.home = self.base / "home"
        self.root = self.base / "repo"
        self.target = self.base / "target"
        self.state = self.base / "state"
        self.fakes = self.base / "fakes"
        self.state.mkdir()
        # The daemon socket gets an absolute path short enough for AF_UNIX
        # (104 bytes on macOS): the launchd plist carries it verbatim (#2936),
        # and a sandbox TMPDIR is too deep for it.
        socket_dir = tempfile.TemporaryDirectory(prefix="rsi-sock-", dir="/tmp")
        self.addCleanup(socket_dir.cleanup)
        self.socket = Path(socket_dir.name) / "daemon.sock"
        self.lifeline_r, self.lifeline_w = os.pipe()
        shutil.copytree(SCRIPTS, self.root / "scripts", ignore=shutil.ignore_patterns("tests"))
        (self.root / "Cargo.toml").write_text(
            '[workspace]\n[package]\nname = "stub"\nversion = "0.0.0"\nedition = "2021"\n'
        )
        write_executable(self.root / "src" / "lib.rs", "")
        for name in RELEASE_BINS:
            write_executable(self.target / "release" / name, '#!/bin/sh\necho "fake-%s $*"\n' % name)
        # #938: --link-only links only what this checkout built.
        (self.target / "release" / ".rsi-release-source").write_text(
            f"worktree={self.root.resolve()}\nstate=built\nhead=unknown\n"
        )
        # Fake build tools keep a fresh HOME offline, even for metadata.
        write_executable(
            self.fakes / "cargo",
            "#!/bin/sh\nprintf '{\"target_directory\":\"%s\"}\\n' \"$CARGO_TARGET_DIR\"\n",
        )
        write_executable(self.fakes / "rustup", '#!/bin/sh\necho "unexpected rustup" >&2\nexit 99\n')
        self.addCleanup(self.stop_launchers)
        # A real temp Unix socket verifies kernel PID credentials, with a fake
        # daemon below the fake holder. No installed daemon or unit is used.
        self.server = self.base / "server.py"
        self.server.write_text("""
import json, os, socket, sys
from pathlib import Path
state = Path(os.environ["FAKE_STATE"])
gate = os.environ.get("FAKE_SERVER_GATE")
if gate:
    # Test hook: hold startup, before any PID marker, until the fixture writes.
    with open(gate, "rb") as opened:
        opened.read(1)
(state / ("daemon-" + str(os.getpid()))).write_text(str(os.getpid()))
inherited = os.environ.get("FAKE_LISTEN_FD")
listener = socket.socket(fileno=int(inherited)) if inherited else socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
with listener:
    if not inherited:
        listener.bind(sys.argv[1])
        listener.listen()
    while True:
        conn, _ = listener.accept()
        with conn:
            request = conn.recv(65536)
            with (state / "rpc").open("a") as log:
                log.write(request.decode())
            result = {} if os.environ.get("FAKE_NEVER_HEALTHY") else {"persistence_queue_capacity": 1, "project_cache_size": 0}
            if result and not os.environ.get("FAKE_LEGACY_HEALTH"):
                result.update(daemon_pid=os.getpid(), daemon_started_at="2026-10-12T00:00:00.000000000Z")
                if os.environ.get("FAKE_WRONG_PID"):
                    result["daemon_pid"] = os.getppid()
                if os.environ.get("FAKE_BAD_START"):
                    result["daemon_started_at"] = "invalid"
            reply = json.dumps({"jsonrpc": "2.0", "id": 1, "result": result}).encode() + b"\\n"
            try:
                conn.sendall(reply)
                conn.recv(1)
            except BrokenPipeError:
                pass
""")
        write_executable(
            self.target / "release" / "rsi-socket-hold",
            f"#!{sys.executable}\n" + """
import os, signal, socket, subprocess, sys
from pathlib import Path
state = Path(os.environ["FAKE_STATE"])
child = None
stopping = False
def stop(signum, frame):
    # The holder owns the daemon child: stop it, then exit once it is reaped.
    global stopping
    stopping = True
    if child is not None:
        child.terminate()
exits_or_idles = os.environ.get("FAKE_HOLDER_EXIT") or os.environ.get("FAKE_HOLDER_IDLE")
if not exits_or_idles:
    signal.signal(signal.SIGTERM, stop)  # before the PID is published
(state / "started").write_text(" ".join(sys.argv[1:]))
(state / "holder").write_text(str(os.getpid()))
if os.environ.get("FAKE_HOLDER_EXIT"):
    sys.exit(1)
if os.environ.get("FAKE_HOLDER_IDLE"):
    import time
    time.sleep(15)
else:
    # Like the real holder, create the listener here and pass it to the child,
    # and remove its socket path on the way out.
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener:
        listener.bind(sys.argv[1])
        try:
            listener.listen()
            env = dict(os.environ, FAKE_LISTEN_FD=str(listener.fileno()))
            lifeline = int(os.environ["FAKE_LIFELINE_FD"])
            child = subprocess.Popen([sys.executable, os.environ["FAKE_SERVER"], sys.argv[1]], env=env, pass_fds=(listener.fileno(), lifeline))
            if stopping:
                child.terminate()
            status = child.wait()
        finally:
            os.unlink(sys.argv[1])
        sys.exit(status)
""",
        )
        write_executable(self.target / "release" / "rsi-rpc", "#!/bin/sh\nexit 99\n")
        # No live rsid is ever visible; the platform and systemd are faked.
        write_executable(self.fakes / "pgrep", "#!/bin/sh\nexit 1\n")
        write_executable(
            self.fakes / "uname",
            '#!/bin/sh\n[ "$1" = "-s" ] && echo "$FAKE_UNAME" || exec /usr/bin/uname "$@"\n',
        )
        write_executable(
            self.fakes / "systemctl",
            "#!/bin/sh\n"
            'echo "$@" >>"$FAKE_STATE/systemctl"\n'
            'case "$2" in\n'
            '  cat) [ "$FAKE_MANAGED" = 1 ]; exit $? ;;\n'
            '  restart) "$FAKE_RUN" -- "$HOME/.rsi/install/rsi-socket-hold" "$RSI_DAEMON_SOCKET_PATH" ;;\n'
            '  start) [ "$3" = rsid.service ] && "$FAKE_RUN" -- "$HOME/.rsi/install/rsi-socket-hold" "$RSI_DAEMON_SOCKET_PATH" ;;\n'
            '  is-active) exit 0 ;;\n'
            "  show)\n"
            '    case "$*" in\n'
            '      *--property=MainPID*) cat "$FAKE_STATE/holder"; exit ;;\n'
            '      *--property=ActiveState*--value) echo "$FAKE_UNIT_STATE"; exit ;;\n'
            '    esac\n'
            '    echo ActiveState=active\n'
            '    echo "ControlGroup=/user.slice/x/$3"\n'
            "    echo MemoryHigh=6442450944\n    echo MemoryMax=8589934592\n"
            "    echo MemorySwapMax=0\n    echo CPUWeight=20 ;;\n"
            "esac\n"
            "exit 0\n",
        )
        write_executable(
            self.fakes / "systemd-run",
            f"#!{sys.executable}\n" + """
import os, subprocess, sys, time
from pathlib import Path
state = Path(os.environ["FAKE_STATE"])
(state / "systemd-run").write_text(" ".join(sys.argv[1:]))
lifeline = int(os.environ["FAKE_LIFELINE_FD"])
with open(os.devnull, "w") as output:
    # Detached like a real unit; the lifeline fd lets the fixture prove it ended.
    subprocess.Popen(sys.argv[sys.argv.index("--") + 1:], stdin=subprocess.DEVNULL, stdout=output, stderr=output, pass_fds=(lifeline,))
for _ in range(100):
    if (state / "holder").exists():
        break
    time.sleep(0.01)
else:
    sys.exit(1)
""",
        )

        # #2936: a fake launchd for the gui domain. It runs the loaded plist's
        # ProgramArguments detached (its EnvironmentVariables, WorkingDirectory
        # and log), reports `pid = N` only while that process lives, and, like
        # launchd, returns from bootout at once while the job keeps showing until
        # its process has exited. No real launchd job is ever touched.
        write_executable(
            self.fakes / "launchctl",
            f"#!{sys.executable}\n" + """
import os, plistlib, signal, subprocess, sys
from pathlib import Path
state = Path(os.environ["FAKE_STATE"])
jobs = state / "launchd"
jobs.mkdir(exist_ok=True)
with (state / "launchctl").open("a") as log:
    log.write(" ".join(sys.argv[1:]) + "\\n")
args = sys.argv[1:]
verb = args[0] if args else ""
loaded, stopping, pid_file = jobs / "loaded", jobs / "stopping", jobs / "pid"
def alive():
    try:
        pid = int(pid_file.read_text())
        os.kill(pid, 0)
    except (OSError, ValueError):
        return None
    return pid
def spawn():
    job = plistlib.loads(loaded.read_bytes())
    env = dict(os.environ, **job["EnvironmentVariables"])
    with open(job["StandardOutPath"], "a") as output:
        child = subprocess.Popen(
            job["ProgramArguments"], cwd=job["WorkingDirectory"], env=env,
            stdin=subprocess.DEVNULL, stdout=output, stderr=output,
            pass_fds=(int(os.environ["FAKE_LIFELINE_FD"]),), start_new_session=True,
        )
    pid_file.write_text(str(child.pid))
if stopping.exists() and not alive():
    stopping.unlink()
if verb == "print":
    if not (loaded.exists() or stopping.exists()):
        sys.exit(113)
    pid = alive()
    print("gui/0/dev.rsi.rsid = {")
    print("\\tstate = " + ("running" if pid else "not running"))
    if pid:
        print(f"\\tpid = {pid}")
    print("}")
elif verb == "bootstrap":
    if loaded.exists() or stopping.exists():
        sys.exit(5)
    loaded.write_bytes(Path(args[2]).read_bytes())
    spawn()
elif verb == "bootout":
    if loaded.exists():
        loaded.unlink()
        if alive():
            stopping.touch()
            os.kill(alive(), signal.SIGTERM)
elif verb == "kickstart":
    if not loaded.exists():
        sys.exit(113)
    if not alive():
        spawn()
sys.exit(0)
""",
        )

    def lifeline_closed(self, timeout):
        # End of file means no process still holds the lifeline write end.
        ready, _, _ = select.select([self.lifeline_r], [], [], timeout)
        return bool(ready) and os.read(self.lifeline_r, 1) == b""

    def holder_pid(self):
        # The PID published by this fixture's fake holder; a half-written file
        # is read again on the next tick.
        try:
            return int((self.state / "holder").read_text())
        except (OSError, ValueError):
            return None

    @staticmethod
    def signal_holder(pid, sig):
        try:
            os.kill(pid, sig)
        except ProcessLookupError:
            pass

    def stop_launchers(self):
        """Stop and reap every fake launcher and daemon this fixture started.

        Idempotent. Returns only once the lifeline reports every owner gone, so
        no fixture process can still write into the temp directory.
        """
        if self.lifeline_w is None:
            return
        os.close(self.lifeline_w)
        self.lifeline_w = None
        try:
            signalled = set()
            deadline = time.monotonic() + 5
            while not self.lifeline_closed(0.02):
                # The holder stops and reaps its own daemon child. A holder
                # still starting is asked as soon as it publishes its PID.
                pid = self.holder_pid()
                if pid is not None and pid not in signalled:
                    signalled.add(pid)
                    self.signal_holder(pid, signal.SIGTERM)
                if time.monotonic() > deadline:
                    break
            else:
                return
            # Last resort for a wedged fake holder.
            if self.holder_pid() is not None:
                self.signal_holder(self.holder_pid(), signal.SIGKILL)
            if not self.lifeline_closed(5):
                self.fail("fixture launchers outlived cleanup; a child never published its PID")
        finally:
            os.close(self.lifeline_r)

    def start_foreign_daemon(self):
        foreign = subprocess.Popen(
            [sys.executable, str(self.server), str(self.socket)],
            cwd=self.base,
            env=dict(os.environ, FAKE_STATE=str(self.state)),
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        self.addCleanup(foreign.wait, 5)
        self.addCleanup(foreign.terminate)
        deadline = time.monotonic() + 3
        while not self.socket.exists():
            if foreign.poll() is not None or time.monotonic() > deadline:
                self.fail("foreign fixture failed to bind")
            time.sleep(0.01)

    def install(self, *flags, uname="Darwin", never_healthy=False, holder_exit=False, holder_idle=False, managed=False, unit_state="active", legacy_health=False, wrong_pid=False, bad_start=False, server_gate=None, script="install-release.sh", running=False, extra_env=None):
        # A run that must come up healthy gets a bound that survives a busy host (#140: the fixed 2 s
        # failed at random on the macOS satellite while other suites ran); a run that expects the wait
        # to give up keeps it short so the test stays fast.
        expects_failure = never_healthy or holder_exit or holder_idle or wrong_pid or bad_start or unit_state != "active" or (extra_env or {}).get("FAKE_LOCAL_PEERPID", "daemon") != "daemon"
        env = dict(
            os.environ,
            HOME=str(self.home),
            CARGO_TARGET_DIR=str(self.target),
            PATH=f"{self.fakes}:{os.environ['PATH']}",
            FAKE_STATE=str(self.state),
            FAKE_SERVER=str(self.server),
            FAKE_RUN=str(self.fakes / "systemd-run"),
            FAKE_MANAGED="1" if managed else "0",
            FAKE_UNIT_STATE=unit_state,
            FAKE_HOLDER_EXIT="1" if holder_exit else "",
            FAKE_HOLDER_IDLE="1" if holder_idle else "",
            FAKE_NEVER_HEALTHY="1" if never_healthy else "",
            FAKE_LEGACY_HEALTH="1" if legacy_health else "",
            FAKE_WRONG_PID="1" if wrong_pid else "",
            FAKE_BAD_START="1" if bad_start else "",
            FAKE_LIFELINE_FD=str(self.lifeline_w),
            FAKE_SERVER_GATE=str(server_gate or ""),
            FAKE_UNAME=uname,
            RSI_INSTALL_HEALTH_WAIT_SECS="2" if expects_failure else "20",
            RSI_DAEMON_SOCKET_PATH=str(self.socket),
        )
        env.update(extra_env or {})
        for name in ("RSI_INSTALL_BIN_DIR", "RSI_SOCKET", "NOW"):
            env.pop(name, None)
        if running:
            # The test process itself is a live PID that stands in for rsid.
            write_executable(self.fakes / "pgrep", "#!/bin/sh\necho %d\n" % os.getpid())
        return subprocess.run(
            ["bash", str(self.root / "scripts" / script), *flags],
            env=env,
            cwd=self.base,
            text=True,
            capture_output=True,
            timeout=10,
            pass_fds=(self.lifeline_w,),
        )


class InstallReleaseStartTest(StartFixture):
    def test_now_link_only_starts_a_down_daemon_and_waits_for_health(self):
        result = self.install("--link-only", "--now")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("starting it", result.stdout)
        self.assertIn("rsid healthy", result.stdout)
        started = (self.state / "started").read_text()
        self.assertIn("daemon.sock", started)
        self.assertIn("rsid-supervisor.sh", started)
        self.assertIn("GetHealthStatus", (self.state / "rpc").read_text())

    def test_no_daemon_after_start_exits_nonzero_with_a_clear_message(self):
        result = self.install("--link-only", "--now", never_healthy=True)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("NO DAEMON IS RUNNING", result.stderr)
        self.assertIn("GetHealthStatus", result.stderr)

    def test_no_restart_still_skips_the_start(self):
        result = self.install("--link-only", "--no-restart")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertFalse((self.state / "started").exists())

    @unittest.skipUnless(sys.platform.startswith("linux"), "systemd launcher is Linux-only")
    def test_linux_starts_a_down_daemon_in_a_collected_unit(self):
        result = self.install("--link-only", "--now", uname="Linux")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("starting it", result.stdout)
        run = (self.state / "systemd-run").read_text()
        self.assertIn("--collect", run)
        self.assertIn("rsi-socket-hold", run)
        self.assertTrue((self.state / "started").exists())

    @unittest.skipUnless(sys.platform.startswith("linux"), "systemd launcher is Linux-only")
    def test_linux_unhealthy_start_exits_nonzero(self):
        result = self.install("--link-only", "--now", uname="Linux", never_healthy=True)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("NO DAEMON IS RUNNING", result.stderr)

    def test_foreign_daemon_cannot_mask_an_exited_holder(self):
        for platform, managed in (("Darwin", False), ("Linux", False), ("Linux", True)):
            if platform == "Linux" and not sys.platform.startswith("linux"):
                continue
            with self.subTest(platform=platform, managed=managed):
                # Each iteration owns a separate socket and fake launcher.
                if not self.socket.exists():
                    self.start_foreign_daemon()
                result = self.install("--link-only", "--now", uname=platform, holder_exit=True, managed=managed)
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertIn("GetHealthStatus", result.stderr)

    def test_foreign_daemon_cannot_mask_a_live_unrelated_holder(self):
        self.start_foreign_daemon()
        result = self.install("--link-only", "--now", holder_idle=True)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("NO DAEMON IS RUNNING from this launch", result.stderr)

    @unittest.skipUnless(sys.platform.startswith("linux"), "systemd launcher is Linux-only")
    def test_managed_service_identifies_its_daemon(self):
        result = self.install("--link-only", "--now", uname="Linux", managed=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("rsid healthy", result.stdout)

    @unittest.skipUnless(sys.platform.startswith("linux"), "kernel fallback is Linux-only here")
    def test_legacy_health_keeps_kernel_identity_fallback(self):
        result = self.install("--link-only", "--now", uname="Linux", managed=True, legacy_health=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("rsid healthy", result.stdout)

    @unittest.skipUnless(sys.platform.startswith("linux"), "checks Linux sender credentials")
    def test_claimed_holder_pid_cannot_replace_replying_daemon_identity(self):
        result = self.install("--link-only", "--now", uname="Linux", managed=True, wrong_pid=True)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("identified GetHealthStatus", result.stderr)

    def test_invalid_startup_identity_cannot_report_readiness(self):
        result = self.install("--link-only", "--now", bad_start=True)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("identified GetHealthStatus", result.stderr)

    def fake_darwin_health_client(self):
        """Run the health client as if on macOS, on a real inherited socket.

        Only the platform branch is simulated. Linux has no LOCAL_PEERPID
        (SOL_LOCAL=0, optname 2), so the fake answers that exact getsockopt with
        FAKE_LOCAL_PEERPID: a number, or "daemon" for the fixture daemon's PID.
        Each call is logged to $FAKE_STATE/peerpid.
        """
        write_executable(self.fakes / "python3", f"#!{sys.executable}\n" + """
import glob, os, socket, sys
if sys.argv[1] != "-":
    os.execv(sys.executable, [sys.executable, *sys.argv[1:]])
source = sys.stdin.read()
sys.argv = sys.argv[1:]
sys.platform = "darwin"
real_getsockopt = socket.socket.getsockopt


def getsockopt(self, level, name, *rest):
    if (level, name) != (0, 2):
        return real_getsockopt(self, level, name, *rest)
    wanted = os.environ.get("FAKE_LOCAL_PEERPID", "")
    if wanted == "daemon":
        wanted = open(glob.glob(os.environ["FAKE_STATE"] + "/daemon-*")[0]).read()
    with open(os.environ["FAKE_STATE"] + "/peerpid", "a") as log:
        log.write(wanted + "\\n")
    return int(wanted)


socket.socket.getsockopt = getsockopt
exec(compile(source, "health-client", "exec"))
""")

    def test_darwin_identity_branch_uses_the_daemon_reply_on_an_inherited_socket(self):
        # Simulate only the health client's platform branch, on a real owned
        # inherited socket. This exercises the portable PID path without
        # relying on Linux's unavailable Darwin LOCAL_PEERPID getsockopt.
        self.fake_darwin_health_client()
        result = self.install("--link-only", "--now", uname="Darwin")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("rsid healthy", result.stdout)
        self.assertFalse((self.state / "peerpid").exists(), "a reply that names its PID must not need LOCAL_PEERPID")

    def test_darwin_legacy_daemon_is_identified_by_local_peerpid(self):
        # #1875: a daemon that predates daemon_pid in GetHealthStatus is
        # identified on macOS by LOCAL_PEERPID alone; it must be accepted when
        # that PID is inside the launcher's process tree.
        self.fake_darwin_health_client()
        result = self.install("--link-only", "--now", uname="Darwin", legacy_health=True, extra_env={"FAKE_LOCAL_PEERPID": "daemon"})
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("rsid healthy", result.stdout)
        daemon_pid = next(self.state.glob("daemon-*")).read_text()
        self.assertEqual(set((self.state / "peerpid").read_text().split()), {daemon_pid})

    def test_darwin_legacy_daemon_outside_the_launcher_tree_is_refused(self):
        self.fake_darwin_health_client()
        result = self.install("--link-only", "--now", uname="Darwin", legacy_health=True, extra_env={"FAKE_LOCAL_PEERPID": str(os.getpid())})
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(set((self.state / "peerpid").read_text().split()), {str(os.getpid())})
        self.assertIn("NO DAEMON IS RUNNING from this launch", result.stderr)

    def test_answering_process_that_exits_right_after_replying_is_named(self):
        # #1875: the daemon replied and then died. The message must say so, not
        # that the launcher exited. A real exit races the probe's next `ps`, so a
        # fake `ps` reports the fixture daemon gone, deterministically.
        write_executable(
            self.fakes / "ps",
            "#!/bin/sh\n"
            'previous=""\n'
            'for arg in "$@"; do\n'
            '  [ "$previous" = -p ] && [ -f "$FAKE_STATE/daemon-$arg" ] && exit 1\n'
            '  previous="$arg"\n'
            "done\n"
            'PATH="${PATH#*:}" exec ps "$@"\n',
        )
        result = self.install("--link-only", "--now", uname="Darwin")
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("process that answered GetHealthStatus exited right after replying", result.stderr)
        self.assertNotIn("launcher exited", result.stderr)

    def launchd_plist_path(self):
        return self.home / "Library" / "LaunchAgents" / "dev.rsi.rsid.plist"

    def launchd_plist(self):
        return plistlib.loads(self.launchd_plist_path().read_bytes())

    def launchctl_calls(self):
        return (self.state / "launchctl").read_text().splitlines()

    def test_darwin_starts_rsid_as_the_launchd_agent(self):
        # #2936: macOS runs the holder chain as the user agent dev.rsi.rsid.
        result = self.install("--link-only", "--now", uname="Darwin")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        service = f"gui/{os.getuid()}/dev.rsi.rsid"
        self.assertIn(f"rsid restarted as launchd agent {service}", result.stdout)
        self.assertIn("rsid healthy", result.stdout)
        self.assertIn(f"bootstrap gui/{os.getuid()} {self.launchd_plist_path()}", self.launchctl_calls())
        job = self.launchd_plist()
        install = self.home / ".rsi" / "install"
        self.assertEqual(job["Label"], "dev.rsi.rsid")
        self.assertEqual(
            job["ProgramArguments"],
            [f"{install}/rsi-socket-hold", str(self.socket), "--",
             f"{install}/rsid-supervisor.sh", f"{install}/rsid"],
        )
        self.assertIs(job["RunAtLoad"], True)
        self.assertIs(job["KeepAlive"], True)
        self.assertEqual(job["StandardOutPath"], f"{self.home}/.rsi/daemon.log")
        self.assertEqual(job["StandardErrorPath"], f"{self.home}/.rsi/daemon.log")
        self.assertEqual(job["WorkingDirectory"], str(self.home))
        # Only the launch environment: provider secrets come from ~/.rsi/.env.
        environment = job["EnvironmentVariables"]
        self.assertEqual(set(environment), {"HOME", "PATH", "LANG", "RSI_SOCKET", "RSI_DAEMON_SOCKET_PATH"})
        self.assertEqual(environment["HOME"], str(self.home))
        self.assertEqual(environment["RSI_DAEMON_SOCKET_PATH"], str(self.socket))
        entries = environment["PATH"].split(":")
        self.assertEqual(entries[:2], [f"{self.home}/.local/bin", f"{self.home}/.cargo/bin"])
        for system in ("/usr/bin", "/bin", "/usr/sbin", "/sbin"):
            self.assertIn(system, entries)
        # The agent's holder is the launcher the health probe identified.
        self.assertIn(f"{install}/rsid-supervisor.sh", (self.state / "started").read_text())

    def test_darwin_agent_path_keeps_the_newest_nvm_node_and_drops_temporary_entries(self):
        for version in ("v9.11.2", "v24.22.0"):
            (self.home / ".nvm" / "versions" / "node" / version / "bin").mkdir(parents=True)
        result = self.install("--link-only", "--now", uname="Darwin")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        entries = self.launchd_plist()["EnvironmentVariables"]["PATH"].split(":")
        newest = f"{self.home}/.nvm/versions/node/v24.22.0/bin"
        self.assertIn(newest, entries)
        self.assertLess(entries.index(newest), entries.index("/usr/bin"))
        # The caller's temporary fakes directory does not outlive this run.
        self.assertEqual([entry for entry in entries if entry == str(self.fakes)], [])

    def test_darwin_restart_boots_the_loaded_agent_out_and_loads_it_again(self):
        first = self.install("--link-only", "--now", uname="Darwin")
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        first_holder = self.holder_pid()
        second = self.install("--link-only", "--now", uname="Darwin")
        self.assertEqual(second.returncode, 0, second.stdout + second.stderr)
        self.assertIn("Stopping launchd agent", second.stdout)
        self.assertIn("rsid healthy", second.stdout)
        calls = self.launchctl_calls()
        service = f"gui/{os.getuid()}/dev.rsi.rsid"
        bootout = calls.index(f"bootout {service}")
        bootstraps = [i for i, call in enumerate(calls) if call.startswith("bootstrap ")]
        self.assertEqual(len(bootstraps), 2, calls)
        self.assertLess(bootout, bootstraps[1])
        self.assertNotEqual(self.holder_pid(), first_holder)

    def test_darwin_no_restart_refreshes_the_agent_plist_without_loading_it(self):
        result = self.install("--link-only", "--no-restart", uname="Darwin")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.launchd_plist()["Label"], "dev.rsi.rsid")
        self.assertIn("launchd agent", result.stdout)
        self.assertFalse((self.state / "started").exists())

    def test_darwin_opt_out_keeps_the_detached_holder_chain(self):
        result = self.install("--link-only", "--now", uname="Darwin", extra_env={"RSI_LAUNCHD_AGENT": "0"})
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("rsid restarted on macOS", result.stdout)
        self.assertTrue((self.state / "started").exists())
        self.assertFalse(self.launchd_plist_path().exists())

    @unittest.skipUnless(sys.platform.startswith("linux"), "systemd launcher is Linux-only")
    def test_managed_service_rejects_a_live_foreign_responder(self):
        self.start_foreign_daemon()
        result = self.install("--link-only", "--now", uname="Linux", managed=True, holder_idle=True)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("identified GetHealthStatus", result.stderr)

    @unittest.skipUnless(sys.platform.startswith("linux"), "systemd launcher is Linux-only")
    def test_inactive_launcher_unit_cannot_report_success(self):
        result = self.install("--link-only", "--now", uname="Linux", unit_state="inactive")
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("exited before rsid became healthy", result.stderr)
    @unittest.skipUnless(sys.platform.startswith("linux"), "systemd launcher is Linux-only")
    def test_cleanup_stops_a_daemon_that_has_not_published_its_pid(self):
        # The installer returns while the fake daemon is held before its PID
        # marker (the late-start race of #232). Cleanup must still stop and
        # reap it, so nothing can write into the temp directory afterwards.
        gate = self.state / "gate"
        os.mkfifo(gate)
        result = self.install("--link-only", "--now", uname="Linux", unit_state="inactive", server_gate=gate)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("exited before rsid became healthy", result.stderr)
        # Opening the write end succeeds only once the daemon is a reader, i.e.
        # it is alive and blocked before writing daemon-<pid>.
        deadline = time.monotonic() + 5
        while True:
            try:
                held = os.open(gate, os.O_WRONLY | os.O_NONBLOCK)
                break
            except OSError as error:
                if error.errno != errno.ENXIO or time.monotonic() > deadline:
                    raise
                time.sleep(0.005)
        self.addCleanup(os.close, held)
        self.assertEqual(list(self.state.glob("daemon-*")), [])
        self.stop_launchers()
        # With the daemon gone nothing still reads the gate.
        with self.assertRaises(OSError) as raised:
            os.close(os.open(gate, os.O_WRONLY | os.O_NONBLOCK))
        self.assertEqual(raised.exception.errno, errno.ENXIO)
        self.assertEqual(list(self.state.glob("daemon-*")), [])


class RsidStartSatelliteTest(StartFixture):
    """#2820: `scripts/rsid-start-satellite` starts the deployed daemon on a
    freshly booted satellite and persists the worker slice limits.

    Reuses the hermetic fixture above. The deployed set is populated by a real
    `install-release.sh --link-only --no-restart` into the temp HOME.
    """

    def deploy(self):
        result = self.install("--link-only", "--no-restart", uname="Linux")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertTrue((self.home / ".rsi" / "install" / "rsid-supervisor.sh").exists())

    def start(self, **kwargs):
        return self.install(script="rsid-start-satellite", uname="Linux", **kwargs)

    def systemctl_calls(self):
        return (self.state / "systemctl").read_text().splitlines()

    def test_starts_the_deployed_daemon_in_a_detached_unit_not_a_scope(self):
        self.deploy()
        result = self.start()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("rsid started from", result.stdout)
        run = (self.state / "systemd-run").read_text()
        self.assertIn("--collect", run)
        self.assertNotIn("--scope", run)
        install_dir = self.home / ".rsi" / "install"
        self.assertIn(f"{install_dir}/rsid-supervisor.sh {install_dir}/rsid", run)
        self.assertNotIn(str(self.root), run)
        started = (self.state / "started").read_text()
        self.assertIn("daemon.sock", started)
        self.assertIn(f"{install_dir}/rsid", started)
        self.assertIn("GetHealthStatus", (self.state / "rpc").read_text())

    def test_persists_the_worker_slice_limits_from_the_scope_env(self):
        self.deploy()
        scope = self.home / ".rsi" / "rsid-scope.env"
        scope.write_text(
            "rsid_scope_memory_high_mib=6144\nrsid_scope_memory_max_mib=8192\n"
            "rsid_scope_memory_swap_max_mib=0\nrsid_scope_cpu_weight=20\n"
            "worker_scope_memory_high_mib=6144\nworker_scope_memory_max_mib=8192\n"
            "worker_scope_memory_swap_max_mib=0\nworker_scope_cpu_weight=20\n"
        )
        result = self.start()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        persisted = (
            "--user set-property rsi-workers.slice MemoryHigh=6144M "
            "MemoryMax=8192M MemorySwapMax=0M CPUWeight=20"
        )
        calls = self.systemctl_calls()
        self.assertIn(persisted, calls)
        self.assertIn(
            "--user set-property --runtime rsi-workers.slice MemoryHigh=6144M "
            "MemoryMax=8192M MemorySwapMax=0M CPUWeight=20",
            calls,
        )
        # Applied to the slice before the daemon unit is launched.
        self.assertTrue((self.state / "systemd-run").exists())

    def test_the_daemon_unit_carries_cargo_bin_on_path(self):
        """#2823: the unit builds with the pinned toolchain, not the system rustc."""
        self.deploy()
        result = self.start()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        run = (self.state / "systemd-run").read_text().split()
        paths = [word for word in run if word.startswith("--setenv=PATH=")]
        self.assertEqual(len(paths), 1, run)
        entries = paths[0].removeprefix("--setenv=PATH=").split(":")
        self.assertIn(f"{self.home}/.cargo/bin", entries)
        self.assertIn(f"{self.home}/.local/bin", entries)
        # The caller's own entries stay, after the prepended ones.
        self.assertIn(str(self.fakes), entries)
        self.assertLess(
            entries.index(f"{self.home}/.cargo/bin"), entries.index(str(self.fakes))
        )

    def test_an_existing_cargo_bin_entry_is_not_duplicated(self):
        self.deploy()
        cargo = f"{self.home}/.cargo/bin"
        result = self.start(extra_env={"PATH": f"{cargo}:{self.fakes}:{os.environ['PATH']}"})
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        run = (self.state / "systemd-run").read_text().split()
        entries = next(w for w in run if w.startswith("--setenv=PATH=")).split("=", 2)[2].split(":")
        self.assertEqual(entries.count(cargo), 1, entries)

    def test_a_running_daemon_is_left_alone(self):
        self.deploy()
        result = self.start(running=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("already running", result.stdout)
        self.assertFalse((self.state / "systemd-run").exists())
        self.assertFalse((self.state / "started").exists())

    def test_managed_service_is_started_after_the_slice_is_persisted(self):
        self.deploy()
        result = self.start(managed=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("rsid.service started", result.stdout)
        calls = self.systemctl_calls()
        persist = next(i for i, c in enumerate(calls) if c.startswith("--user set-property rsi-workers.slice"))
        start = calls.index("--user start rsid.service")
        self.assertLess(persist, start)

    def test_unhealthy_start_exits_nonzero(self):
        self.deploy()
        result = self.start(never_healthy=True)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("NO DAEMON IS RUNNING", result.stderr)

    def test_refuses_when_nothing_is_deployed(self):
        result = self.start()
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("Not deployed", result.stderr)
        self.assertFalse((self.state / "systemd-run").exists())

    def test_refuses_an_unsupported_platform(self):
        result = self.install(script="rsid-start-satellite", uname="FreeBSD")
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("supports Linux (systemd user units) and macOS", result.stderr)

    def test_macos_starts_the_deployed_daemon_as_the_launchd_agent(self):
        # #2936: on macOS the deployed chain is loaded as dev.rsi.rsid.
        self.deploy()
        result = self.install(script="rsid-start-satellite", uname="Darwin")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        service = f"gui/{os.getuid()}/dev.rsi.rsid"
        self.assertIn(f"as launchd agent {service}", result.stdout)
        plist = self.home / "Library" / "LaunchAgents" / "dev.rsi.rsid.plist"
        calls = (self.state / "launchctl").read_text().splitlines()
        self.assertIn(f"bootstrap gui/{os.getuid()} {plist}", calls)
        install_dir = self.home / ".rsi" / "install"
        self.assertIn(f"{install_dir}/rsid-supervisor.sh {install_dir}/rsid", (self.state / "started").read_text())
        self.assertIn("GetHealthStatus", (self.state / "rpc").read_text())

    def test_macos_a_loaded_agent_with_no_process_is_kickstarted(self):
        self.deploy()
        first = self.install(script="rsid-start-satellite", uname="Darwin")
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        # The holder chain ends while launchd keeps the job loaded.
        holder = self.holder_pid()
        os.kill(holder, signal.SIGTERM)
        deadline = time.monotonic() + 5
        while True:
            try:
                os.kill(holder, 0)
            except ProcessLookupError:
                break
            self.assertLess(time.monotonic(), deadline, "fake holder did not stop")
            time.sleep(0.02)
        second = self.install(script="rsid-start-satellite", uname="Darwin")
        self.assertEqual(second.returncode, 0, second.stdout + second.stderr)
        calls = (self.state / "launchctl").read_text().splitlines()
        self.assertIn(f"kickstart gui/{os.getuid()}/dev.rsi.rsid", calls)
        self.assertNotEqual(self.holder_pid(), holder)
        self.assertIn("rsid healthy", second.stdout)


if __name__ == "__main__":
    unittest.main()
