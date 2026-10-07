import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "fmt-changed.sh"

UNFORMATTED_CHILD = "pub fn child( ) -> u8 {  1 }\n"


@unittest.skipUnless(shutil.which("rustfmt"), "rustfmt is not installed")
class FmtChangedTest(unittest.TestCase):
    """#1121: formatting a changed file leaves its untouched child modules alone."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="fmt-changed-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.name", "Fmt Changed Test")
        self.git("config", "user.email", "fmt-changed@example.invalid")
        (self.root / "src").mkdir()
        self.write("src/lib.rs", "mod child;\npub fn parent() -> u8 {\n    child::child()\n}\n")
        self.write("src/child.rs", UNFORMATTED_CHILD)
        self.git("add", ".")
        self.git("commit", "-q", "-m", "base")

    def git(self, *args):
        return subprocess.run(
            ["git", *args], cwd=self.root, check=True, capture_output=True, text=True
        ).stdout

    def write(self, relative, text):
        (self.root / relative).write_text(text)

    def read(self, relative):
        return (self.root / relative).read_text()

    def run_script(self, *args):
        return subprocess.run(
            [str(SCRIPT), *args], cwd=self.root, check=True, capture_output=True, text=True
        ).stdout

    def test_formats_the_changed_parent_and_restores_its_untouched_child(self):
        self.write("src/lib.rs", "mod child;\npub fn parent( ) -> u8 { child::child() + 1 }\n")

        printed = self.run_script()

        self.assertEqual(printed.split(), ["src/lib.rs"])
        self.assertEqual(
            self.read("src/lib.rs"),
            "mod child;\npub fn parent() -> u8 {\n    child::child() + 1\n}\n",
        )
        self.assertEqual(self.read("src/child.rs"), UNFORMATTED_CHILD)
        self.assertEqual(self.git("status", "--porcelain").split(), ["M", "src/lib.rs"])

    def test_a_changed_child_is_formatted_with_its_parent(self):
        self.write("src/lib.rs", "mod child;\npub fn parent( ) -> u8 { child::child() }\n")
        self.write("src/child.rs", "pub fn child( ) -> u8 {  2 }\n")

        self.run_script()

        self.assertEqual(self.read("src/child.rs"), "pub fn child() -> u8 {\n    2\n}\n")

    def test_base_mode_formats_files_committed_since_the_base(self):
        base = self.git("rev-parse", "HEAD").strip()
        self.write("src/lib.rs", "mod child;\npub fn parent( ) -> u8 { child::child() }\n")
        self.git("commit", "-q", "-am", "unformatted change")

        self.run_script(base)

        self.assertEqual(
            self.read("src/lib.rs"),
            "mod child;\npub fn parent() -> u8 {\n    child::child()\n}\n",
        )
        self.assertEqual(self.read("src/child.rs"), UNFORMATTED_CHILD)

    def test_no_changed_rust_file_is_a_no_op(self):
        self.write("README.md", "notes\n")

        self.assertEqual(self.run_script(), "")
        self.assertEqual(self.read("src/child.rs"), UNFORMATTED_CHILD)

    def test_moving_base_formats_only_worker_changes(self):
        base = self.git("rev-parse", "HEAD").strip()
        self.write("src/child.rs", "pub fn child( ) -> u8 {  2 }\n")
        self.git("commit", "-q", "-am", "upstream change")
        self.git("update-ref", "refs/remotes/origin/rolling", "HEAD")
        # Build the worker's divergent history in this disposable fixture.
        self.git("reset", "--hard", base)
        self.write("src/lib.rs", "mod child;\npub fn parent( ) -> u8 { child::child() + 1 }\n")
        self.git("commit", "-q", "-am", "worker change")

        printed = self.run_script("origin/rolling")

        self.assertEqual(printed.splitlines(), ["src/lib.rs"])
        self.assertEqual(self.read("src/child.rs"), UNFORMATTED_CHILD)
        self.assertEqual(
            self.read("src/lib.rs"),
            "mod child;\npub fn parent() -> u8 {\n    child::child() + 1\n}\n",
        )

    def test_includes_staged_unstaged_and_untracked_rust_files(self):
        for args in [(), ("HEAD",)]:
            with self.subTest(args=args):
                self.write("src/staged.rs", "pub fn staged( ) { }\n")
                self.git("add", "src/staged.rs")
                self.write("src/child.rs", "pub fn child( ) -> u8 {  2 }\n")
                self.write("src/untracked.rs", "pub fn untracked( ) { }\n")

                printed = self.run_script(*args)

                self.assertEqual(
                    printed.splitlines(),
                    ["src/child.rs", "src/staged.rs", "src/untracked.rs"],
                )
                self.assertEqual(self.read("src/child.rs"), "pub fn child() -> u8 {\n    2\n}\n")
                self.assertEqual(self.read("src/staged.rs"), "pub fn staged() {}\n")
                self.assertEqual(self.read("src/untracked.rs"), "pub fn untracked() {}\n")


if __name__ == "__main__":
    unittest.main()
