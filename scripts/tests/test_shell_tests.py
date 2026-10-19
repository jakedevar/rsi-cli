"""Run every `scripts/tests/*.sh` shell test inside the Python script-test lane (#297).

`scripts/python-test-gate.py` only selects `test_*.py` suites, so a shell test beside
them ran in no lane: a change to `scripts/rsid-health.sh` passed its scoped_test
without ever running `test_rsid_health_wait.sh`. This wrapper is the lane. It runs each
shell test as one subTest (exit 0, stderr shown on failure) and names, per shell test,
the subject scripts it covers. The gate selects a suite whose text mentions a changed
file's basename, so listing the shell test and its subjects below is what makes a
change to either one select this wrapper (proved by `test_the_gate_selects_*`).

Adding a `scripts/tests/*.sh` without listing it in SHELL_TESTS (or SKIPPED) fails
`test_every_shell_test_is_listed`.
"""

import importlib.util
from pathlib import Path
import shutil
import subprocess
import sys
import unittest

SCRIPTS = Path(__file__).resolve().parents[1]
TESTS = SCRIPTS / "tests"
ROOT = SCRIPTS.parent
TIMEOUT_SECONDS = 180

# shell test -> the scripts it exercises (basenames under scripts/).
SHELL_TESTS = {
    "check-corpus-pins.test.sh": ("check-corpus-pins.sh",),
    "test_rsid_health_wait.sh": ("rsid-health.sh",),
    "test_rsid_processes.sh": ("rsid-processes.sh",),
    "test_rsid_quiet_restart.sh": ("rsid-quiet-restart.sh", "rsid-supervisor.sh"),
}

# shell test -> "#<Issue>: reason". A skipped test is reported as skipped, never green.
SKIPPED = {}

_gate_spec = importlib.util.spec_from_file_location("python_test_gate", SCRIPTS / "python-test-gate.py")
GATE = importlib.util.module_from_spec(_gate_spec)
_gate_spec.loader.exec_module(GATE)


def bash_unavailable():
    """Reason the shell tests cannot run on this host, else None."""
    if sys.platform == "win32":
        return "bash shell tests are not run on Windows"
    if shutil.which("bash") is None:
        return "bash is not installed on this host"
    return None


class ShellTests(unittest.TestCase):
    def test_every_shell_test_is_listed(self):
        on_disk = {path.name for path in TESTS.glob("*.sh")}
        listed = set(SHELL_TESTS) | set(SKIPPED)
        self.assertEqual(on_disk - listed, set(), "scripts/tests/*.sh not run by any lane: list it in SHELL_TESTS")
        self.assertEqual(listed - on_disk, set(), "listed shell test no longer exists")

    def test_a_skip_names_a_filed_issue(self):
        for name, reason in SKIPPED.items():
            self.assertRegex(reason, r"^#\d+: \S", f"{name}: a skip must name a filed Issue and a reason")
            self.assertNotIn(name, SHELL_TESTS, f"{name} is both run and skipped")

    def test_every_covered_subject_script_exists(self):
        for name, subjects in SHELL_TESTS.items():
            self.assertTrue(subjects, f"{name} names no subject script")
            for subject in subjects:
                self.assertTrue((SCRIPTS / subject).is_file(), f"{name}: scripts/{subject} does not exist")

    def test_the_gate_selects_this_wrapper_for_a_shell_test_or_its_subject(self):
        wrapper = Path(__file__).resolve().relative_to(ROOT).as_posix()
        for name, subjects in SHELL_TESTS.items():
            for changed in (Path("scripts/tests") / name, *(Path("scripts") / s for s in subjects)):
                with self.subTest(shell_test=name, changed=changed.as_posix()):
                    self.assertIn(wrapper, GATE.selected_tests(ROOT, [changed]))

    def test_the_gate_runs_this_wrapper_under_the_stdlib_runner(self):
        wrapper = Path(__file__).resolve().relative_to(ROOT).as_posix()
        commands = GATE.test_commands(ROOT, [wrapper], "python3")
        self.assertEqual(commands, [["python3", "-m", "unittest", "-v", wrapper]])

    def test_each_shell_test_exits_zero(self):
        unavailable = bash_unavailable()
        for name in sorted(SHELL_TESTS):
            with self.subTest(shell_test=name):
                if unavailable is not None:
                    self.skipTest(unavailable)
                result = subprocess.run(["bash", str(TESTS / name)], cwd=ROOT, capture_output=True,
                                        text=True, timeout=TIMEOUT_SECONDS)
                self.assertEqual(result.returncode, 0,
                                 f"{name} exited {result.returncode}\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}")

    def test_skipped_shell_tests_are_reported_as_skipped(self):
        for name, reason in sorted(SKIPPED.items()):
            with self.subTest(shell_test=name):
                self.skipTest(f"{name}: {reason}")


if __name__ == "__main__":
    unittest.main()
