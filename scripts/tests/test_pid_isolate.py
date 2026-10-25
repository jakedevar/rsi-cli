"""PID-namespace wrapper for test lanes (#3478): probe rules, argv shape and the real canary."""

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import time
import unittest
from types import SimpleNamespace

SCRIPT = Path(__file__).resolve().parents[1] / "pid-isolate.py"
_spec = importlib.util.spec_from_file_location("pid_isolate", SCRIPT)
ISOLATE = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(ISOLATE)


def fake_run(*codes, stderr=""):
    """A subprocess.run stand-in that answers each call with the next exit code."""
    calls = []
    answers = iter(codes)

    def run(argv, **_kwargs):
        calls.append(argv)
        return SimpleNamespace(returncode=next(answers), stderr=stderr, stdout="")
    return run, calls


class ProbeRules(unittest.TestCase):
    def probe(self, run=None, **kwargs):
        kwargs.setdefault("platform", "linux")
        kwargs.setdefault("which", lambda name: "/usr/bin/" + name)
        kwargs.setdefault("environ", {})
        return ISOLATE.probe(run or fake_run(0)[0], **kwargs)

    def test_a_passing_canary_is_namespaced_with_the_callers_own_uid(self):
        run, calls = fake_run(0)
        found = self.probe(run)
        self.assertEqual(found, {"mode": "namespaced", "reason": "", "mapping": "current_user"})
        argv = calls[0]
        self.assertEqual(argv[0], "unshare")
        for flag in ("--user", f"--map-user={os.getuid()}", f"--map-group={os.getgid()}", "--pid",
                     "--fork", "--kill-child", "--mount-proc"):
            self.assertIn(flag, argv)
        self.assertNotIn("--map-root-user", argv)

    def test_an_unshare_without_map_user_falls_back_to_root_mapping(self):
        run, calls = fake_run(1, 0, stderr="unshare: unrecognized option '--map-user=1000'")
        found = self.probe(run)
        self.assertEqual((found["mode"], found["mapping"]), ("namespaced", "root"))
        self.assertIn("--map-root-user", calls[1])

    def test_unavailable_reasons_are_typed(self):
        self.assertEqual(self.probe(environ={"RSI_PID_ISOLATION": "off"})["reason"], "disabled")
        self.assertEqual(self.probe(platform="darwin")["reason"], "not_linux")
        self.assertEqual(self.probe(platform="win32")["reason"], "not_linux")
        self.assertEqual(self.probe(which=lambda _name: None)["reason"], "unshare_missing")
        denied, _ = fake_run(1, 1, stderr="unshare: unshare failed: Operation not permitted")
        found = self.probe(denied)
        self.assertEqual(found["mode"], "unavailable")
        self.assertIn("Operation not permitted", found["reason"])

    def test_a_canary_that_still_sees_host_pids_is_unavailable_not_namespaced(self):
        found = self.probe(fake_run(12)[0])
        self.assertEqual(found, {"mode": "unavailable", "reason": "host_pids_visible"})

    def test_an_unstartable_unshare_is_unavailable(self):
        def broken(*_args, **_kwargs):
            raise OSError("exec format error")
        self.assertEqual(self.probe(broken)["mode"], "unavailable")

    def test_wrap_prefixes_only_a_namespaced_result(self):
        command = ["python3", "-m", "unittest", "t.py"]
        namespaced = {"mode": "namespaced", "reason": "", "mapping": "current_user"}
        wrapped = ISOLATE.wrap(command, namespaced)
        self.assertEqual(wrapped[-4:], command)
        self.assertEqual(wrapped[0], "unshare")
        self.assertEqual(wrapped[wrapped.index("--") + 1:], command)
        self.assertEqual(ISOLATE.wrap(command, {"mode": "unavailable", "reason": "not_linux"}), command)


