#!/usr/bin/env python3
"""Tests for tools/check_operator_identifiers.py (#1454). Run: python3 -m unittest tools/test_check_operator_identifiers.py"""
import contextlib
import io
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import check_operator_identifiers as coi  # noqa: E402

# Fictional identifiers; the real list is private local config.
EMAIL = "jane.doe" + "@example.invalid"
PHONE = "555-0100-" + "0199"


def git(cwd, *args):
    subprocess.run(
        ["git", "-c", "user.email=t@example.invalid", "-c", "user.name=t", *args],
        cwd=cwd, check=True, capture_output=True,
    )


class Repo:
    def __init__(self, test):
        self.tmp = tempfile.TemporaryDirectory()
        test.addCleanup(self.tmp.cleanup)
        self.dir = Path(self.tmp.name) / "repo"
        self.dir.mkdir()
        git(self.dir, "init", "-q")
        self.write("README.md", "hello\n")
        git(self.dir, "add", "README.md")
        git(self.dir, "commit", "-qm", "init")
        self.ids = Path(self.tmp.name) / "ids"

    def write(self, rel, text):
        p = self.dir / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text)

    def set_ids(self, text):
        self.ids.write_text(text)

    def run(self, *argv):
        env = dict(os.environ, RSI_OPERATOR_IDENTIFIERS_FILE=str(self.ids))
        cwd = os.getcwd()
        old = os.environ.get("RSI_OPERATOR_IDENTIFIERS_FILE")
        os.environ["RSI_OPERATOR_IDENTIFIERS_FILE"] = env["RSI_OPERATOR_IDENTIFIERS_FILE"]
        os.chdir(self.dir)
        out = io.StringIO()
        try:
            with contextlib.redirect_stdout(out):
                code = coi.main(["check", *argv])
        finally:
            os.chdir(cwd)
            if old is None:
                os.environ.pop("RSI_OPERATOR_IDENTIFIERS_FILE")
            else:
                os.environ["RSI_OPERATOR_IDENTIFIERS_FILE"] = old
        return code, out.getvalue()


class CheckOperatorIdentifiers(unittest.TestCase):
    def test_staged_addition_in_thoughts_is_flagged_without_echoing_the_value(self):
        r = Repo(self)
        r.set_ids(f"# operator contacts\n{EMAIL}\n")
        r.write("thoughts/shared/handoffs/h.md", f"line one\nContact: {EMAIL.upper()}\n")
        git(r.dir, "add", "thoughts")
        code, out = r.run("--staged")
        self.assertEqual(code, 1)
        self.assertIn("thoughts/shared/handoffs/h.md:2: operator identifier #1", out)
        self.assertNotIn(EMAIL.lower(), out.lower())

    def test_clean_staged_diff_passes(self):
        r = Repo(self)
        r.set_ids(f"{EMAIL}\n{PHONE}\n")
        r.write("a.md", "the operator's release contact (local config)\n")
        git(r.dir, "add", "a.md")
        self.assertEqual(r.run("--staged"), (0, ""))

    def test_range_flags_added_line_with_second_identifier(self):
        r = Repo(self)
        r.set_ids(f"{EMAIL}\n{PHONE}\n")
        r.write("a.rs", f'const P: &str = "{PHONE}";\n')
        git(r.dir, "add", "a.rs")
        git(r.dir, "commit", "-qm", "leak")
        code, out = r.run("--range", "HEAD~1..HEAD")
        self.assertEqual(code, 1)
        self.assertIn("a.rs:1: operator identifier #2", out)

    def test_pre_existing_lines_and_removals_are_not_flagged(self):
        r = Repo(self)
        r.write("a.md", f"old {EMAIL}\n")
        git(r.dir, "add", "a.md")
        git(r.dir, "commit", "-qm", "history")
        r.set_ids(f"{EMAIL}\n")
        r.write("a.md", "scrubbed\n")
        git(r.dir, "add", "a.md")
        self.assertEqual(r.run("--staged"), (0, ""))

    def test_missing_or_empty_list_is_a_noop(self):
        r = Repo(self)
        r.write("a.md", f"{EMAIL}\n")
        git(r.dir, "add", "a.md")
        self.assertEqual(r.run("--staged"), (0, ""))  # no list file at all
        r.set_ids("# only comments\n\n")
        self.assertEqual(r.run("--staged"), (0, ""))

    def test_unreadable_range_does_not_block(self):
        r = Repo(self)
        r.set_ids(f"{EMAIL}\n")
        self.assertEqual(r.run("--range", "deadbeef..cafebabe"), (0, ""))


if __name__ == "__main__":
    unittest.main()
