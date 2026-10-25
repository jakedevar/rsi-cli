"""Hermetic tests for the accepted-source landing guard."""

import importlib.util
import contextlib
import io
import json
from pathlib import Path
import subprocess
import sys
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from scratch_git import scratch_git, scratch_tempdir  # noqa: E402

SCRIPT = Path(__file__).resolve().parents[1] / "rolling-landing-guard.py"
sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location("rolling_landing_guard", SCRIPT)
GUARD = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(GUARD)


class LandingGuardTest(unittest.TestCase):
    def setUp(self):
        self.temp = scratch_tempdir(prefix="rsi-landing-guard-test-")
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
        return scratch_git(self.repo, *args).stdout.strip()

    def commit(self, message):
        self.git("add", "crates/fixture")
        self.git("commit", "-q", "-m", message)
        return self.git("rev-parse", "HEAD")

    def test_empty_accepted_delta_reports_already_on_rolling(self):
        args = [str(SCRIPT), "--repo", str(self.repo), "--base", self.base,
                "--source", self.base, "--target", self.base,
                "--candidate", self.base, "--plan-only"]
        output = io.StringIO()
        with mock.patch.object(sys, "argv", args), contextlib.redirect_stdout(output):
            self.assertEqual(GUARD.main(), 0)
        report = json.loads(output.getvalue())
        self.assertEqual(report["outcome"], "already_on_rolling")
        self.assertEqual(report["candidate"], self.base)
        self.assertEqual(report["lost_hunks"], [])

    def test_target_writer_static_guard_reads_the_candidate_even_in_plan_only(self):
        root = SCRIPT.parents[1]
        for relative in ("scripts/check-target-reclaim.py", "scripts/target-reclaim-retained.json",
                         "crates/rsid-store/src/sandbox/target_reclaim.rs",
                         "crates/rsi-common/src/sandbox_storage.rs"):
            path = self.repo / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text((root / relative).read_text())
            self.git("add", relative)
        self.git("commit", "-q", "-m", "install target writer guard")
        good = self.git("rev-parse", "HEAD")
        GUARD.check_target_reclaim(self.repo, good)
        writer = self.repo / "desktop/scripts/build.sh"
        writer.parent.mkdir(parents=True)
        writer.write_text("mkdir -p target/new-thing\n")
        self.git("add", "desktop/scripts/build.sh")
        self.git("commit", "-q", "-m", "unregistered target writer")
        candidate = self.git("rev-parse", "HEAD")
        writer.write_text("mkdir -p target/.rsi-tmp/new-thing\n")
        # main's plan-only path is the lander's pre-build policy check.
        args = [str(SCRIPT), "--repo", str(self.repo), "--base", good,
                "--source", candidate, "--target", good,
                "--candidate", candidate, "--plan-only"]
        output = io.StringIO()
        with mock.patch.object(sys, "argv", args), contextlib.redirect_stderr(output):
            self.assertEqual(GUARD.main(), 2)
        self.assertIn("desktop/scripts/build.sh:1: target/new-thing", output.getvalue())
        # Checking the older good commit still works with the bad worktree.
        writer.write_text("mkdir -p target/new-other\n")
        GUARD.check_target_reclaim(self.repo, good)

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

    def test_filter_is_explicit_and_may_target_readers(self):
        self.assertEqual(
            GUARD.test_commands(["fixture"], ["fixture=accepted_default"]),
            [["cargo", "test", "-p", "fixture", "--lib", "accepted_default"]],
        )
        with self.assertRaises(GUARD.GuardError):
            GUARD.test_commands(["fixture"], ["fixture="])
        # A reader of a changed doc may live in a crate the diff did not touch,
        # and bin:/test: filters select a cargo target.
        self.assertEqual(
            GUARD.test_commands(["fixture"], ["other=bin:tool", "other=test:corpus"]),
            [["cargo", "test", "-p", "fixture", "--lib"],
             ["cargo", "test", "-p", "other", "--bin", "tool"],
             ["cargo", "test", "-p", "other", "--test", "corpus"]],
        )

    def stale_merge_scenario(self):
        """A source that merged an older rolling state that rolling then renamed."""
        trunk = self.git("rev-parse", "--abbrev-ref", "HEAD")
        other = self.file.with_name("other.rs")
        other.write_text("fn first() {}\nfn last() {}\n")
        self.base = self.commit("base with other file")
        self.git("switch", "-q", "-c", "source")
        self.file.write_text("new default\nobsolete line\nkeep\n")
        self.commit("source own change")
        self.git("switch", "-q", trunk)
        other.write_text("fn first() {}\nfn test_stale_lock_is_preserved() {}\nfn last() {}\n")
        self.commit("rolling adds a test")
        self.git("switch", "-q", "source")
        self.git("merge", "-q", "--no-ff", "-m", "merge older rolling", trunk)
        source = self.git("rev-parse", "HEAD")
        self.git("switch", "-q", trunk)
        other.write_text("fn first() {}\nfn last() {}\n"
                         "fn test_stale_lock_is_preserved_and_cargo_runs() {}\n")
        target = self.commit("rolling renames the test on purpose")
        self.git("merge", "-q", "--no-ff", "-m", "land source", source)
        return source, target, self.git("rev-parse", "HEAD")

    def run_guard(self, source, target, candidate):
        args = ["guard", "--repo", str(self.repo), "--base", self.base,
                "--source", source, "--target", target,
                "--candidate", candidate, "--plan-only"]
        output = io.StringIO()
        with mock.patch.object(sys, "argv", args), contextlib.redirect_stdout(output):
            code = GUARD.main()
        return code, json.loads(output.getvalue())

    def test_merged_in_rolling_content_later_renamed_is_not_a_reversion(self):
        source, target, candidate = self.stale_merge_scenario()
        # Measured from the stale base alone, the merged-in test reads as the
        # source's own insertion that the candidate lost.
        stale = GUARD.lost_hunks(self.repo, self.base, source, candidate)
        self.assertEqual(len(stale), 1)
        self.assertIn("accepted insertion 'fn test_stale_lock_is_preserved() {}'", stale[0])
        code, report = self.run_guard(source, target, candidate)
        self.assertEqual(code, 0)
        self.assertEqual(report["lost_hunks"], [])

    def test_real_reversion_is_still_refused_after_stale_merge(self):
        source, target, candidate = self.stale_merge_scenario()
        self.file.write_text("old default\nobsolete line\nkeep\n")
        candidate = self.commit("bad resolution reverts the source's own change")
        code, report = self.run_guard(source, target, candidate)
        self.assertEqual(code, 1)
        self.assertEqual(len(report["lost_hunks"]), 1)
        self.assertIn("restored base line 'old default'", report["lost_hunks"][0])

    def criss_cross_scenario(self):
        """Source and rolling merge each other's earlier state (two merge bases),
        then rolling rewraps both merged-in lines (rustfmt) before the landing."""
        trunk = self.git("rev-parse", "--abbrev-ref", "HEAD")
        other = self.file.with_name("other.rs")
        extra = self.file.with_name("extra.rs")
        other.write_text("use a::{\n    a1, a2, a3,\n    a4, a5,\n};\n")
        extra.write_text("use b::{\n    b1, b2, b3,\n    b4, b5,\n};\n")
        self.git("add", "crates/fixture")
        self.base = self.commit("base with two files")
        self.git("switch", "-q", "-c", "source")
        extra.write_text("use b::{\n    b1, b2, b3, b0,\n    b4, b5,\n};\n")
        s1 = self.commit("source adds a line shared with rolling")
        self.git("switch", "-q", trunk)
        other.write_text("use a::{\n    a1, a2, a3, a0,\n    a4, a5,\n};\n")
        m1 = self.commit("rolling adds a line")
        self.git("merge", "-q", "--no-ff", "-m", "rolling takes the source line", s1)
        self.git("switch", "-q", "source")
        self.git("merge", "-q", "--no-ff", "-m", "source takes the rolling line", m1)
        self.file.write_text("new default\nobsolete line\nkeep\n")
        source = self.commit("source own change")
        self.git("switch", "-q", trunk)
        other.write_text("use a::{\n    a1, a2, a3,\n    a4, a5, a0,\n};\n")
        extra.write_text("use b::{\n    b1, b2, b3,\n    b4, b5, b0,\n};\n")
        target = self.commit("rolling rewraps both lines")
        self.git("merge", "-q", "--no-ff", "-m", "land source", source)
        return s1, m1, source, target, self.git("rev-parse", "HEAD")

    def test_criss_cross_source_is_measured_against_its_virtual_merge_base(self):
        s1, m1, source, target, candidate = self.criss_cross_scenario()
        bases = set(self.git("merge-base", "--all", source, target).split())
        self.assertEqual(bases, {s1, m1})
        # Either merge base alone reads the other's merged-in line as the
        # source's own insertion that the rewrap "restored".
        for single in (s1, m1):
            self.assertTrue(GUARD.lost_hunks(self.repo, single, source, candidate), single)
        narrowed = GUARD.own_base(self.repo, self.base, source, target)
        self.assertNotIn(narrowed, (self.base, s1, m1))
        code, report = self.run_guard(source, target, candidate)
        self.assertEqual(code, 0)
        self.assertEqual(report["lost_hunks"], [])

    def test_real_reversion_is_still_refused_with_several_merge_bases(self):
        _s1, _m1, source, target, _candidate = self.criss_cross_scenario()
        self.file.write_text("old default\nobsolete line\nkeep\n")
        candidate = self.commit("bad resolution reverts the source's own change")
        code, report = self.run_guard(source, target, candidate)
        self.assertEqual(code, 1)
        self.assertEqual(len(report["lost_hunks"]), 1)
        self.assertIn("restored base line 'old default'", report["lost_hunks"][0])

    def test_own_base_never_widens_the_accepted_delta(self):
        self.file.write_text("new default\nobsolete line\nkeep\n")
        source = self.commit("accepted default")
        # An already-integrated source keeps its full base-to-source check.
        self.assertEqual(GUARD.own_base(self.repo, self.base, source, source), self.base)
        # Base already equals the shared state: unchanged.
        self.assertEqual(GUARD.own_base(self.repo, self.base, source, self.base), self.base)
        # A base that is not an ancestor of the shared state stands.
        self.assertEqual(GUARD.own_base(self.repo, source, source, self.base), source)

    def test_toml_parser_falls_back_to_tomli_then_fails_clearly(self):
        real = GUARD.importlib.import_module
        tomli = mock.Mock()

        def without_tomllib(name, *args, **kwargs):
            if name == "tomllib":
                raise ModuleNotFoundError("No module named 'tomllib'", name="tomllib")
            if name == "tomli":
                return tomli
            return real(name, *args, **kwargs)

        with mock.patch.object(GUARD.importlib, "import_module", without_tomllib):
            self.assertIs(GUARD.load_toml_module(), tomli)

        def without_any(name, *args, **kwargs):
            if name in ("tomllib", "tomli"):
                raise ModuleNotFoundError(f"No module named '{name}'", name=name)
            return real(name, *args, **kwargs)

        with mock.patch.object(GUARD.importlib, "import_module", without_any):
            with self.assertRaisesRegex(GUARD.GuardError, "Python >= 3.11 .*tomli"):
                GUARD.load_toml_module()


if __name__ == "__main__":
    unittest.main()