@unittest.skipUnless(sys.platform.startswith("linux"), "PID namespaces are Linux-only")
class RealNamespace(unittest.TestCase):
    """The canary: a process in the namespace cannot see, signal or find a host process."""

    @classmethod
    def setUpClass(cls):
        cls.status = ISOLATE.probe(environ={})

    def setUp(self):
        if self.status["mode"] != "namespaced":
            self.skipTest("unprivileged user + PID namespaces unavailable here: " + self.status["reason"])
        # Stands in for the hub daemon: a host process the namespaced command must not reach.
        self.decoy = subprocess.Popen(["sleep", "300"])
        self.addCleanup(self.stop_decoy)

    def stop_decoy(self):
        self.decoy.kill()
        self.decoy.wait()

    def isolate(self, *argv, **kwargs):
        return subprocess.run([sys.executable, str(SCRIPT), "--require", "--", *argv],
                              capture_output=True, text=True, timeout=60, **kwargs)

    def test_the_decoy_daemon_pid_is_invisible_and_cannot_be_signalled(self):
        script = (
            "import os, signal, sys\n"
            "pid = int(sys.argv[1])\n"
            "assert os.getpid() == 1, os.getpid()\n"
            "assert not os.path.exists('/proc/%d' % pid), 'decoy visible in /proc'\n"
            "assert pid not in {int(n) for n in os.listdir('/proc') if n.isdigit()}\n"
            "for sig in (0, signal.SIGINT, signal.SIGTERM, signal.SIGKILL):\n"
            "    try:\n"
            "        os.kill(pid, sig)\n"
            "    except ProcessLookupError:\n"
            "        continue\n"
            "    sys.exit('signalled the decoy with %d' % sig)\n"
        )
        done = self.isolate(sys.executable, "-c", script, str(self.decoy.pid))
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn("pid_isolation=namespaced", done.stderr)
        time.sleep(0.2)
        self.assertIsNone(self.decoy.poll(), "the decoy was signalled from inside the namespace")

    def test_host_wide_pgrep_and_pkill_find_nothing_outside_the_tree(self):
        # The #3476 shape: `pgrep -x <daemon>` then `kill -INT` of what it found.
        decoy_name = "sleep"
        script = (f"pgrep -x {decoy_name} >/dev/null && {{ pkill -INT -x {decoy_name}; exit 9; }}; exit 0")
        done = self.isolate("sh", "-c", script)
        self.assertEqual(done.returncode, 0, done.stderr)
        time.sleep(0.2)
        self.assertIsNone(self.decoy.poll())

    def test_the_wrapped_commands_exit_status_and_output_pass_through(self):
        done = self.isolate("sh", "-c", "echo out; echo err >&2; exit 7")
        self.assertEqual(done.returncode, 7)
        self.assertIn("out", done.stdout)
        self.assertIn("err", done.stderr)

    def test_killing_the_wrapper_takes_the_namespace_down(self):
        # A unique argv makes the namespaced sleep findable from the host.
        duration = f"3{os.getpid()}.5"
        child = subprocess.Popen([sys.executable, str(SCRIPT), "--require", "--", "sleep", duration],
                                 stderr=subprocess.DEVNULL)
        pattern = f"^sleep {duration}$"
        deadline = time.monotonic() + 10
        while subprocess.run(["pgrep", "-f", pattern], capture_output=True).returncode != 0:
            self.assertLess(time.monotonic(), deadline, "the namespaced sleep never started")
            time.sleep(0.1)
        child.kill()
        child.wait()
        # --kill-child: nothing from that namespace outlives its killed wrapper.
        deadline = time.monotonic() + 10
        while subprocess.run(["pgrep", "-f", pattern], capture_output=True).returncode == 0:
            self.assertLess(time.monotonic(), deadline, "a namespaced sleep outlived its killed wrapper")
            time.sleep(0.1)


class CommandLine(unittest.TestCase):
    def run_cli(self, *args, env=None):
        merged = {**os.environ, **(env or {})}
        return subprocess.run([sys.executable, str(SCRIPT), *args], capture_output=True, text=True,
                              timeout=60, env=merged)

    def test_status_prints_json_and_exits_3_when_unavailable(self):
        done = self.run_cli("--status", env={"RSI_PID_ISOLATION": "off"})
        self.assertEqual(done.returncode, 3)
        self.assertEqual(json.loads(done.stdout), {"mode": "unavailable", "reason": "disabled"})

    def test_a_disabled_namespace_runs_the_command_unwrapped_unless_required(self):
        env = {"RSI_PID_ISOLATION": "off"}
        done = self.run_cli("--", sys.executable, "-c", "import os; print(os.getppid() > 0)", env=env)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn("pid_isolation=unavailable reason=disabled", done.stderr)
        required = self.run_cli("--require", "--", sys.executable, "-c", "print('ran')", env=env)
        self.assertEqual(required.returncode, 3)
        self.assertNotIn("ran", required.stdout)

    def test_usage_without_a_command_exits_2(self):
        self.assertEqual(self.run_cli().returncode, 2)


if __name__ == "__main__":
    unittest.main()
