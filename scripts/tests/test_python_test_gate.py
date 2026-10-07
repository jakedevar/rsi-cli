"""Scripts/tools gates select and execute only matching Python suites (#1606)."""

import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "python-test-gate.py"
SPEC = importlib.util.spec_from_file_location("python_test_gate", SCRIPT)
GATE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(GATE)


class PythonTestGate(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
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

    def test_changed_test_selects_itself_and_deleted_source_keeps_coverage(self):
        self.assertEqual(self.select("scripts/tests/test_other.py"), ["scripts/tests/test_other.py"])
        self.assertEqual(self.select("scripts/check-touched-shards"),
                         ["scripts/tests/test_check_touched_shards.py"])

    def test_docs_thoughts_and_unmatched_scripts_select_nothing(self):
        self.assertEqual(self.select("docs/widget.md", "thoughts/plan.md", "scripts/uncovered.sh"), [])

    def test_runner_uses_stdlib_for_unittest_and_pytest_for_function_tests(self):
        self.assertEqual(GATE.test_commands(self.repo, self.select(
            "scripts/tests/test_other.py", "tools/widget.py")), [
                ["python3", "-m", "unittest", "-v", "scripts/tests/test_other.py"],
                ["python3", "-m", "pytest", "-q", "tools/tests/test_widget.py"]])

    def test_git_diff_plan_runs_candidate_tests_and_returns_a_failure(self):
        def git(*args):
            return subprocess.run(["git", *args], cwd=self.repo, check=True, capture_output=True, text=True).stdout.strip()
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
