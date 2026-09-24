"""Hermetic tests for the accepted-source landing guard."""

import importlib.util
import contextlib
import io
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


SCRIPT = Path(__file__).resolve().parents[1] / "rolling-landing-guard.py"
sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location("rolling_landing_guard", SCRIPT)
GUARD = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(GUARD)


class LandingGuardTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="rsi-landing-guard-test-")
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name)
        self.git("init", "-q")
        self.git("config", "user.name", "Landing Guard Test")
        self.git("config", "user.email", "landing-guard@example.invalid")
        crate = self.repo / "crates" / "fixture"
        (crate / "src").mkdir(parents=True)
        (crate / "Cargo.toml").write_text('[package]\nname = "fixture"\nversion = "0.1.0"\n')
        self.file = crate / "src" / "lib.rs"
        self.file.write_text("old default\nobsolete line\nkeep\n")
        self.base = self.commit("base")

    def git(self, *args):
        return subprocess.check_output(["git", "-C", str(self.repo), *args], text=True).strip()

    def commit(self, message):
        self.git("add", "crates/fixture")
        self.git("commit", "-q", "-m", message)
        return self.git("rev-parse", "HEAD")

    def test_reports_exact_accepted_line_reverted_by_later_resolution(self):
        self.file.write_text("new default\nobsolete line\nkeep\n")
        source = self.commit("accepted default")
        self.file.write_text("old default\nobsolete line\nkeep\n")
        candidate = self.commit("bad resolution")
        self.assertTrue(GUARD.ancestor(self.repo, source, candidate))
        findings = GUARD.lost_hunks(self.repo, self.base, source, candidate)
        self.assertEqual(len(findings), 1)
        self.assertIn("restored base line 'old default'", findings[0])
        self.assertIn("replaced by accepted 'new default'", findings[0])
        self.assertEqual(GUARD.affected_crates(self.repo, source, candidate), ["fixture"])

    def test_accepts_unrelated_followup_and_selects_affected_crate(self):
        self.file.write_text("new default\nobsolete line\nkeep\n")
        source = self.commit("accepted default")
        other = self.repo / "crates" / "fixture" / "src" / "other.rs"
        other.write_text("pub fn other() {}\n")
        candidate = self.commit("followup")
        self.assertEqual(GUARD.lost_hunks(self.repo, self.base, source, candidate), [])
        self.assertEqual(GUARD.affected_crates(self.repo, source, candidate), ["fixture"])

    def test_detects_pure_deletion_restored(self):
        self.file.write_text("old default\nkeep\n")
        source = self.commit("accepted deletion")
        self.file.write_text("old default\nobsolete line\nkeep\n")
        candidate = self.commit("bad restoration")
        findings = GUARD.lost_hunks(self.repo, self.base, source, candidate)
        self.assertEqual(len(findings), 1)
        self.assertIn("restored base line 'obsolete line'", findings[0])

    def test_accepts_later_evolution_of_accepted_line(self):
        self.file.write_text("new default\nobsolete line\nkeep\n")
        source = self.commit("accepted default")
        self.file.write_text("newer default\nobsolete line\nkeep\n")
        candidate = self.commit("later evolution")
        self.assertEqual(GUARD.lost_hunks(self.repo, self.base, source, candidate), [])

    def test_accepts_later_rename_and_edit(self):
        self.file.write_text("new default\nobsolete line\nkeep\n")
        source = self.commit("accepted default")
        renamed = self.file.with_name("renamed.rs")
        self.git("mv", str(self.file), str(renamed))
        renamed.write_text("newer default\nobsolete line\nkeep\n")
        candidate = self.commit("later rename and evolution")
        self.assertEqual(GUARD.renamed_paths(self.repo, source, candidate),
                         {"crates/fixture/src/lib.rs": "crates/fixture/src/renamed.rs"})
        self.assertEqual(GUARD.lost_hunks(self.repo, self.base, source, candidate), [])

    def test_reports_rollback_after_rename_at_new_path(self):
        self.file.write_text("new default\nobsolete line\nkeep\n")
        source = self.commit("accepted default")
        renamed = self.file.with_name("renamed.rs")
        self.git("mv", str(self.file), str(renamed))
        renamed.write_text("old default\nobsolete line\nkeep\n")
        candidate = self.commit("renamed stale resolution")
        findings = GUARD.lost_hunks(self.repo, self.base, source, candidate)
        self.assertEqual(len(findings), 1)
        self.assertIn("renamed.rs:1: restored base line 'old default'", findings[0])

    def test_reports_missing_addition_with_unrelated_later_edit(self):
        self.file.write_text("old default\naccepted addition\nobsolete line\nkeep\n")
        source = self.commit("accepted addition")
        self.file.write_text("old default\nobsolete line\nkeep\nunrelated edit\n")
        candidate = self.commit("lost insertion and unrelated edit")
        findings = GUARD.lost_hunks(self.repo, self.base, source, candidate)
        self.assertEqual(len(findings), 1)
        self.assertIn("accepted insertion 'accepted addition'", findings[0])

    def test_identical_attribute_occurrences_survive_boundary_remapping(self):
        self.file.write_text("#[allow(clippy::unwrap_used)]\nfn first() {}\n"
                             "#[allow(clippy::unwrap_used)]\nfn second() {}\n"
                             "old default\nobsolete line\nkeep\n")
        source = self.commit("accepted duplicate attributes")
        self.file.write_text("fn prelude() {}\n#[allow(clippy::unwrap_used)]\n"
                             "fn first() {}\n#[allow(clippy::unwrap_used)]\n"
                             "fn second() {}\nold default\nobsolete line\nkeep\n")
        candidate = self.commit("unrelated prelude shifts boundary")
        self.assertEqual(GUARD.lost_hunks(self.repo, self.base, source, candidate), [])

    def test_new_duplicate_elsewhere_does_not_hide_lost_insertion(self):
        self.file.write_text("fn a() {}\nfn b() {}\nfn c() {}\n")
        base = self.commit("three functions")
        self.file.write_text("fn a() {}\n#[tokio::test]\nfn b() {}\nfn c() {}\n")
        source = self.commit("accepted attribute on b")
        self.file.write_text("fn a() {}\nfn b() {}\nfn c() {}\n"
                             "#[tokio::test]\nfn d() {}\n")
        candidate = self.commit("lost attribute on b, new attribute on d")
        findings = GUARD.lost_hunks(self.repo, base, source, candidate)
        self.assertEqual(len(findings), 1)
        self.assertIn("accepted insertion '#[tokio::test]'", findings[0])

    def test_lost_hunk_still_runs_affected_crate_tests(self):
        self.file.write_text("new default\nobsolete line\nkeep\n")
        source = self.commit("accepted default")
        self.file.write_text("old default\nobsolete line\nkeep\n")
        candidate = self.commit("lost accepted default")
        calls = []
        arguments = ["guard", "--repo", str(self.repo), "--base", self.base,
                     "--source", source, "--target", self.base,
                     "--candidate", candidate, "--worktree", str(self.repo)]
        with mock.patch.object(sys, "argv", arguments), \
             mock.patch.object(GUARD, "run_tests", side_effect=lambda *args: calls.append(args)), \
             contextlib.redirect_stdout(io.StringIO()) as output:
            result = GUARD.main()
        self.assertEqual(result, 1)
        self.assertEqual(len(calls), 1)
        self.assertIn('"tests_executed": true', output.getvalue())

    def test_accepts_evolved_insertion_at_same_boundary(self):
        self.file.write_text("old default\naccepted addition\nobsolete line\nkeep\n")
        source = self.commit("accepted addition")
        self.file.write_text("old default\nevolved addition\nobsolete line\nkeep\n")
        candidate = self.commit("evolved insertion")
        self.assertEqual(GUARD.lost_hunks(self.repo, self.base, source, candidate), [])

    def test_binary_evolution_passes_but_exact_rollback_fails(self):
        binary = self.file.with_name("asset.bin")
        binary.write_bytes(b"\x00base")
        base = self.commit("binary base")
        binary.write_bytes(b"\x00accepted")
        source = self.commit("accepted binary")
        binary.write_bytes(b"\x00evolved")
        evolved = self.commit("later binary evolution")
        self.assertEqual(GUARD.lost_hunks(self.repo, base, source, evolved), [])
        binary.write_bytes(b"\x00base")
        reverted = self.commit("binary rollback")
        findings = GUARD.lost_hunks(self.repo, base, source, reverted)
        self.assertEqual(len(findings), 1)
        self.assertIn("accepted binary edit reverted to base blob sha256:", findings[0])

    def test_filter_is_explicit_and_must_name_affected_crate(self):
        self.assertEqual(
            GUARD.test_commands(["fixture"], ["fixture=accepted_default"]),
            [["cargo", "test", "-p", "fixture", "--lib", "accepted_default"]],
        )
        with self.assertRaises(GUARD.GuardError):
            GUARD.test_commands(["fixture"], ["other=accepted_default"])


if __name__ == "__main__":
    unittest.main()
