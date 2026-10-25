"""Scripts/tools gates select and execute only matching Python suites (#1606)."""

import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from scratch_git import scratch_git, scratch_tempdir  # noqa: E402

SCRIPT = Path(__file__).resolve().parents[1] / "python-test-gate.py"
SPEC = importlib.util.spec_from_file_location("python_test_gate", SCRIPT)
GATE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(GATE)


class PythonTestGate(unittest.TestCase):
    def setUp(self):
        self.temp = scratch_tempdir(prefix="python-test-gate-")
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name)
        self.write("scripts/tests/test_check_touched_shards.py", "import unittest\n# test-coverage.json\n")
        self.write("scripts/tests/test_check_released_migrations.py", "import unittest\n")
        self.write("tools/tests/test_widget.py", "def test_widget(): pass\n")
        self.write("scripts/tests/test_other.py", "import unittest\n")

    def write(self, path, text):
        target = self.repo / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text)

    def select(self, *paths):
        return GATE.selected_tests(self.repo, paths)

    def test_script_and_its_changed_test_are_deduplicated(self):
        self.assertEqual(self.select("scripts/check-touched-shards", "scripts/tests/test_check_touched_shards.py"),
                         ["scripts/tests/test_check_touched_shards.py"])

    def test_support_file_selects_the_suite_that_reads_it(self):
        self.assertEqual(self.select("scripts/test-coverage.json"),
                         ["scripts/tests/test_check_touched_shards.py"])

    def test_tools_can_match_both_test_locations(self):
        self.assertEqual(self.select("tools/check-released-migrations.py", "tools/widget.py"),
                         ["scripts/tests/test_check_released_migrations.py", "tools/tests/test_widget.py"])

    def test_changed_top_level_tools_suite_selects_itself(self):
        self.write("tools/test_gadget.py", "import unittest\n")
        self.assertEqual(self.select("tools/test_gadget.py"), ["tools/test_gadget.py"])

    def test_tools_source_matches_all_three_suite_locations(self):
        self.write("tools/test_check_released_migrations.py", "import unittest\n")
        self.write("tools/tests/test_check_released_migrations.py", "def test_check(): pass\n")
        expected = ["scripts/tests/test_check_released_migrations.py",
                    "tools/test_check_released_migrations.py",
                    "tools/tests/test_check_released_migrations.py"]
        for source in ("tools/check-released-migrations.py", "tools/check_released_migrations.py"):
            with self.subTest(source=source):
                self.assertEqual(self.select(source), expected)

    def test_unrelated_change_keeps_top_level_tools_suite_unselected(self):
        self.write("tools/test_gadget.py", "import unittest\n# gadget-config.json\n")
        self.assertEqual(self.select("tools/unrelated.py"), [])
        self.assertEqual(self.select("tools/gadget-config.json"), ["tools/test_gadget.py"])
        self.assertEqual(self.select("scripts/tests/test_other.py", "tools/widget.py"),
                         ["scripts/tests/test_other.py", "tools/tests/test_widget.py"])

    def test_top_level_tools_suites_keep_their_runner_and_unavailable_status(self):
        self.write("tools/test_gadget.py", "import unittest\n")
        self.write("tools/test_function.py", "def test_function(): pass\n")
        commands = GATE.test_commands(self.repo, self.select(
            "tools/test_gadget.py", "tools/test_function.py"), sys.executable)
        self.assertEqual(commands, [
            [sys.executable, "-m", "pytest", "-q", "tools/test_function.py"],
            [sys.executable, "-m", "unittest", "-v", "tools/test_gadget.py"]])
        with patch.object(GATE.importlib.util, "find_spec", return_value=None):
            status = GATE.runner_status(commands[0])
        self.assertEqual((status["status"], status["runner"], status["executed_tests"]),
                         ("runner_unavailable", "pytest", 0))

    def test_changed_test_selects_itself_and_deleted_source_keeps_coverage(self):
        self.assertEqual(self.select("scripts/tests/test_other.py"), ["scripts/tests/test_other.py"])
        self.assertEqual(self.select("scripts/check-touched-shards"),
                         ["scripts/tests/test_check_touched_shards.py"])

    def test_a_new_test_file_selects_the_guards_that_scan_the_whole_directory(self):
        # #3306: a new scripts/tests/test_x.py landed red because the guard that
        # scans every file of the directory was not selected with it.
        self.write("scripts/tests/test_fixture_git_guard.py",
                   "import unittest\n\n# python-test-gate: scans scripts/tests\n")
        self.write("scripts/tests/test_install_release_git_hooks.py", "import unittest\n")
        self.assertEqual(self.select("scripts/tests/test_install_release_git_hooks.py"),
                         ["scripts/tests/test_fixture_git_guard.py",
                          "scripts/tests/test_install_release_git_hooks.py"])
        # A non-test fixture file and a deletion also select the guard.
        self.assertEqual(self.select("scripts/tests/fixtures/data.txt"),
                         ["scripts/tests/test_fixture_git_guard.py"])
        self.assertEqual(self.select("scripts/tests/test_gone.sh"),
                         ["scripts/tests/test_fixture_git_guard.py"])

    def test_a_scan_guard_is_not_selected_by_paths_outside_its_directory(self):
        self.write("scripts/tests/test_fixture_git_guard.py",
                   "import unittest\n# python-test-gate: scans scripts/tests\n")
        self.assertEqual(self.select("scripts/uncovered.sh", "tools/widget.py", "docs/x.md"),
                         ["tools/tests/test_widget.py"])
        # The marker names its directory: a tools/tests scan is not selected by scripts/tests.
        self.write("tools/tests/test_tools_guard.py", "import unittest\n# python-test-gate: scans tools/tests\n")
        self.assertEqual(self.select("scripts/tests/test_other.py"),
                         ["scripts/tests/test_fixture_git_guard.py", "scripts/tests/test_other.py"])

    def test_docs_thoughts_and_unmatched_scripts_select_nothing(self):
        self.assertEqual(self.select("docs/widget.md", "thoughts/plan.md", "scripts/uncovered.sh"), [])

    def test_a_path_outside_scripts_and_tools_selects_the_test_that_names_it(self):
        self.write("scripts/tests/test_desktop.py",
                   "import unittest\nLOCK = 'desktop/src-tauri/Cargo.lock'\nTOML = 'desktop/src-tauri/Cargo.toml'\n")
        self.assertEqual(self.select("desktop/src-tauri/Cargo.lock"), ["scripts/tests/test_desktop.py"])
        # The basename of a manifest is everywhere: only the full path observes it.
        self.assertEqual(self.select("crates/rsid/Cargo.toml"), [])
        self.assertEqual(self.select("desktop/src-tauri/src/main.rs"), [])

    def test_changed_paths_cover_every_directory_not_only_scripts_and_tools(self):
        def git(*args):
            return scratch_git(self.repo, *args).stdout.strip()
        identity = ("-c", "user.name=Test", "-c", "user.email=test@example.invalid")
        self.write("scripts/tests/test_desktop.py", "import unittest\n# desktop/src-tauri/Cargo.lock\n")
        self.write("desktop/src-tauri/Cargo.lock", "a\n")
        git("init", "-q")
        git("add", "-A")
        git(*identity, "commit", "-qm", "base")
        base = git("rev-parse", "HEAD")
        self.write("desktop/src-tauri/Cargo.lock", "b\n")
        git("add", "desktop/src-tauri/Cargo.lock")
        git(*identity, "commit", "-qm", "lock")
        paths = GATE.changed_paths(self.repo, base, "HEAD")
        self.assertEqual(paths, [Path("desktop/src-tauri/Cargo.lock")])
        self.assertEqual(GATE.selected_tests(self.repo, paths), ["scripts/tests/test_desktop.py"])

    def test_runner_uses_stdlib_for_unittest_and_pytest_for_function_tests(self):
        self.assertEqual(GATE.test_commands(self.repo, self.select(
            "scripts/tests/test_other.py", "tools/widget.py")), [
                ["python3", "-m", "unittest", "-v", "scripts/tests/test_other.py"],
                ["python3", "-m", "pytest", "-q", "tools/tests/test_widget.py"]])

    def test_available_stdlib_unittest_runner_is_not_reported_unavailable(self):
        command = [sys.executable, "-m", "unittest", "-v", "scripts/tests/test_other.py"]
        self.assertIsNone(GATE.runner_status(command))

    def test_missing_module_is_a_typed_runner_unavailable_with_zero_tests(self):
        command = [sys.executable, "-m", "pytest", "-q", "tools/tests/test_widget.py"]
        with patch.object(GATE.importlib.util, "find_spec", return_value=None):
            status = GATE.runner_status(command)
        self.assertEqual(status, {
            "status": "runner_unavailable", "runner": "pytest", "python": sys.executable,
            "test_file": "tools/tests/test_widget.py", "executed_tests": 0,
            "reason": f"{sys.executable} -m pytest: No module named pytest"})

    def test_missing_module_for_another_interpreter_is_probed_without_importing(self):
        command = ["python-elsewhere", "-m", "pytest", "-q", "tools/tests/test_widget.py"]
        probes = []

        def probe(args, **kwargs):
            probes.append(args)
            return subprocess.CompletedProcess(args, 3, "", "")
        status = GATE.runner_status(command, run=probe)
        self.assertEqual(probes[0][:3], ["python-elsewhere", "-I", "-c"])
        self.assertEqual((status["status"], status["runner"], status["executed_tests"]),
                         ("runner_unavailable", "pytest", 0))
        self.assertIn("No module named pytest", status["reason"])

    def test_missing_interpreter_is_runner_unavailable_not_a_crash(self):
        def probe(args, **kwargs):
            raise FileNotFoundError(2, "No such file or directory", args[0])
        status = GATE.runner_status(["python-missing", "-m", "unittest", "-v", "t.py"], run=probe)
        self.assertEqual((status["status"], status["runner"], status["executed_tests"]),
                         ("runner_unavailable", "unittest", 0))
        self.assertIn("python-missing", status["reason"])

    def test_probe_that_finds_the_module_is_available(self):
        probe = lambda args, **kwargs: subprocess.CompletedProcess(args, 0, "", "")
        self.assertIsNone(GATE.runner_status(["python-elsewhere", "-m", "pytest", "-q", "t.py"], run=probe))

    def test_unavailable_function_style_runner_never_falls_back_to_unittest(self):
        commands = GATE.test_commands(self.repo, self.select("tools/widget.py"))
        self.assertEqual(commands, [["python3", "-m", "pytest", "-q", "tools/tests/test_widget.py"]])
        with patch.object(GATE.importlib.util, "find_spec", return_value=None):
            status = GATE.runner_status([sys.executable, *commands[0][1:]])
        self.assertEqual((status["status"], status["runner"]), ("runner_unavailable", "pytest"))

    def test_genuine_failure_is_the_runner_exit_and_distinct_from_unavailable(self):
        self.write("scripts/tests/test_failing.py", "import unittest\nclass T(unittest.TestCase):\n"
                   "    def test_fails(self): self.assertEqual(1, 2)\n"
                   "    def test_passes(self): self.assertEqual(2, 2)\n")
        command = GATE.test_commands(self.repo, ["scripts/tests/test_failing.py"], sys.executable)[0]
        self.assertIsNone(GATE.runner_status(command))
        result = subprocess.run(command, cwd=self.repo, capture_output=True, text=True)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("Ran 2 tests", result.stderr)
        self.assertIn("test_fails", result.stderr)

    def test_available_runner_executes_positive_assertions(self):
        self.write("scripts/tests/test_passing.py", "import unittest\nclass T(unittest.TestCase):\n"
                   "    def test_one(self): self.assertEqual(1, 1)\n"
                   "    def test_two(self): self.assertTrue(True)\n")
        command = GATE.test_commands(self.repo, ["scripts/tests/test_passing.py"], sys.executable)[0]
        result = subprocess.run(command, cwd=self.repo, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Ran 2 tests", result.stderr)

    def test_git_diff_plan_runs_candidate_tests_and_returns_a_failure(self):
        def git(*args):
            return scratch_git(self.repo, *args).stdout.strip()
        git("init", "-q")
        git("-c", "user.name=Test", "-c", "user.email=test@example.invalid",
            "commit", "--allow-empty", "-qm", "base")
        base = git("rev-parse", "HEAD")
        self.write("scripts/tests/test_other.py", "import unittest\nclass Test(unittest.TestCase):\n"
                   "    def test_candidate(self): self.assertEqual(1, 2)\n")
        git("add", "scripts/tests/test_other.py")
        git("-c", "user.name=Test", "-c", "user.email=test@example.invalid", "commit", "-qm", "candidate")
        plan = subprocess.run([sys.executable, str(SCRIPT), "--repo", str(self.repo), "--base", base],
                              check=True, capture_output=True, text=True)
        commands = json.loads(plan.stdout)
        self.assertEqual(len(commands), 1)
        result = subprocess.run(commands[0], cwd=self.repo, capture_output=True, text=True)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("test_candidate", result.stderr)


if __name__ == "__main__":
    unittest.main()
