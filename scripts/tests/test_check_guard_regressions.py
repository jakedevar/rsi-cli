"""Changed guards must retain production findings or declare exact losses (#520)."""

import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from scratch_git import scratch_git, scratch_tempdir

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts/check-guard-regressions.py"
SPEC = importlib.util.spec_from_file_location("guard_regressions", SCRIPT)
CHECK = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CHECK)
GUARD = "scripts/check-example.py"
ENTRIES = ('def entries(text):\n'
           '    return [(i, "flag") for i, line in enumerate(text.splitlines(), 1) if "FLAG" in line]\n')


class GuardRegressionTest(unittest.TestCase):
    def setUp(self):
        self.temp = scratch_tempdir(prefix="guard-regression-test-")
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name)
        self.git("init", "-q")
        self.git("config", "user.name", "Test")
        self.git("config", "user.email", "test@example.invalid")
        self.write(GUARD, ENTRIES)
        self.write("scripts/writer", "FLAG\n")
        self.base = self.commit("base")

    def git(self, *args):
        return scratch_git(self.repo, *args).stdout.strip()

    def write(self, path, text):
        target = self.repo / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text)

    def commit(self, message):
        self.git("add", ".")
        self.git("commit", "-qm", message)
        return self.git("rev-parse", "HEAD")

    def losses(self, head="HEAD"):
        return CHECK.compare(self.repo, self.base, head)

    def loosen(self):
        self.write(GUARD, "def entries(text):\n    return []\n")
        self.commit("loosen")

    def test_same_guard_and_unrelated_edits_report_nothing(self):
        self.write("scripts/writer", "FLAG\nFLAG\n")
        self.commit("writer")
        self.assertEqual(self.losses(), [])
        self.assertEqual(CHECK.compare(self.repo, self.base, "HEAD", [GUARD]), [])

    def test_both_versions_scan_committed_head_despite_dirty_worktree(self):
        self.write("scripts/writer", "heading\nFLAG\n")
        self.loosen()
        self.write("scripts/writer", "dirty\n")
        losses = self.losses()
        self.assertEqual([(x["path"], x["line"], x["finding"], x["intended"]) for x in losses],
                         [("scripts/writer", 2, "flag", False)])
        result = subprocess.run([sys.executable, str(SCRIPT), self.base, "HEAD", "--repo", str(self.repo)],
                                text=True, capture_output=True)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("lost production finding scripts/writer:2: flag (UNDECLARED)", result.stdout)

    def test_head_tree_ignores_archive_attributes(self):
        self.write(".gitattributes", "scripts/writer export-ignore\n")
        self.loosen()
        self.assertEqual([(x["path"], x["line"]) for x in self.losses()], [("scripts/writer", 1)])

    def test_test_files_and_inline_rust_test_items_are_excluded(self):
        self.write("scripts/tests/test_writer.py", "FLAG\n")
        self.write("crates/x/tests/writer.rs", "FLAG\n")
        self.write("crates/x/src/tests.rs", "FLAG\n")
        self.write("crates/x/src/e2e_tests/writer.rs", "FLAG\n")
        self.write("crates/x/src/writer.rs", '#[cfg(test)]\nmod tests {\n // FLAG\n fn f() { let s = r#"}"#; }\n}\n'
                   '#[test]\nfn fixture() { /* outer /* FLAG */ nested */ }\n'
                   'fn production() { /* FLAG */ }\n')
        self.loosen()
        self.assertEqual([(x["path"], x["line"]) for x in self.losses()],
                         [("crates/x/src/writer.rs", 8), ("scripts/writer", 1)])

    def test_unchanged_guard_runs_neither_version(self):
        self.write(GUARD, "raise RuntimeError('do not run unchanged')\n")
        same = self.commit("unsupported but unchanged")
        self.assertEqual(CHECK.compare(self.repo, same, same, [GUARD]), [])

    def test_new_guard_has_no_baseline_and_deleted_guard_loses_findings(self):
        self.write("tools/check-new.py", "def entries(text):\n    return []\n")
        self.commit("new guard")
        self.assertEqual(self.losses(), [])
        (self.repo / GUARD).unlink()
        self.commit("delete guard")
        losses = self.losses()
        self.assertEqual([(x["path"], x["head_guard"]) for x in losses], [("scripts/writer", None)])

    def test_each_declaration_needs_exact_finding_hashes_and_reason(self):
        self.loosen()
        declaration = {k: v for k, v in self.losses()[0].items() if k != "intended"}
        declaration["reason"] = "The matched name is a reviewed fixture alias."
        self.write(CHECK.DECLARATIONS, json.dumps([declaration]))
        self.commit("declare")
        self.assertTrue(self.losses()[0]["intended"])
        self.write(GUARD, "# another loosening\ndef entries(text):\n    return []\n")
        self.commit("later guard")
        self.assertFalse(self.losses()[0]["intended"])
        declaration["reason"] = " "
        self.write(CHECK.DECLARATIONS, json.dumps([declaration]))
        self.commit("bad reason")
        with self.assertRaisesRegex(ValueError, "each exact loss needs"):
            self.losses()

    def test_one_declaration_does_not_approve_another_loss(self):
        self.write("scripts/second", "FLAG\n")
        self.loosen()
        declaration = {k: v for k, v in self.losses()[0].items() if k != "intended"}
        declaration["reason"] = "Review approved this single fixture alias."
        self.write(CHECK.DECLARATIONS, json.dumps([declaration]))
        self.commit("one declared loss")
        losses = self.losses()
        self.assertEqual([x["intended"] for x in losses], [True, False])

    def test_unsupported_changed_guard_is_reported_and_passes(self):
        self.write(GUARD, "print('no adapter')\n")
        self.base = self.commit("unsupported base")
        self.write(GUARD, "print('still no adapter')\n")
        self.commit("unsupported head")
        result = subprocess.run([sys.executable, str(SCRIPT), self.base, "HEAD", "--repo", str(self.repo)],
                                text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"unsupported: {GUARD} (no entries() or stdout opt-in; not compared)", result.stderr)
        self.assertEqual(result.stdout, "")

    def test_removing_supported_adapter_still_fails(self):
        self.write(GUARD, "print('adapter removed')\n")
        self.commit("remove adapter")
        with self.assertRaisesRegex(ValueError, "needs entries"):
            self.losses()

    def test_unsupported_guard_does_not_hide_supported_loss_or_bad_declaration(self):
        unsupported = "tools/check-unsupported.py"
        self.write(unsupported, "raise RuntimeError('must not execute')\n")
        self.base = self.commit("unsupported base")
        self.write(unsupported, "raise RuntimeError('still must not execute')\n")
        self.loosen()
        command = [sys.executable, str(SCRIPT), self.base, "HEAD", "--repo", str(self.repo)]
        result = subprocess.run(command, text=True, capture_output=True)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn(f"unsupported: {unsupported}", result.stderr)
        self.assertIn("lost production finding scripts/writer:1: flag (UNDECLARED)", result.stdout)
        self.write(CHECK.DECLARATIONS, "{}")
        self.commit("malformed declaration")
        result = subprocess.run(command, text=True, capture_output=True)
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn(f"{CHECK.DECLARATIONS} must be a list", result.stderr)

    def test_stable_stdout_adapter_and_broken_runner(self):
        source = ('# guard-regressions: stdout\nimport sys\n'
                  'print("scripts/writer:1: flag")\nsys.exit(1)\n')
        self.write(GUARD, source)
        self.base = self.commit("stdout base")
        self.write(GUARD, '# guard-regressions: stdout\n')
        self.commit("stdout head")
        self.assertEqual(self.losses()[0]["finding"], "flag")
        self.write(GUARD, '# guard-regressions: stdout\nraise RuntimeError("broken")\n')
        self.commit("broken runner")
        with self.assertRaisesRegex(ValueError, "guard exited 1|unstable guard diagnostic"):
            self.losses()

    def test_head_can_opt_both_versions_into_stdout_adapter(self):
        self.write(GUARD, 'import sys\nprint("scripts/writer:1: flag")\nsys.exit(1)\n')
        self.base = self.commit("stable output before adapter existed")
        self.write(GUARD, '# guard-regressions: stdout\n')
        self.commit("opt in while loosening")
        self.assertEqual(self.losses()[0]["finding"], "flag")

    def test_guards_cannot_share_import_state(self):
        source = ('import builtins\n'
                  'if hasattr(builtins, "guard_test_marker"): raise RuntimeError("leaked")\n'
                  'builtins.guard_test_marker = True\n' + ENTRIES)
        self.write(GUARD, source)
        self.base = self.commit("isolated base")
        self.write(GUARD, source + "# changed\n")
        self.commit("isolated head")
        self.assertEqual(self.losses(), [])

    def test_scoped_test_wires_exact_refs_to_actionable_static_receipt(self):
        source = (ROOT / "scripts/scoped-test").read_text()
        self.assertIn('"guard_regressions": [sys.executable, "scripts/check-guard-regressions.py", base, head]', source)


class RustTestExclusionTest(unittest.TestCase):
    def test_strings_comments_and_lifetimes_do_not_change_item_bounds(self):
        source = ('// #[cfg(test)]\nfn production() {}\n'
                  '#[cfg(test)]\nmod tests {\n'
                  ' fn f(s: &\'static str) { let s = r##"}"##; /* { /* } */ } */ }\n}\n'
                  'fn still_production() {}\n')
        self.assertEqual(CHECK.rust_test_lines(source), {3, 4, 5, 6})

    def test_file_level_test_cfg_and_shared_production_line(self):
        self.assertEqual(CHECK.rust_test_lines('#![cfg(test)]\nfn f() {}\n'), {1, 2, 3})
        hidden = CHECK.rust_test_lines('#[cfg(test)]\nmod tests {} fn production() {}\n')
        self.assertEqual(hidden, {1})
        self.assertEqual(CHECK.rust_test_lines('fn production() {} #[test] fn fixture() {}\n'), set())
        self.assertEqual(CHECK.rust_test_lines('mod tests { #![cfg(test)] fn fixture() {} }\nfn production() {}\n'), set())


if __name__ == "__main__":
    unittest.main()
