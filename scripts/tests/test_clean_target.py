import subprocess
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from scratch_git import scratch_tempdir  # noqa: E402

SCRIPT = Path(__file__).resolve().parents[1] / "clean-target"


class CleanTargetTest(unittest.TestCase):
    def setUp(self):
        self.temp = scratch_tempdir("clean-target-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def run_script(self, *args):
        return subprocess.run(
            [str(SCRIPT), *args], capture_output=True, text=True, cwd=self.root
        )

    def test_keeps_only_the_harness_tmp(self):
        target = self.root / "target"
        (target / "debug" / "deps").mkdir(parents=True)
        (target / "debug" / "deps" / "a.rlib").write_text("x")
        (target / ".rsi-tmp" / "claude-1000").mkdir(parents=True)
        (target / ".rsi-tmp" / "claude-1000" / "task.output").write_text("live")
        (target / "CACHEDIR.TAG").write_text("sig")
        (target / ".hidden").write_text("h")
        result = self.run_script(str(self.root))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([p.name for p in target.iterdir()], [".rsi-tmp"])
        self.assertEqual(
            (target / ".rsi-tmp" / "claude-1000" / "task.output").read_text(), "live"
        )

    def test_missing_target_is_a_noop(self):
        result = self.run_script(str(self.root))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((self.root / "target").exists())

    def test_symlinked_target_is_refused(self):
        real = self.root / "real"
        real.mkdir()
        (real / "keep").write_text("k")
        (self.root / "target").symlink_to(real)
        result = self.run_script(str(self.root))
        self.assertEqual(result.returncode, 1)
        self.assertTrue((real / "keep").exists())

    def test_two_arguments_are_a_usage_error(self):
        self.assertEqual(self.run_script("a", "b").returncode, 2)


if __name__ == "__main__":
    unittest.main()
