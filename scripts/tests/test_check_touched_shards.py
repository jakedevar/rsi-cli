"""Intent checks for scripts/check-touched-shards (#1099): one typed receipt per candidate."""

import importlib.util
import json
import os
import re
from pathlib import Path
import shutil
import runpy
import signal
import subprocess
import sys
import time
import unittest
from unittest import mock


sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from scratch_git import scratch_git, scratch_tempdir  # noqa: E402

SCRIPT = Path(__file__).resolve().parents[1] / "check-touched-shards"


MODE_FLAGS = {"--compile", "--no-compile", "--filters-only"}


def with_mode(args):
    """The arguments with the verifier's compile mode spelled out when no mode was given (#2564).

    A bare run is refused; the tests that exercise the compiling verifier request it, as
    the supervised `check-touched-shards` recipe does."""
    args = list(args)
    return args if MODE_FLAGS.intersection(args) else ["--compile", *args]


def without_progress(stderr):
    """stderr minus the `check-touched-shards: [ 1.2s] ...` phase lines (#1657)."""
    return "".join(line for line in stderr.splitlines(keepends=True) if not line.startswith("check-touched-shards: ["))
GATE = '#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-{shard}"))]\n#[test]\nfn {name}() {{}}\n'


class CheckTouchedShardsTest(unittest.TestCase):
    def setUp(self):
        self.temp = scratch_tempdir(prefix="touched-shards-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.git("init", "-q", "-b", "rolling")
        self.git("config", "user.name", "T")
        self.git("config", "user.email", "t@example.invalid")
        self.write("crates/rsid/src/store/migrations/v150.rs", "// v150\n")
        self.write("crates/rsid/src/foo.rs", GATE.format(shard="store-02", name="a"))
        self.write("crates/rsid/src/bar.rs", "pub fn bar() {}\n")
        self.write("crates/rsid/src/bar/tests.rs", GATE.format(shard="other-03", name="b"))
        self.write("crates/rsi-common/src/types.rs", "pub fn t() {}\n#[cfg(test)]\nmod tests { #[test] fn a() {} }\n")
        self.write("crates/rsi-common/src/untested.rs", "pub fn u() {}\n")
        self.write("crates/rsid/src/bin/tool.rs", "fn main() {}\n#[test]\nfn t() {}\n")
        self.write("crates/rsid/tests/it.rs", "#[test]\nfn t() {}\n")
        self.write("scripts/desktop-test-gate.py", "# the desktop lane's selector (stub)\n")
        self.git("add", ".")
        self.git("commit", "-q", "-m", "base")
        self.git("branch", "base")
        self.git("checkout", "-q", "-b", "feature")

    def git(self, *args):
        scratch_git(self.root, *args)

    def write(self, rel, text):
        path = self.root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def commit(self, msg="change"):
        self.git("add", ".")
        self.git("commit", "-q", "-m", msg)

    def check(self, *extra, cargo="true"):
        env = dict(os.environ, CHECK_TOUCHED_SHARDS_CARGO=cargo)
        result = subprocess.run(
            [str(SCRIPT), *with_mode(["--repo", str(self.root), "--base", "base", *extra])],
            capture_output=True, text=True, env=env,
        )
        summary, _, rest = result.stdout.partition("\n")
        return result.returncode, summary, json.loads(rest)

    def test_shard_named_only_in_a_fixture_string_is_not_compiled(self):
        text = GATE.format(shard="store-02", name="real") + (
            '#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]\n'
            "#[test]\nfn fixture() {\n"
            '    let gate = "feature = \\"test-shard-other-03\\"";\n'
            "}\n"
        )
        self.write("crates/rsid/src/foo.rs", text)
        self.commit()
        code, summary, receipt = self.check("--no-compile")
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["shards"]["map"], {"store-02": ["crates/rsid/src/foo.rs"]})

    def test_a_desktop_only_diff_selects_the_desktop_lane_and_no_cargo_filter(self):
        # Hub #3001: desktop/ is its own Tauri workspace, so no shard filter covers it; the
        # receipt names the lane instead and a diff elsewhere never selects it.
        self.write("desktop/src/model.ts", "export const a = 1;\n")
        self.write("desktop/src-tauri/src/lib.rs", "pub fn a() {}\n")
        self.commit()
        code, summary, receipt = self.check("--filters-only")
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["desktop"], {
            "selected": True, "total": 2,
            "paths": ["desktop/src-tauri/src/lib.rs", "desktop/src/model.ts"]})
        self.assertEqual((receipt["suggested_filters"], receipt["shards"]["map"]), ([], {}))

    def test_a_diff_outside_desktop_does_not_select_the_desktop_lane(self):
        self.write("crates/rsi-common/src/types.rs", "pub fn t() { let _x = 1; }\n#[cfg(test)]\nmod tests { #[test] fn a() {} }\n")
        self.write("docs/desktop/notes.md", "# notes\n")
        self.commit()
        code, summary, receipt = self.check("--filters-only")
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["desktop"], {"selected": False, "total": 0, "paths": []})
        self.assertEqual(receipt["suggested_filters"], ["rsi-common=types"])

    def test_a_mixed_diff_keeps_its_cargo_filters_and_adds_the_desktop_lane(self):
        self.write("crates/rsi-common/src/types.rs", "pub fn t() { let _x = 1; }\n#[cfg(test)]\nmod tests { #[test] fn a() {} }\n")
        self.write("desktop/package.json", "{}\n")
        self.commit()
        code, summary, receipt = self.check("--filters-only")
        self.assertEqual(code, 0, summary)
        self.assertEqual((receipt["desktop"]["selected"], receipt["desktop"]["paths"]), (True, ["desktop/package.json"]))
        self.assertEqual(receipt["suggested_filters"], ["rsi-common=types"])

    def test_the_desktop_selection_agrees_with_the_desktop_test_gate(self):
        gate = runpy.run_path(str(SCRIPT.with_name("desktop-test-gate.py")), run_name="desktop_test_gate")
        selects = runpy.run_path(str(SCRIPT), run_name="check_touched_shards")["desktop_lane"]
        for changed in ([], ["desktop/a"], ["docs/desktop/a"], ["desktop-notes.md"], ["x", "desktop/b/c"]):
            self.assertEqual(selects(changed)["selected"], gate["selected"](changed), changed)

    def test_a_keybindings_or_tui_source_or_path_dependency_diff_selects_the_desktop_lane(self):
        # Hub #3042: the trigger set is what the desktop suite reads, not `desktop/` alone. The path
        # dependencies come from the candidate's own manifest, so a new one needs no selector edit.
        self.write("desktop/src-tauri/Cargo.toml", '[package]\nname = "d"\n[dependencies]\n'
                   'rsi-graph = { path = "../../crates/rsi-graph" }\n')
        self.write("crates/rsi-graph/src/lib.rs", "pub fn g() {}\n")
        self.write("crates/rsi/src/keybindings.rs", "// keys\n")
        self.write("docs/keybindings.md", "# keys\n")
        self.commit()
        for name, text in (("docs/keybindings.md", "# keys changed\n"),
                           ("crates/rsi/src/keybindings.rs", "// keys changed\n"),
                           ("crates/rsi-graph/src/lib.rs", "pub fn g() { let _x = 1; }\n")):
            self.git("branch", "-f", "base", "HEAD")
            self.write(name, text)
            self.commit()
            code, summary, receipt = self.check("--filters-only")
            self.assertEqual(code, 0, summary)
            self.assertEqual(receipt["desktop"], {"selected": True, "total": 1, "paths": [name]}, name)
        self.git("branch", "-f", "base", "HEAD")
        self.write("crates/rsid/src/bar.rs", "pub fn bar() { let _x = 1; }\n")
        self.commit()
        code, summary, receipt = self.check("--filters-only")
        self.assertEqual((code, receipt["desktop"]["selected"]), (0, False), summary)

    def test_a_candidate_without_the_desktop_selector_fails_closed_only_when_its_diff_needs_it(self):
        # #355: the selector script decides the desktop lane, so its absence is not an empty selection.
        self.git("rm", "-q", "scripts/desktop-test-gate.py")
        self.commit("delete the selector")
        code, summary, receipt = self.check("--filters-only")
        self.assertEqual(code, 1, summary)
        self.assertTrue(receipt["desktop"]["selected"])
        self.assertIn("scripts/desktop-test-gate.py is missing from the candidate", receipt["desktop"]["error"])
        # Missing in base and candidate alike: a desktop change fails, an unrelated one passes.
        self.git("branch", "-f", "base", "HEAD")
        self.write("docs/notes.md", "# notes\n")
        self.commit("unrelated")
        code, summary, receipt = self.check("--filters-only")
        self.assertEqual((code, receipt["desktop"]), (0, {"selected": False, "total": 0, "paths": []}), summary)
        self.write("desktop/src/model.ts", "export const a = 1;\n")
        self.commit("desktop")
        code, summary, receipt = self.check("--filters-only")
        self.assertEqual(code, 1, summary)
        self.assertIn("error", receipt["desktop"])

    def criss_cross(self):
        """rolling and the worker branch each merged the other's earlier commit (#1633).

        `git merge-base --all base feature` names two bases, `base~1` and the
        worker's first commit; either alone makes the diff include a file the
        other side already holds. The real candidate diff is only the last commit.
        """
        self.write("crates/rsid/src/beta.rs", GATE.format(shard="store-02", name="beta"))
        self.commit("worker first")
        worker_first = scratch_git(self.root, "rev-parse", "HEAD").stdout.strip()
        self.git("checkout", "-q", "base")
        self.write("crates/rsid/src/alpha.rs", GATE.format(shard="other-03", name="alpha"))
        self.commit("rolling first")
        rolling_first = scratch_git(self.root, "rev-parse", "HEAD").stdout.strip()
        self.git("merge", "-q", "--no-ff", "-m", "rolling takes the worker's first commit", worker_first)
        self.git("checkout", "-q", "feature")
        self.git("merge", "-q", "--no-ff", "-m", "worker takes rolling's first commit", rolling_first)
        self.write("crates/rsi-common/src/types.rs", "pub fn t() { let _x = 1; }\n#[cfg(test)]\nmod tests { #[test] fn a() {} }\n")
        self.commit("worker last")

    def test_several_merge_bases_select_only_the_real_candidate_diff(self):
        # #1633: `git merge-base` picks one of two bases, so the diff re-counted
        # files the other side landed; the diff against the base tip is the truth.
        self.criss_cross()
        bases = scratch_git(self.root, "merge-base", "--all", "base", "feature").stdout.split()
        self.assertEqual(len(bases), 2, bases)
        code, summary, receipt = self.check("--no-compile")
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["changed_files"], 1)
        self.assertEqual(receipt["shards"]["map"], {})
        self.assertEqual(receipt["suggested_filters"], ["rsi-common=types"])
        self.assertEqual(sorted(receipt["merge_bases"]), sorted(bases))
        self.assertEqual(receipt["merge_base"], receipt["base"])

    def test_several_merge_bases_diff_the_merge_result_not_the_stale_side(self):
        # #2814: rolling moved on from a file the branch merged earlier, so the
        # branch still carries the older bytes. The merge result keeps rolling's
        # version, so only the branch's docs change is the candidate: docs-only.
        self.criss_cross()
        self.git("reset", "-q", "--hard", "HEAD~1")  # drop "worker last" (Rust change)
        self.write("docs/note.md", "# note\n")
        self.commit("worker docs")
        self.git("checkout", "-q", "base")
        self.write("crates/rsid/src/alpha.rs", GATE.format(shard="other-03", name="alpha2"))
        self.write("crates/rsid/src/beta.rs", GATE.format(shard="store-02", name="beta2"))
        self.commit("rolling moves on")
        bases = scratch_git(self.root, "merge-base", "--all", "base", "feature").stdout.split()
        self.assertEqual(len(bases), 2, bases)
        code, summary, receipt = self.check("--no-compile", "--head", "feature")
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["changed_files"], 1)
        self.assertEqual(receipt["shards"]["map"], {})
        self.assertEqual(receipt["suggested_filters"], [])

    def test_one_merge_base_keeps_the_merge_base_diff(self):
        self.write("crates/rsi-common/src/types.rs", "pub fn t() { let _x = 1; }\n#[cfg(test)]\nmod tests { #[test] fn a() {} }\n")
        self.commit()
        code, summary, receipt = self.check("--no-compile")
        self.assertEqual(code, 0, summary)
        self.assertEqual(len(receipt["merge_bases"]), 1)
        self.assertEqual(receipt["merge_base"], receipt["merge_bases"][0])

    def rewind_fixture(self):
        path = "crates/rsid-store/src/store/tests.rs"
        helpers = """const REWIND_TEARDOWN_COVERED_THROUGH: i32 = 156;
fn rewind_post_v121_tail_to() {
    old_tail();
}
fn assert_fixture_matches_claimed_version() {
    old_catalog();
}
fn assert_post_v77_chain_replayed() {
    old_catalog();
}
fn indirect_fixture() {
    rewind_store_to_schema_version();
}
"""
        tests = ""
        for name, call in [("migration_old", "rewind_store_to_schema_version"),
                           ("migration_tail", "rewind_post_v121_tail_to"),
                           ("migration_replay", "assert_post_v77_chain_replayed"),
                           ("migration_indirect", "indirect_fixture"),
                           ("schema_rewinds_correctly", "unrelated"),
                           ("fixture_guard_rejects_drift", "unrelated"),
                           ("ordinary_store_test", "unrelated")]:
            tests += GATE.format(shard="store-01", name=name).replace("{}", "{\n    " + call + "();\n}")
        self.write(path, helpers + tests)
        # Cross-module callers must retain their own shard.
        self.write("crates/rsid-store/src/store/other.rs",
                   GATE.format(shard="store-02", name="external_migration").replace(
                       "{}", "{\n    rewind_post_v121_tail_to();\n}"))
        self.commit("rewind fixtures")
        self.git("branch", "-f", "base")
        return path, helpers + tests

    def test_rewind_helper_only_diff_selects_callers_and_fixture_guards(self):
        path, original = self.rewind_fixture()
        self.write(path, original.replace("156;", "157;").replace("old_tail();", "new_tail();")
                   .replace("old_catalog();", "new_catalog();"))
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(set(receipt["suggested_filters"]), {
            f"rsid=shard:store-01:test({name})" for name in (
                "migration_old", "migration_tail", "migration_replay", "migration_indirect",
                "schema_rewinds_correctly", "fixture_guard_rejects_drift")
        } | {"rsid=shard:store-02:test(external_migration)"})

    def additive_rewind_change(self, migration):
        path, original = self.rewind_fixture()
        pins = "".join(
            GATE.format(shard=shard, name=name).replace("{}", "{\n    rewind_post_v121_tail_to();\n}")
            for shard, name in (("store-01", "every_recovered_migration_step_actually_executes"),
                                ("store-01", "queue_fixture_rewinds_v150"),
                                ("store-02", "previous_schema_upgrades_to_the_live_indexes")))
        self.write(path, original + pins)
        self.commit("pins")
        self.git("branch", "-f", "base")
        self.write("crates/rsid-store/src/store/migrations/v151.rs", migration)
        self.write(path, (original + pins).replace("156;", "157;").replace("old_tail();", "new_tail();"))
        self.commit()
        return self.check()

    def test_additive_migration_selects_only_the_chain_pins(self):
        code, summary, receipt = self.additive_rewind_change(
            "impl Store {\n    fn migrate_v151(&self) {\n"
            "        add_column_if_not_exists_tx(&tx, \"t\", \"c\", \"TEXT\");\n"
            "        // DROP is only a comment here\n    }\n}\n")
        self.assertEqual(code, 0, summary)
        self.assertEqual(set(receipt["suggested_filters"]), {
            "rsid=shard:store-01:test(every_recovered_migration_step_actually_executes)",
            "rsid=shard:store-01:test(queue_fixture_rewinds_v150)",
            "rsid=shard:store-02:test(previous_schema_upgrades_to_the_live_indexes)"})

    def test_a_slice_numbered_after_a_declared_pending_sibling_is_not_a_conflict(self):
        """#1285: v152 over head v150 conflicts unless a declaration names v151 as pending."""
        self.write("crates/rsid-store/src/store/migrations/v152.rs", "// v152\n")
        self.commit("slice")
        code, summary, receipt = self.check()
        self.assertTrue(receipt["migrations"]["numbering_conflict"], summary)
        self.assertIn(" RENUMBER", summary)
        declaration = {"schema_version": 1, "version": 152, "files": [], "pending_predecessors": [151]}
        self.write("tools/provisional-migrations/issue-1285-gap.json", json.dumps(declaration))
        self.commit("declare gap")
        code, summary, receipt = self.check()
        self.assertFalse(receipt["migrations"]["numbering_conflict"], summary)
        self.assertEqual(receipt["migrations"]["pending_predecessors"], [151])
        self.assertNotIn(" RENUMBER", summary)

    def test_a_declared_gap_does_not_excuse_a_wrong_first_number(self):
        self.write("crates/rsid-store/src/store/migrations/v153.rs", "// v153\n")
        declaration = {"schema_version": 1, "version": 153, "files": [], "pending_predecessors": [151]}
        self.write("tools/provisional-migrations/issue-1285-gap.json", json.dumps(declaration))
        self.commit("slice")
        code, summary, receipt = self.check()
        self.assertTrue(receipt["migrations"]["numbering_conflict"], summary)

    def test_a_migration_only_diff_selects_the_catalog_replay_pins(self):
        # #2029: the replay test lives outside tests.rs and is named by no pattern.
        self.rewind_fixture()
        replay = GATE.format(shard="store-03", name="migration_replays_from_previous_head").replace(
            "{}", "{\n    rewind_post_v121_tail_to();\n    let _ = \"SELECT sql FROM sqlite_master\";\n}")
        self.write("crates/rsid-store/src/store/custody/tests.rs", replay)
        self.commit("replay pin")
        self.git("branch", "-f", "base")
        self.write("crates/rsid-store/src/store/migrations/v151.rs",
                   "impl Store {\n    fn migrate_v151(&self) {\n"
                   "        add_column_if_not_exists_tx(&tx, \"t\", \"c\", \"TEXT\");\n    }\n}\n")
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertIn("rsid=shard:store-03:test(migration_replays_from_previous_head)", receipt["suggested_filters"])
        self.assertNotIn("rsid=shard:store-01:test(migration_old)", receipt["suggested_filters"])

    def test_data_rewriting_migration_keeps_every_rewind_caller(self):
        code, summary, receipt = self.additive_rewind_change(
            "impl Store {\n    fn migrate_v151(&self) {\n"
            "        add_column_if_not_exists_tx(&tx, \"t\", \"c\", \"TEXT\");\n"
            "        tx.execute(\"UPDATE t SET c = 'x'\", []);\n    }\n}\n")
        self.assertEqual(code, 0, summary)
        self.assertIn("rsid=shard:store-01:test(migration_old)", receipt["suggested_filters"])
        self.assertIn("rsid=shard:store-02:test(external_migration)", receipt["suggested_filters"])

    def test_isolated_new_table_selects_only_chain_pins(self):
        code, summary, receipt = self.additive_rewind_change(
            'impl Store {\n    fn migrate_v151(&self) {\n'
            '        tx.execute_batch("CREATE TABLE turns(id TEXT PRIMARY KEY, state TEXT);'
            " CREATE INDEX turns_state ON turns(state);"
            " CREATE TRIGGER turns_guard BEFORE UPDATE ON turns WHEN NEW.state='bad'"
            " BEGIN SELECT RAISE(ABORT,'bad state'); END;\");\n"
            '        tx.pragma_update(None, "user_version", 151);\n    }\n}\n')
        self.assertEqual(code, 0, summary)
        self.assertEqual(set(receipt["suggested_filters"]), {
            "rsid=shard:store-01:test(every_recovered_migration_step_actually_executes)",
            "rsid=shard:store-01:test(queue_fixture_rewinds_v150)",
            "rsid=shard:store-02:test(previous_schema_upgrades_to_the_live_indexes)"})

    def test_imported_journal_migration_with_foreign_keys_keeps_only_chain_pins(self):
        migration = ('//! Journal\nuse super::super::Store;\nuse crate::error::Result;\n'
                     'use rusqlite::{Transaction, TransactionBehavior};\n'
                     'impl Store {\n    pub(super) fn migrate_v151(&self, version: i32) -> Result<()> {\n'
                     '        if version < 151 {\n'
                     '            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;\n'
                     '            tx.execute_batch("CREATE TABLE turns(id TEXT PRIMARY KEY,'
                     ' session_id TEXT REFERENCES sessions(id) ON DELETE RESTRICT,'
                     " link TEXT DEFAULT 'https://example.invalid/journal');"
                     ' CREATE INDEX turns_session ON turns(session_id);'
                     ' CREATE TRIGGER turns_identity BEFORE UPDATE ON turns WHEN NEW.id IS NOT OLD.id'
                     " BEGIN SELECT RAISE(ABORT,'immutable'); END;\");\n"
                     '            tx.pragma_update(None, "user_version", 151)?;\n'
                     '            tx.commit()?;\n        }\n        Ok(())\n    }\n}\n')
        code, summary, receipt = self.additive_rewind_change(migration)
        self.assertEqual(code, 0, summary)
        self.assertEqual(set(receipt["suggested_filters"]), {
            "rsid=shard:store-01:test(every_recovered_migration_step_actually_executes)",
            "rsid=shard:store-01:test(queue_fixture_rewinds_v150)",
            "rsid=shard:store-02:test(previous_schema_upgrades_to_the_live_indexes)"})
        self.assertIn("proven additive/isolated", receipt["narrowed_filters"][0]["reason"])
        classify = runpy.run_path(str(SCRIPT))["additive_migration"]
        self.assertFalse(classify(migration.replace("use rusqlite::", "use custom::")))
        self.assertFalse(classify(migration.replace("tx.commit()?;", "mutate_existing(&tx);\n            tx.commit()?;")))
        self.assertFalse(classify(migration.replace("CREATE INDEX turns_session ON turns", "CREATE INDEX turns_session ON sessions")))

    def test_new_table_proof_rejects_existing_targets_data_writes_and_unknown_helpers(self):
        classify = runpy.run_path(str(SCRIPT))["additive_migration"]
        base = 'impl Store { fn migrate_v151(&self) { tx.execute_batch("%s"); %s } }'
        new = "CREATE TABLE turns(id TEXT PRIMARY KEY, state TEXT);"
        self.assertTrue(classify(base % (new, "")))
        for extra, rust in [
            ("CREATE INDEX old_idx ON sessions(id);", ""),
            ("CREATE TRIGGER old_guard BEFORE UPDATE ON sessions BEGIN SELECT RAISE(ABORT,'bad'); END;", ""),
            ("CREATE TRIGGER writes_old AFTER INSERT ON turns BEGIN UPDATE sessions SET state='x'; END;", ""),
            ("INSERT INTO turns VALUES('a','live');", ""),
            ("DROP TABLE turns;", ""),
            ("CREATE TABLE copied AS SELECT * FROM turns;", ""),
            ("PRAGMA user_version=151;", ""),
            ("", "mutate_existing(&tx);"),
            ("", "(mutate_existing)(&tx);"),
            ("", "mutate_existing!(&tx);"),
            ("", 'tx.execute("UPDATE sessions SET state=1", []);'),
            ("", 'tx.pragma_update(None, "foreign_keys", 0);'),
        ]:
            with self.subTest(extra=extra, rust=rust):
                self.assertFalse(classify(base % (new + extra, rust)))
        self.assertFalse(classify('fn migrate_v151() { tx.execute_batch(SQL_FROM_HELPER); }'))
        self.assertFalse(classify(base % ("CREATE TABLE IF NOT EXISTS turns(id TEXT);", "")))
        self.assertFalse(classify(base % ("CREATE TABLE /* existing */ IF NOT EXISTS turns(id TEXT);", "")))
        self.assertTrue(classify(base % (
            "CREATE TABLE turns(id TEXT CHECK(rsi_uuid_is_canonical(id)),"
            " created_at TEXT CHECK(rsi_rfc3339_nanos_is_canonical(created_at)));", "")))

    def test_another_hunk_keeps_the_full_store_test_module(self):
        path, original = self.rewind_fixture()
        self.write(path, original.replace("156;", "157;").replace("unrelated();", "changed();"))
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["suggested_filters"], ["rsid=shard:store-01:test(store::tests)"])

    def test_deleting_non_fixture_code_keeps_the_full_module(self):
        path, original = self.rewind_fixture()
        self.write(path, original.replace("156;", "157;").replace("    unrelated();\n", ""))
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["suggested_filters"], ["rsid=shard:store-01:test(store::tests)"])

    STORE_MOD = """pub struct Store;
impl Store {
    pub fn open() -> u8 {
        1
    }
    pub fn other() -> u8 {
        2
    }
}
mod tests;
"""
    EXACT_TEST = "h1_v83_startup_terminal_history_is_idempotent_across_file_backed_reopen"

    def store_open_fixture(self, exact=True):
        """A store module whose open lifecycle hooks the repo's real declaration covers (#1585)."""
        self.write("scripts/test-coverage.json",
                   (Path(__file__).resolve().parents[1] / "test-coverage.json").read_text())
        self.write("crates/rsid-store/src/lib.rs", "pub mod store;\n")
        self.write("crates/rsid-store/src/store/mod.rs", self.STORE_MOD)
        tests = "".join(GATE.format(shard=shard, name=name) for shard, name in [
            ("store-01", "store_open_refuses_newer_schema"), ("store-02", "store_open_accepts_latest"),
            ("store-01", "init_schema_runs_the_full_chain"), ("store-01", "unrelated_store_test"),
            ("store-02", "another_unrelated_test")] + ([("store-01", self.EXACT_TEST)] if exact else []))
        self.write("crates/rsid-store/src/store/tests.rs", tests)
        self.commit("store open fixture")
        self.git("branch", "-f", "base")

    def edit_store_mod(self, *replacements):
        text = self.STORE_MOD
        for old, new in replacements:
            self.assertIn(old, text)
            text = text.replace(old, new, 1)
        self.write("crates/rsid-store/src/store/mod.rs", text)
        self.commit()

    def test_a_store_open_hook_change_selects_the_declared_open_tests_not_the_store_shards(self):
        self.store_open_fixture()
        self.edit_store_mod(("        1\n", "        // backup before migrating\n        helper(1)\n"))
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["declared_coverage"][0]["path"], "crates/rsid-store/src/store/mod.rs")
        by_shard = {f.split(":")[1]: f for f in receipt["suggested_filters"]}
        self.assertEqual(set(by_shard), {"store-01", "store-02"})
        for name in ("store_open_refuses_newer_schema", "init_schema_runs_the_full_chain", self.EXACT_TEST):
            self.assertIn(name, by_shard["store-01"])
        self.assertIn("store_open_accepts_latest", by_shard["store-02"])
        for flt in by_shard.values():
            self.assertNotIn("unrelated", flt)
            self.assertTrue(flt.startswith("rsid=shard:store-0") and "test(/^store::tests::" in flt, flt)
        # The shard compile of the changed source is retained.
        self.assertIn("store-01", receipt["shards"]["compiled_ok"])

    def test_a_new_pre_migration_helper_and_its_call_stay_declared(self):
        self.store_open_fixture()
        self.edit_store_mod(
            ("        1\n", "        pre_migration_backup();\n        1\n"),
            ("    pub fn other", "    /// Copy the file before any migration runs.\n    fn pre_migration_backup() {\n"
                               "        // copy\n    }\n\n    pub fn other"))
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(len(receipt["declared_coverage"]), 1)
        self.assertTrue(all("test(/^store::tests::" in f for f in receipt["suggested_filters"]))

    def test_a_change_outside_the_declared_functions_derives_the_module_filters(self):
        self.store_open_fixture()
        for replacements in (
            (("        2\n", "        3\n"),),
            (("        1\n", "        4\n"), ("        2\n", "        3\n")),
            (("pub struct Store;", "pub struct Store { field: u8 }"),),
        ):
            with self.subTest(replacements=replacements):
                self.git("reset", "-q", "--hard", "base")
                self.edit_store_mod(*replacements)
                code, summary, receipt = self.check()
                self.assertEqual(code, 0, summary)
                self.assertEqual(receipt["declared_coverage"], [])
                self.assertIn("rsid=shard:store-01:test(store)", receipt["suggested_filters"])

    def test_an_unresolvable_declaration_falls_back_to_the_module_filters(self):
        self.store_open_fixture(exact=False)
        self.edit_store_mod(("        1\n", "        4\n"))
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["declared_coverage"], [])
        self.assertIn("rsid=shard:store-01:test(store)", receipt["suggested_filters"])

    def test_a_rule_with_several_test_modules_selects_each_modules_named_tests(self):
        """The terminal-watch rule names tests in three modules, not all of `session` (#1593)."""
        self.write("scripts/test-coverage.json", json.dumps({"version": 1, "rules": [{
            "path": "crates/rsid/src/session/mod.rs", "reason": "planner",
            "functions": ["plan_fire"],
            "tests": [{"module": "session::tests_a", "prefixes": ["watch_"]},
                      {"module": "session::tests_b", "names": ["notice_stays_durable"]}]}]}))
        self.write("crates/rsid/src/lib.rs", "pub mod session;\n")
        mod = "mod tests_a;\nmod tests_b;\npub fn plan_fire() -> u8 {\n    1\n}\npub fn other() -> u8 {\n    2\n}\n"
        self.write("crates/rsid/src/session/mod.rs", mod)
        self.write("crates/rsid/src/session/tests_a.rs",
                   GATE.format(shard="session-01", name="watch_fires") + GATE.format(shard="session-01", name="unrelated_a"))
        self.write("crates/rsid/src/session/tests_b.rs",
                   GATE.format(shard="session-02", name="notice_stays_durable") + GATE.format(shard="session-02", name="unrelated_b"))
        self.commit("planner fixture")
        self.git("branch", "-f", "base")
        self.write("crates/rsid/src/session/mod.rs", mod.replace("    1\n", "    helper(1)\n", 1))
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(len(receipt["declared_coverage"]), 1)
        filters = receipt["suggested_filters"]
        self.assertEqual({f.split(":")[1] for f in filters}, {"session-01", "session-02"})
        self.assertTrue(any("watch_fires" in f for f in filters), filters)
        self.assertTrue(any("notice_stays_durable" in f for f in filters), filters)
        self.assertFalse(any("unrelated" in f for f in filters), filters)

    def test_maps_diff_to_shards_and_emits_one_line_and_receipt(self):
        self.write("crates/rsid/src/foo.rs", GATE.format(shard="store-02", name="a") + "const EDIT: u8 = 1;\n")
        self.write("crates/rsid/src/bar.rs", "pub fn bar() { }\n")
        self.write("crates/rsi-common/src/types.rs", "pub fn t() { }\n#[test]\nfn a() {}\n")
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertTrue(summary.startswith("check-touched-shards OK"), summary)
        self.assertEqual(receipt["shards"]["compiled_ok"], ["other-03", "store-02"])
        self.assertTrue(receipt["merge_clean"])
        self.assertTrue(receipt["ok"])
        self.assertEqual(
            receipt["suggested_filters"],
            ["rsi-common=types", "rsid=shard:other-03:test(bar)", "rsid=shard:store-02:test(foo)"],
        )
        self.assertEqual(receipt["audits"]["diff_check"]["verdict"], "ok")

    def test_filters_only_name_targets_that_hold_a_test(self):
        # An untested rsi-common module, a bin target and an integration test.
        self.write("crates/rsi-common/src/untested.rs", "pub fn u() { }\n")
        self.write("crates/rsid/src/bin/tool.rs", "fn main() { }\n#[test]\nfn t() {}\n")
        self.write("crates/rsid/tests/it.rs", "#[test]\nfn t() { }\n")
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        # bin and integration targets never map to a lib shard or compile one.
        self.assertEqual(receipt["shards"]["compiled_ok"], [])
        self.assertEqual(receipt["suggested_filters"], ["rsid=bin:tool", "rsid=test:it"])

    E2E_MODULES = ("agent_projects", "editing_mode", "fresh_install", "manager_decisions", "manager_presets",
                   "manager_seats", "manager_surface", "operator_todos", "topology_run")

    def e2e_fixture(self, modules=E2E_MODULES):
        """The real e2e surface map beside a fixture rsi `e2e_tui` target, committed as the new base."""
        self.write("scripts/e2e-tui-surfaces.json", (SCRIPT.parent / "e2e-tui-surfaces.json").read_text())
        mods = "".join(f'#[path = "e2e_tui/{m}.rs"]\nmod {m};\n' for m in modules)
        self.write("crates/rsi/tests/e2e_tui.rs", mods)
        for module in modules:
            self.write(f"crates/rsi/tests/e2e_tui/{module}.rs", "#[tokio::test]\nasync fn test_e2e_x() {}\n")
        self.commit("e2e fixture")
        self.git("branch", "-f", "base")

    def e2e_filters(self, receipt):
        return [f for f in receipt["suggested_filters"] if f.startswith("rsi=test:e2e_tui")]

    def test_a_settings_change_selects_the_editing_mode_e2e_module_only(self):
        # #2321: #2241 changed Settings staging and the stale editing_mode e2e was never selected.
        self.e2e_fixture()
        self.write("crates/rsi/src/settings_staging.rs", "pub fn stage() {}\n")
        self.write("crates/rsi/src/ui/settings.rs", "pub fn draw() {}\n")
        self.commit()
        code, summary, receipt = self.check("--no-compile")
        self.assertEqual(code, 0, summary)
        self.assertEqual(self.e2e_filters(receipt), ["rsi=test:e2e_tui:test(editing_mode::)"])
        self.assertEqual([item["module"] for item in receipt["e2e_surfaces"]], ["editing_mode"])

    def test_a_global_manager_workspace_change_selects_manager_seats_and_fresh_install(self):
        # #2321: #2237 changed the global manager workspace; manager_seats and fresh_install never ran.
        self.e2e_fixture()
        self.write("crates/rsi/src/overlay/global_manager_workspace.rs", "pub fn open() {}\n")
        self.commit()
        code, summary, receipt = self.check("--no-compile")
        self.assertEqual(code, 0, summary)
        filters = self.e2e_filters(receipt)
        for module in ("manager_seats", "fresh_install", "manager_surface"):
            self.assertIn(f"rsi=test:e2e_tui:test({module}::)", filters)
        self.assertNotIn("rsi=test:e2e_tui:test(editing_mode::)", filters)
        self.assertNotIn("rsi=test:e2e_tui", filters)

    def test_an_unmapped_tui_file_and_a_comment_only_edit_select_no_e2e_module(self):
        self.e2e_fixture()
        self.write("crates/rsi/src/clipboard.rs", "pub fn copy() {}\n")
        self.write("crates/rsi/src/todo_panel.rs", "// panel\npub fn t() {}\n")
        self.commit("base2")
        self.git("branch", "-f", "base")
        self.write("crates/rsi/src/clipboard.rs", "pub fn copy() { }\n")
        self.write("crates/rsi/src/todo_panel.rs", "// panel, reworded\npub fn t() {}\n")
        self.commit()
        code, summary, receipt = self.check("--no-compile")
        self.assertEqual(code, 0, summary)
        self.assertEqual(self.e2e_filters(receipt), [])

    def test_a_mapped_module_the_e2e_root_does_not_mount_is_dropped_and_reported(self):
        self.e2e_fixture(modules=tuple(m for m in self.E2E_MODULES if m != "operator_todos"))
        self.write("crates/rsi/src/todo_panel.rs", "pub fn t() {}\n")
        self.commit()
        code, summary, receipt = self.check("--no-compile")
        self.assertEqual(code, 0, summary)
        self.assertEqual(self.e2e_filters(receipt), [])
        self.assertEqual([d["filter"] for d in receipt["dropped_filters"]], ["rsi=test:e2e_tui:test(operator_todos::)"])

    def test_every_mounted_e2e_tui_module_is_mapped_or_explicitly_unmapped(self):
        # #2339: a new e2e_tui module must be added to scripts/e2e-tui-surfaces.json
        # (or listed in "unmapped_ok") so a change to its surface selects it.
        import re
        repo = SCRIPT.parent.parent
        surfaces = json.loads((SCRIPT.parent / "e2e-tui-surfaces.json").read_text())
        mapped = {m for s in surfaces["surfaces"] for m in s["modules"]}
        mapped |= set(surfaces.get("unmapped_ok", []))
        root = (repo / surfaces["root"]).read_text()
        mounted = set(re.findall(r"^\s*(?:pub\s+)?mod\s+(\w+)\s*;", root, re.M))
        self.assertTrue(mounted, "no modules found in the e2e_tui root")
        missing = sorted(mounted - mapped)
        self.assertEqual(
            missing, [],
            f"e2e_tui modules missing from scripts/e2e-tui-surfaces.json (map them or add to unmapped_ok): {missing}",
        )

    def test_every_mapped_module_selected_collapses_to_the_whole_target(self):
        self.e2e_fixture()
        for path in ("overlay/harness_manager.rs", "settings.rs", "todo_panel.rs", "overlay/graph.rs",
                     "app/agent_projects.rs", "overlay/project_form.rs"):
            self.write(f"crates/rsi/src/{path}", "pub fn f() {}\n")
        self.commit()
        code, summary, receipt = self.check("--no-compile")
        self.assertEqual(code, 0, summary)
        self.assertEqual(self.e2e_filters(receipt), ["rsi=test:e2e_tui"])

    def test_default_binary_main_change_selects_its_binary_tests(self):
        # Cargo compiles src/main.rs as the package-named binary target.
        self.write("crates/rsi-remote/src/main.rs", "fn main() {}\n#[test]\nfn decision_targets_test() {}\n")
        self.commit("default binary")
        self.git("branch", "-f", "base")
        self.write("crates/rsi-remote/src/main.rs", "fn main() { handler(); }\n#[test]\nfn decision_targets_test() {}\n")
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["shards"]["compiled_ok"], [])
        self.assertEqual(receipt["suggested_filters"], ["rsi-remote=bin:rsi-remote"])

    def test_main_test_added_beside_library_module_changes_adds_the_binary_filter(self):
        # #1703: library module filters alone omitted the end-to-end test in main.rs.
        self.write("crates/rsi-remote/src/lib.rs", "pub mod answers;\n")
        self.write("crates/rsi-remote/src/answers.rs", "pub fn a() {}\n#[cfg(test)]\nmod tests { #[test] fn a() {} }\n")
        self.write("crates/rsi-remote/src/main.rs", "fn main() {}\n")
        self.commit("remote base")
        self.git("branch", "-f", "base")
        self.write("crates/rsi-remote/src/answers.rs", "pub fn a() { changed(); }\n#[cfg(test)]\nmod tests { #[test] fn a() {} }\n")
        self.write("crates/rsi-remote/src/main.rs", "fn main() {}\n#[tokio::test]\nasync fn end_to_end_through_the_real_handler() {}\n")
        self.commit()
        code, summary, receipt = self.check("--no-compile")
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["suggested_filters"], ["rsi-remote=answers", "rsi-remote=bin:rsi-remote"])

    def test_filters_only_derives_the_same_filters_without_compile_or_heavy_audits(self):
        self.write("crates/rsid/src/foo.rs", GATE.format(shard="store-02", name="a") + "const EDIT: u8 = 1;\n")
        self.write("crates/rsid/src/bin/tool.rs", "fn main() { x(); }\n#[test]\nfn t() {}\n")
        self.commit()
        _, _, full = self.check("--no-compile")
        # A cargo that fails proves --filters-only never invokes it.
        code, summary, receipt = self.check("--filters-only", cargo="false")
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["suggested_filters"], full["suggested_filters"])
        self.assertIn("rsid=bin:tool", receipt["suggested_filters"])
        self.assertTrue(receipt["shards"]["compile_skipped"])
        self.assertTrue(receipt["filters_only"])
        self.assertEqual(receipt["audits"]["shard_gates"]["verdict"], "skipped")
        self.assertEqual(receipt["audits"]["released_migrations"]["verdict"], "skipped")
        self.assertEqual(receipt["audits"]["diff_check"]["verdict"], "ok")
        self.assertIn("filters", receipt["timings_sec"])

    def test_binary_source_scanner_is_selected_when_the_scanned_source_changes(self):
        # A binary test that include_str!s a lib source observes it (#1769).
        self.write("crates/rsid/src/lib.rs", "pub mod launch;\n")
        self.write("crates/rsid/src/launch.rs", "pub fn launch() {}\n")
        self.write("crates/rsid/src/main.rs",
                   'fn main() {}\n#[test]\nfn launch_scanner() { let _ = include_str!("launch.rs"); }\n')
        self.write("crates/rsid/src/bin/other.rs", "fn main() {}\n#[test]\nfn t() {}\n")
        self.commit("scanner base")
        self.git("branch", "-f", "base")
        self.write("crates/rsid/src/launch.rs", "pub fn launch() { changed(); }\n")
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertIn("rsid=bin:rsid", receipt["suggested_filters"])
        self.assertNotIn("rsid=bin:other", receipt["suggested_filters"])

    def test_registry_and_spawn_source_changes_select_target_source_scanners(self):
        # A bin test can resolve the repo root through a helper, then inspect
        # source trees and named registry files (#2165).
        self.write("crates/rsid/src/lib.rs", "pub fn marker() {}\n")
        self.write("crates/rsid/src/bin/rsi-model-control-validate.rs",
                   'fn fixture_root() -> String { let _ = "crates/rsid/src"; '
                   'let _ = "../rsid-store/src"; std::env::var("CARGO_MANIFEST_DIR").unwrap() }\n'
                   '#[test]\nfn validates_registry() { let _ = fixture_root(); '
                   'let _ = "crates/rsid-store/src/model_control/registry.rs"; }\n')
        self.write("crates/rsid-store/src/model_control/registry.rs", "pub fn registry() {}\n")
        self.write("crates/rsid/src/process.rs", "pub fn spawn() {}\n")
        self.commit("target source scanner")
        self.git("branch", "-f", "base")
        self.write("crates/rsid-store/src/model_control/registry.rs", "pub fn registry() { changed(); }\n")
        self.commit("registry edit")
        code, summary, receipt = self.check("--filters-only")
        self.assertEqual(code, 0, summary)
        self.assertIn("rsid=bin:rsi-model-control-validate", receipt["suggested_filters"])

        self.git("reset", "--hard", "base")
        self.write("crates/rsid/src/process.rs", "pub fn spawn() { std::process::Command::new(\"git\"); }\n")
        self.commit("new process spawn")
        code, summary, receipt = self.check("--filters-only")
        self.assertEqual(code, 0, summary)
        self.assertIn("rsid=bin:rsi-model-control-validate", receipt["suggested_filters"])

    def test_test_only_fixture_edit_with_quote_ending_comment_names_its_tests_and_no_bin(self):
        # #2434: a `//` comment ending in a quote desynced the lexer for the rest of the
        # file, so a helper extraction in a test module went broad (137 tests) and a bin
        # source scanner was built for a diff that touches no production source.
        def tests(edit):
            return (
                '// Regression: the CLI echoes results as a "user"\n'
                'fn expect_user() { let _ = "user".to_string(); }\n'
                + "".join(GATE.format(shard="session-03", name=f"filler_{n}") for n in range(70))
                + f'fn helper() {{ {edit} }}\n'
                + GATE.format(shard="session-03", name="uses_helper").replace("{}", "{ helper(); }")
            )
        self.write("crates/rsid/src/lib.rs", "pub mod session;\n")
        self.write("crates/rsid/src/session/tests.rs", tests("let _ = 1;"))
        self.write("crates/rsid/src/bin/rsi-model-control-validate.rs",
                   'fn fixture_root() -> String { let _ = "crates/rsid/src"; '
                   'std::env::var("CARGO_MANIFEST_DIR").unwrap() }\n'
                   '#[test]\nfn validates() { let _ = fixture_root(); }\n')
        self.commit("test module base")
        self.git("branch", "-f", "base")
        self.write("crates/rsid/src/session/tests.rs", tests("let _ = 2;"))
        self.commit("helper edit")
        code, summary, receipt = self.check("--filters-only")
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["broad_filters"], [], summary)
        self.assertFalse(any("=bin:" in f for f in receipt["suggested_filters"]), receipt["suggested_filters"])
        self.assertTrue(any("uses_helper" in f for f in receipt["suggested_filters"]), receipt["suggested_filters"])

    SCANNER = (
        'pub mod p2_07_gate_permit_spine {\n'
        '    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]\n'
        '    #[test]\n'
        '    fn exactly_one_writer() {\n'
        '        let root = std::path::PathBuf::from(rsi_common::manifest_dir!()).join("../rsid-store/src");\n'
        '        let _ = std::fs::read_dir(root);\n'
        '    }\n'
        '}\n'
        '#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]\n'
        '#[test]\nfn names_no_source_tree() { let _ = std::env::var("CARGO_MANIFEST_DIR"); }\n'
    )

    def scanner_tree(self):
        self.write("crates/rsid/src/lib.rs", "pub mod session;\n")
        self.write("crates/rsid/src/session/issue21_phase2_tests.rs", self.SCANNER)
        self.write("crates/rsid-store/src/lib.rs", "pub mod store;\n")
        self.write("crates/rsid-store/src/store/cohort_settlement.rs", "pub fn settle() {}\n")
        self.write("crates/rsi-common/src/other.rs", "pub fn other() {}\n")
        self.commit("scanner base")
        self.git("branch", "-f", "base")

    def test_store_only_diff_selects_the_source_scanners_that_read_the_store(self):
        # #2034: a scanner reads production files it does not name, so a store-only
        # diff must still run it, by exact name and from the tests themselves.
        self.scanner_tree()
        self.write("crates/rsid-store/src/store/cohort_settlement.rs", "pub fn settle() { changed(); }\n")
        self.commit()
        for flags in (("--no-compile",), ("--filters-only",)):
            code, summary, receipt = self.check(*flags)
            self.assertEqual(code, 0, summary)
            joined = "\n".join(receipt["suggested_filters"])
            self.assertIn("session::issue21_phase2_tests::(?:\\w+::)*exactly_one_writer", joined)
            self.assertNotIn("names_no_source_tree", joined)
            self.assertEqual(receipt["source_scanners"], ["session::issue21_phase2_tests::exactly_one_writer"])

    def test_diff_outside_the_scanned_crates_leaves_scanners_out(self):
        self.scanner_tree()
        self.write("crates/rsi-common/src/other.rs", "pub fn other() { changed(); }\n")
        self.commit()
        code, summary, receipt = self.check("--filters-only")
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["source_scanners"], [])
        self.assertNotIn("exactly_one_writer", "\n".join(receipt["suggested_filters"]))

    TOKEN_SCANNERS = [
        "store::cold_archive::read_classes::cold_archive_reader_classification",
        "store::cold_archive::read_tests::cold_archive_reader_invariance",
    ]

    def token_scanner_tree(self, token="conversation_events"):
        registry = json.loads(SCRIPT.with_name("scanned-tokens.json").read_text())
        if token != "conversation_events":
            registry["tokens"][token] = registry["tokens"].pop("conversation_events")
        self.write("scripts/scanned-tokens.json", json.dumps(registry))
        self.write("crates/rsid-store/src/lib.rs", "pub mod store;\n")
        self.write("crates/rsid-store/src/store/mod.rs", "pub mod cold_archive;\n")
        self.write("crates/rsid-store/src/store/cold_archive/mod.rs",
                   "#[cfg(test)]\nmod read_classes;\n#[cfg(test)]\nmod read_tests;\n")
        for module, name in zip(("read_classes", "read_tests"), self.TOKEN_SCANNERS):
            self.write(f"crates/rsid-store/src/store/cold_archive/{module}.rs",
                       GATE.format(shard="store-04", name=name.split("::")[-1]))
        self.commit("token-scanner base")
        self.git("branch", "-f", "base")

    def assert_token_scanners(self, receipt):
        self.assertEqual(receipt["source_scanners"], self.TOKEN_SCANNERS)
        filters = receipt["suggested_filters"]
        # This is the same parser scoped-test uses on the queue's derivation.
        scoped = runpy.run_path(str(SCRIPT.with_name("scoped-test")))
        plan = scoped["selection_plan"](filters, {"store-04": ["rsid", "rsid-store"]})
        selected = "\n".join(str(selection) for selection in plan["rsid-store"])
        for name in self.TOKEN_SCANNERS:
            self.assertIn(name.split("::")[-1], selected)

    def test_new_token_reader_in_each_crate_selects_registered_scanners(self):
        self.token_scanner_tree()
        for package in ("rsid-store", "rsid", "rsid-remote-read"):
            with self.subTest(package=package):
                self.write(f"crates/{package}/src/new_reader.rs",
                           'pub fn new_reader() { query("SELECT id FROM conversation_events"); }\n')
                self.commit()
                code, summary, receipt = self.check("--filters-only")
                self.assertEqual(code, 0, summary)
                self.assert_token_scanners(receipt)
                self.git("branch", "-f", "base")

    def test_existing_token_reader_predicate_edit_and_removal_select_scanners(self):
        self.token_scanner_tree()
        path = "crates/rsid-store/src/store/existing.rs"
        self.write(path, 'pub fn existing() {\n    query("SELECT id FROM conversation_events");\n    gate(1);\n}\n')
        self.commit("existing reader")
        self.git("branch", "-f", "base")
        for text in ('pub fn existing() {\n    query("SELECT id FROM conversation_events");\n    gate(2);\n}\n',
                     'pub fn existing() { query("SELECT id FROM another_table"); }\n'):
            self.write(path, text)
            self.commit()
            code, summary, receipt = self.check("--filters-only")
            self.assertEqual(code, 0, summary)
            self.assert_token_scanners(receipt)

    def test_unrelated_and_test_only_token_edits_keep_focused_selection(self):
        self.token_scanner_tree()
        self.write("crates/rsid/src/unrelated.rs", "pub fn unrelated() { changed(); }\n")
        self.write("crates/rsid-remote-read/src/tests.rs",
                   '#[test]\nfn fixture() { query("conversation_events"); }\n')
        self.commit()
        code, summary, receipt = self.check("--filters-only")
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["source_scanners"], [])

    def test_future_scanner_token_needs_only_a_registry_entry(self):
        self.token_scanner_tree("future_scanned_table")
        self.write("crates/rsid/src/future_reader.rs",
                   'pub fn future_reader() { query("future_scanned_table"); }\n')
        self.commit()
        code, summary, receipt = self.check("--filters-only")
        self.assertEqual(code, 0, summary)
        self.assert_token_scanners(receipt)

    def test_unprovable_token_leaf_keeps_scoped_pins_and_unfiltered_landing(self):
        self.token_scanner_tree()
        path = "crates/rsid-remote-read/src/new_reader.rs"
        self.write(path, 'pub const SQL: &str = "conversation_events";\n')
        self.commit()
        code, summary, receipt = self.check("--filters-only")
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["suggested_filters"], [])
        self.assertEqual(receipt["unfiltered_leaf_paths"], [path])
        self.assertEqual(receipt["source_scanners"], self.TOKEN_SCANNERS)
        joined = "\n".join(receipt["scanned_token_filters"])
        for name in self.TOKEN_SCANNERS:
            self.assertIn(name.split("::")[-1], joined)

    def test_unresolved_registered_scanner_fails_derivation(self):
        self.token_scanner_tree()
        self.write("crates/rsid/src/new_reader.rs", 'pub fn read() { query("conversation_events"); }\n')
        self.write("crates/rsid-store/src/store/cold_archive/read_classes.rs", "// removed\n")
        self.commit()
        result = subprocess.run([str(SCRIPT), "--repo", str(self.root), "--base", "base",
                                 "--filters-only", "--json"], capture_output=True, text=True)
        self.assertEqual(result.returncode, 1)
        self.assertIn("scanned-token test does not resolve", result.stderr)

    def test_integration_test_reading_a_changed_command_file_is_selected(self):
        # #1926: a command edit changes what an integration test that reads it observes.
        self.write("crates/rsi-common/src/lib.rs", "pub fn marker() {}\n")
        self.write(".claude/commands/research.md", "old\n")
        self.write("crates/rsi-common/tests/reads_command.rs",
                   '#[test]\nfn reads() { let _ = root.join(".claude/commands/research.md"); }\n')
        self.write("crates/rsid/src/lib.rs", "pub fn marker() {}\n")
        self.write("crates/rsid/tests/joins_command.rs",
                   '#[test]\nfn joins() { let _ = cur.join(".claude").join("commands"); }\n')
        self.write("crates/rsid/tests/reads_skill.rs",
                   '#[test]\nfn skill() { let _ = root.join(".agents/skills/x/SKILL.md"); }\n')
        self.write("crates/rsid/tests/unrelated.rs", '#[test]\nfn other() { let _ = "settings"; }\n')
        self.commit("asset readers base")
        self.git("branch", "-f", "base")
        self.write(".claude/commands/research.md", "new\n")
        self.commit()
        code, summary, receipt = self.check("--filters-only")
        self.assertEqual(code, 0, summary)
        self.assertIn("rsi-common=test:reads_command", receipt["suggested_filters"])
        self.assertIn("rsid=test:joins_command", receipt["suggested_filters"])
        self.assertNotIn("rsid=test:reads_skill", receipt["suggested_filters"])
        self.assertNotIn("rsid=test:unrelated", receipt["suggested_filters"])

    def test_comment_and_doc_only_rust_hunks_select_only_source_readers(self):
        path = "crates/rsid/src/session/launch.rs"
        original = ("/// Old launch guidance.\n"
                    "pub fn launch() {\n    work(); // old note\n}\n"
                    "/* old block /* nested */ done */\n")
        self.write("crates/rsid/src/lib.rs", "pub mod session;\n")
        self.write("crates/rsid/src/session/mod.rs", "mod launch;\n")
        self.write(path, original + self.gated_tests("session-01", [f"runtime_{n}" for n in range(70)]))
        self.write("crates/rsid/src/session/scanners.rs",
                   'fn read_launch() {\n    let source = include_str!("launch.rs");\n}\n' +
                   self.gated_tests("other-03", ["launch_guidance_contract"], {"launch_guidance_contract": "read_launch"}))
        self.write("crates/rsid/src/main.rs",
                   'fn main() {}\n#[test]\nfn source_contract() { let _ = include_str!("session/launch.rs"); }\n')
        self.commit("launch docs fixture")
        self.git("branch", "-f", "base")
        self.write(path, (original + self.gated_tests("session-01", [f"runtime_{n}" for n in range(70)]))
                   .replace("Old launch guidance.", "New launch guidance.")
                   .replace("old note", "new note AGENT_VERBS")
                   .replace("old block /* nested */", "new block /* nested /* deeper */ */"))
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertFalse(receipt["broad"])
        self.assertEqual(receipt["shards"]["compiled_ok"], ["session-01"])
        self.assertEqual(set(receipt["suggested_filters"]), {
            "rsid=bin:rsid",
            "rsid=shard:other-03:test(/^session::scanners::(?:\\w+::)*launch_guidance_contract$/)"})
        self.assertIn("only Rust comments/docs", receipt["narrowed_filters"][0]["reason"])

    def test_comment_deletion_without_source_readers_keeps_only_compile_check(self):
        self.write("crates/rsid/src/foo.rs", "/// Guidance\n" + GATE.format(shard="store-02", name="a"))
        self.commit("comment base")
        self.git("branch", "-f", "base")
        self.write("crates/rsid/src/foo.rs", GATE.format(shard="store-02", name="a"))
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["suggested_filters"], [])
        self.assertEqual(receipt["shards"]["compiled_ok"], ["store-02"])

    def test_rust_comment_classification_preserves_literals_attributes_and_mixed_code(self):
        classify = runpy.run_path(str(SCRIPT))["rust_comment_only_change"]
        fixtures = [
            'fn f() { let s = "https://old/*value*/"; } // guide',
            'fn f() { let s = r##"/*old*/ // value"##; } // guide',
            'fn f() { let s = b"//old"; } // guide',
            'fn old() { let c = \'/\'; } // guide',
            '#[doc = "old"]\nfn f() {} // guide',
            'fn f() { old(); } // guide',
        ]
        for original in fixtures:
            with self.subTest(source=original):
                self.assertTrue(classify(original, original.replace("guide", "new guide")))
                self.assertFalse(classify(original, original.replace("old", "new").replace("guide", "new guide")))
        self.assertFalse(classify("fn f() {} // old", "fn f() {} /* unterminated"))
        self.assertFalse(classify("fn f() { let c = '/'; } // guide", "fn f() { let c = '*'; } // new guide"))
        self.assertFalse(classify("fn f() {}", "fn f() { changed(); } // guide"))
        self.assertTrue(classify("/** old */\nfn f() {}", "/** new */\nfn f() {}"))

    def registration_fixture(self, package="rsid", module="session"):
        root = f"crates/{package}/src/{module}/mod.rs"
        self.write(f"crates/{package}/src/lib.rs", f"pub mod {module};\n")
        self.write(root, "mod old;\nmod new;\npub fn marker() {}\n")
        self.write(f"crates/{package}/src/{module}/old.rs", GATE.format(shard="store-02", name="old"))
        self.write(f"crates/{package}/src/{module}/new.rs", GATE.format(shard="other-03", name="new"))
        self.commit("registration fixture")
        self.git("branch", "-f", "base")
        return root

    def test_registration_additions_select_only_the_registered_modules(self):
        session = self.registration_fixture()
        store = self.registration_fixture("rsid-store", "store")
        self.write(session, "mod old;\nmod new;\nmod added; // registration\npub fn marker() {}\n")
        self.write("crates/rsid/src/session/added.rs", GATE.format(shard="other-03", name="added"))
        self.write(store, "mod old;\nmod new;\npub mod added;\npub use new::Item;\npub fn marker() {}\n")
        self.write("crates/rsid-store/src/store/added.rs", GATE.format(shard="store-02", name="added"))
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(set(receipt["suggested_filters"]), {
            "rsid=shard:other-03:test(session::added)",
            "rsid=shard:other-03:test(store::new)",
            "rsid=shard:store-02:test(store::added)",
        })

    def test_path_registration_in_regular_rust_file_selects_only_the_new_module(self):
        path = "crates/rsid-store/src/store/pending_questions.rs"
        original = self.gated_tests("store-03", [f"existing_{n}" for n in range(10)])
        self.write("crates/rsid-store/src/lib.rs", "pub mod store;\n")
        self.write("crates/rsid-store/src/store/mod.rs", "pub mod pending_questions;\n")
        self.write(path, original)
        self.commit("pending questions fixture")
        self.git("branch", "-f", "base")
        self.write(path, '#[path = "remote_answers.rs"]\npub mod remote_answers;\n' + original)
        self.write("crates/rsid-store/src/store/remote_answers.rs",
                   '#[cfg(test)]\n#[path = "remote_answers/tests.rs"]\nmod tests;\n')
        self.write("crates/rsid-store/src/store/remote_answers/tests.rs",
                   self.gated_tests("store-04", ["new_answer_journal"]))
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["suggested_filters"], [
            "rsid=shard:store-04:test(store::pending_questions::remote_answers)"])

    def test_plain_mod_tests_under_a_path_mount_does_not_claim_the_stem_directory_file_1697(self):
        # rustc loads `store/tests.rs` for a plain `mod tests;` in a `#[path]`-mounted file, so
        # `store/remote_answers/tests.rs` is never compiled and its shard gate covers nothing.
        path = "crates/rsid-store/src/store/pending_questions.rs"
        original = self.gated_tests("store-03", [f"existing_{n}" for n in range(10)])
        self.write("crates/rsid-store/src/lib.rs", "pub mod store;\n")
        self.write("crates/rsid-store/src/store/mod.rs", "pub mod pending_questions;\n")
        self.write(path, original)
        self.write("crates/rsid-store/src/store/remote_answers.rs", "pub fn f() {}\n")
        self.write("crates/rsid-store/src/store/remote_answers/tests.rs",
                   self.gated_tests("store-02", ["never_compiled"]))
        self.commit("stray test file fixture")
        self.git("branch", "-f", "base")
        self.write(path, '#[path = "remote_answers.rs"]\npub mod remote_answers;\n' + original)
        self.write("crates/rsid-store/src/store/remote_answers.rs", "pub fn f() {}\n#[cfg(test)]\nmod tests;\n")
        self.write("crates/rsid-store/src/store/tests.rs", self.gated_tests("store-04", ["compiled_beside"]))
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        # The never-compiled file's store-02 gate is not claimed as coverage.
        self.assertNotIn("store-02", receipt["shards"]["map"])
        self.assertEqual(receipt["shards"]["map"]["store-04"], [
            "crates/rsid-store/src/store/remote_answers.rs", "crates/rsid-store/src/store/tests.rs"])
        self.assertEqual(receipt["suggested_filters"], [
            "rsid=shard:store-04:test(store::pending_questions::remote_answers)"])

    def test_registration_removal_selects_the_removed_module(self):
        root = self.registration_fixture()
        self.write(root, "mod new;\npub fn marker() {}\n")
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["suggested_filters"], ["rsid=shard:store-02:test(session::old)"])
        self.assertEqual(receipt["shards"]["compiled_ok"], ["store-02"])

    def test_pub_use_removal_selects_the_reexported_module(self):
        root = self.registration_fixture()
        self.write(root, "mod old;\nmod new;\npub use self::new::{Item, Other};\npub fn marker() {}\n")
        self.commit("reexport")
        self.git("branch", "-f", "base")
        self.write(root, "mod old;\nmod new;\npub fn marker() {}\n")
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["suggested_filters"], ["rsid=shard:other-03:test(session::new)"])

    def test_pub_use_paths_and_aliases_select_the_reexported_module(self):
        root = self.registration_fixture()
        for use in ("new", "new as renamed", "crate::session::new", "super::session::new::Item", "self::new::*"):
            with self.subTest(use=use):
                self.write(root, f"mod old;\nmod new;\npub(crate) use {use};\npub fn marker() {{}}\n")
                self.commit()
                code, summary, receipt = self.check()
                self.assertEqual(code, 0, summary)
                self.assertEqual(receipt["suggested_filters"], ["rsid=shard:other-03:test(session::new)"])

    def test_real_mod_rs_change_retains_the_root_filter(self):
        root = self.registration_fixture()
        # Include a registration removal so both diff sides must be examined.
        self.write(root, "mod new;\npub fn marker() { println!(\"changed\"); }\n")
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["suggested_filters"], [
            "rsid=shard:other-03:test(session)",
        ])

    def test_removing_code_from_mod_rs_retains_the_root_filter(self):
        root = self.registration_fixture()
        self.write(root, "mod old;\nmod new;\n")
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["suggested_filters"], [
            "rsid=shard:other-03:test(session)",
        ])

    def test_binary_submodule_selects_its_binary_even_when_tests_are_elsewhere(self):
        self.write("crates/rsid/src/bin/tool/helper.rs", "pub fn helper() {}\n")
        self.write("crates/rsi-common/src/bin/nested/main.rs", "fn main() {}\n")
        self.write("crates/rsi-common/src/bin/nested/tests.rs", "#[test]\nfn t() {}\n")
        self.write("crates/rsi-common/src/bin/nested/deep/helper.rs", "pub fn helper() {}\n")
        self.commit("binary modules")
        self.git("branch", "-f", "base")
        self.write("crates/rsid/src/bin/tool/helper.rs", "pub fn helper() { }\n")
        self.write("crates/rsi-common/src/bin/nested/deep/helper.rs", "pub fn helper() { }\n")
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["suggested_filters"], ["rsi-common=bin:nested", "rsid=bin:tool"])
        self.assertEqual(receipt["shards"]["compiled_ok"], [])

    def filters_after(self, base_files, changed_files):
        """The suggested filters of a diff that rewrites `changed_files` on top of `base_files`."""
        for rel, text in base_files.items():
            self.write(rel, text)
        self.commit("fixture base")
        self.git("branch", "-f", "base")
        for rel, text in changed_files.items():
            self.write(rel, text)
        self.commit()
        code, summary, receipt = self.check("--filters-only")
        self.assertEqual(code, 0, summary)
        return receipt["suggested_filters"]

    def test_tests_helper_module_selects_only_the_integration_targets_that_mount_it(self):
        # #1937: tests/common/mod.rs is no target, but the tests that `mod common;` observe it.
        filters = self.filters_after(
            {"crates/rsi-common/tests/a.rs": "mod common;\n#[test]\nfn t() {}\n",
             "crates/rsi-common/tests/b.rs": "#[test]\nfn t() {}\n",
             "crates/rsi-common/tests/common/mod.rs": "pub fn h() {}\n"},
            {"crates/rsi-common/tests/common/mod.rs": "pub fn h() { work(); }\n"})
        self.assertEqual(filters, ["rsi-common=test:a"])

    def test_integration_directory_target_and_mounted_suite_modules_select_their_target(self):
        # #1937: tests/NAME/main.rs owns its directory; tests/suite.rs owns the modules it mounts.
        filters = self.filters_after(
            {"crates/rsi-common/tests/dir/main.rs": "mod sub;\n#[test]\nfn t() {}\n",
             "crates/rsi-common/tests/dir/sub.rs": "pub fn s() {}\n",
             "crates/rsi-common/tests/suite.rs": "mod cases;\n#[test]\nfn t() {}\n",
             "crates/rsi-common/tests/suite/cases.rs": "pub fn c() {}\n"},
            {"crates/rsi-common/tests/dir/sub.rs": "pub fn s() { work(); }\n",
             "crates/rsi-common/tests/suite/cases.rs": "pub fn c() { work(); }\n"})
        self.assertEqual(filters, ["rsi-common=test:dir", "rsi-common=test:suite"])

    def test_module_mounted_only_by_the_binary_selects_the_binary_not_a_lib_filter(self):
        # #1650: `cargo test --lib gateway` selects no test when only main.rs mounts gateway.
        gateway = "pub fn g() {}\n#[cfg(test)]\nmod tests { #[test] fn a() {} }\n"
        for lib in (False, True):
            with self.subTest(lib=lib):
                files = {"crates/rsi-remote/src/main.rs": "mod gateway;\nfn main() {}\n",
                         "crates/rsi-remote/src/gateway.rs": gateway}
                if lib:
                    files["crates/rsi-remote/src/lib.rs"] = "pub fn l() {}\n"
                filters = self.filters_after(files, {"crates/rsi-remote/src/gateway.rs": gateway.replace("g() {}", "g() { work(); }")})
                self.assertEqual(filters, ["rsi-remote=bin:rsi-remote"])

    def test_module_shared_by_the_library_and_a_binary_selects_both(self):
        filters = self.filters_after(
            {"crates/rsi-common/src/lib.rs": "pub mod foo;\n",
             "crates/rsi-common/src/bin/n.rs": '#[path = "../foo.rs"]\nmod foo;\nfn main() {}\n',
             "crates/rsi-common/src/foo.rs": "pub fn f() {}\n#[test]\nfn t() {}\n"},
            {"crates/rsi-common/src/foo.rs": "pub fn f() { work(); }\n#[test]\nfn t() {}\n"})
        self.assertEqual(filters, ["rsi-common=bin:n", "rsi-common=foo"])

    def test_sources_that_include_a_changed_data_file_run_their_own_tests(self):
        # A library module's test, a bin and an integration test that include_str! a
        # changed non-Rust file observe it; a reader of another file does not.
        read = '#[cfg(test)]\nmod tests {{ #[test] fn reads() {{ let _ = include_str!("{path}"); }} }}\n'
        filters = self.filters_after(
            {"docs/x.md": "old\n", "docs/y.md": "old\n",
             "crates/rsi-common/src/lib.rs": "pub mod schema;\npub mod other;\n",
             "crates/rsi-common/src/schema.rs": "pub fn s() {}\n" + read.format(path="../../../docs/x.md"),
             "crates/rsi-common/src/other.rs": "pub fn o() {}\n" + read.format(path="../../../docs/y.md"),
             "crates/rsi-common/src/bin/tool.rs": 'fn main() {}\n#[test]\nfn t() { let _ = include_bytes!("../../../../docs/x.md"); }\n',
             "crates/rsi/tests/it.rs": '#[test]\nfn t() { let _ = include_str!("../../../docs/x.md"); }\n'},
            {"docs/x.md": "new\n"})
        self.assertEqual(sorted(filters), ["rsi-common=bin:tool", "rsi-common=schema", "rsi=test:it"])

    def test_binary_test_reading_a_changed_skill_file_is_selected(self):
        # #1926 covered integration tests only; a bin's own test reads assets too.
        filters = self.filters_after(
            {".claude/skills/x/SKILL.md": "old\n",
             "crates/rsi-common/src/bin/tool.rs": 'fn main() {}\n#[test]\nfn t() { let _ = root.join(".claude/skills/x/SKILL.md"); }\n',
             "crates/rsi-common/src/bin/other.rs": "fn main() {}\n#[test]\nfn t() {}\n"},
            {".claude/skills/x/SKILL.md": "new\n"})
        self.assertEqual(filters, ["rsi-common=bin:tool"])

    def test_package_fixture_read_by_a_path_literal_selects_only_its_readers(self):
        filters = self.filters_after(
            {"crates/rsi-common/tests/fixtures/m/a.json": "{}",
             "crates/rsi-common/tests/corpus.rs": '#[test]\nfn t() { let _ = dir.join("fixtures").join("m"); }\n',
             "crates/rsi-common/tests/other.rs": '#[test]\nfn t() { let _ = dir.join("snapshots"); }\n',
             # Another package's bare file name is not a read of this fixture.
             "crates/rsi/tests/bare.rs": '#[test]\nfn t() { let _ = dir.join("a.json"); }\n'},
            {"crates/rsi-common/tests/fixtures/m/a.json": '{"x": 1}'})
        self.assertEqual(filters, ["rsi-common=test:corpus"])

    def synthetic_path_filters(self, reader, extra=None):
        """Filters of a docs-only change when `reader` is the only crate source naming docs/x.md."""
        base = {"docs/x.md": "old\n",
                "crates/rsid/src/bin/tool.rs": "fn main() {}\n" + reader,
                "crates/rsid/src/bin/other.rs": "fn main() {}\n#[test]\nfn t() {}\n"}
        base.update(extra or {})
        return self.filters_after(base, {"docs/x.md": "new\n"})

    def test_a_fabricated_path_list_fed_to_in_memory_code_selects_no_bin(self):
        # #199: `classify(&["docs/x.md"])` is fixture input, not a read of the doc.
        filters = self.synthetic_path_filters(
            '#[test]\nfn t() {\n    assert!(classify(&["docs/x.md", "scripts/y.sh"]));\n}\n'
            "fn classify(paths: &[&str]) -> bool {\n    paths.iter().any(|p| p.starts_with(\"docs/\"))\n}\n")
        self.assertEqual(filters, [])

    def test_a_fabricated_path_loop_checked_in_memory_selects_no_bin(self):
        filters = self.synthetic_path_filters(
            '#[test]\nfn t() {\n    for path in [\n        "docs/x.md",\n        "crates/a/Cargo.toml",\n    ] {\n'
            "        assert_eq!(classify(&[path]), None);\n    }\n}\n"
            "fn classify(paths: &[&str]) -> Option<u8> {\n    paths.first().map(|_| 1)\n}\n")
        self.assertEqual(filters, [])

    def test_a_path_list_a_test_reads_from_disk_selects_its_bin(self):
        for name, reader in {
            "fs read of each path": '#[test]\nfn t() {\n    for p in ["docs/x.md"] {\n        let _ = std::fs::read_to_string(root.join(p));\n    }\n}\n',
            "same-file helper reads the list":
                '#[test]\nfn t() {\n    load(&["docs/x.md"]);\n}\nfn load(paths: &[&str]) {\n    for p in paths { let _ = std::fs::read(p); }\n}\n',
            "path method observes the list":
                '#[test]\nfn t() {\n    for p in ["docs/x.md"] { assert!(root.join(p).exists()); }\n}\n',
            "alias hides the reader":
                'use std::fs::read as slurp;\n#[test]\nfn t() {\n    for p in ["docs/x.md"] { let _ = slurp(p); }\n}\n',
            "glob import hides the reader":
                'use helpers::*;\n#[test]\nfn t() {\n    load_all(&["docs/x.md"]);\n}\n',
            "unknown macro":
                '#[test]\nfn t() {\n    read_docs!(&["docs/x.md"]);\n}\n',
            "include macro":
                '#[test]\nfn t() {\n    let _ = (&["docs/x.md"], include_str!("../../../../docs/x.md"));\n}\n',
            "const list outside any function":
                'const DOCS: &[&str] = &["docs/x.md"];\n#[test]\nfn t() {\n    for p in DOCS { check(p); }\n}\n',
            "single path literal join":
                '#[test]\nfn t() {\n    let _ = root.join("docs/x.md");\n}\n',
            "function parameter may be a reader":
                '#[test]\nfn t() {\n    scan(&["docs/x.md"], std::fs::read_to_string);\n}\nfn scan(paths: &[&str], read: fn(&str) -> String) {\n    for p in paths { read(p); }\n}\n',
        }.items():
            with self.subTest(name):
                self.assertEqual(self.synthetic_path_filters(reader, {"crates/rsid/src/bin/tool.rs": "fn main() {}\n" + reader}),
                                 ["rsid=bin:tool"])

    def test_a_path_list_whose_consumer_cannot_be_proved_in_memory_selects_its_bin(self):
        # #199: a known std name, a let alias or a typed receiver is not proof; only a project
        # function, a `for` loop or a safe macro may consume the list, and only plain data may reach it.
        for name, reader in {
            "unknown receiver method on a bound list":
                'fn fixture(reader: &ExternalReader) {\n    let paths = ["docs/x.md"];\n    reader.load_paths(&paths);\n}\n',
            "std-named method on an unknown receiver":
                'fn fixture(reader: &ExternalReader) {\n    let paths = ["docs/x.md"];\n    reader.get(&paths);\n}\n',
            "std-named method taking the list directly":
                'fn fixture(reader: &ExternalReader) {\n    reader.get(&["docs/x.md"]);\n}\n',
            "callback aliased by let":
                'fn fixture(read: impl Fn(&str)) {\n    let reader = read;\n    for path in ["docs/x.md"] {\n        reader(path);\n    }\n}\n',
            "external associated function":
                'fn fixture() {\n    external::load(&["docs/x.md"]);\n}\n',
            "external constructor then method":
                'fn fixture() {\n    let reader = ExternalReader::new();\n    reader.get(&["docs/x.md"]);\n}\n',
            "list used as a method receiver with a project callback":
                'fn fixture() {\n    ["docs/x.md"].iter().for_each(load_file);\n}\nfn load_file(p: &&str) {\n    let _ = std::fs::read(p);\n}\n',
            "callback parameter of a project function":
                'fn fixture() {\n    run(&["docs/x.md"], load_file);\n}\nfn run(paths: &[&str], f: fn(&str)) {\n    for p in paths { f(p); }\n}\nfn load_file(p: &str) {\n    let _ = std::fs::read(p);\n}\n',
            "generic parameter of a project function":
                'fn fixture() {\n    run(&["docs/x.md"]);\n}\nfn run<T: Loader>(paths: &[&str]) {\n    T::load(paths);\n}\n',
            "project method with a std name that reads":
                'struct Loader;\nimpl Loader {\n    fn get(&self, paths: &[&str]) {\n        for p in paths { let _ = std::fs::read(p); }\n    }\n}\n'
                'fn fixture() {\n    check(&["docs/x.md"], &Loader);\n}\nfn check(paths: &[&str], loader: &Loader) {\n    loader.get(paths);\n}\n',
            "foreign field inside a project wrapper":
                'struct Local {\n    backend: ExternalReader,\n}\nfn fixture(reader: &Local) {\n    check(&["docs/x.md"], reader);\n}\n'
                'fn check(paths: &[&str], reader: &Local) {\n    for path in paths {\n        reader.backend.get(path);\n    }\n}\n',
            "foreign closure parameter":
                'fn fixture() {\n    let work = |reader: ExternalReader| {\n        for path in ["docs/x.md"] {\n            reader.get(path);\n        }\n    };\n}\n',
            "foreign field built in the walk":
                'struct Local {\n    backend: ExternalReader,\n}\nfn fixture() {\n    let local = Local { backend: ExternalReader::new() };\n    check(&["docs/x.md"], &local);\n}\n'
                'fn check(paths: &[&str], local: &Local) {\n    for path in paths {\n        local.backend.get(path);\n    }\n}\n',
            "callback field fed by a function item":
                'struct Hook<\'a> {\n    run: &\'a dyn Fn(&str),\n}\nfn fixture() {\n    check(&["docs/x.md"], &Hook { run: &load_file });\n}\n'
                'fn check(paths: &[&str], hook: &Hook) {\n    for path in paths { (hook.run)(path); }\n}\nfn load_file(p: &str) {\n    let _ = std::fs::read(p);\n}\n',
            "callback field received from the caller":
                'struct Hook<\'a> {\n    run: &\'a dyn Fn(&str),\n}\nfn fixture(hook: &Hook) {\n    check(&["docs/x.md"], hook);\n}\n'
                'fn check(paths: &[&str], hook: &Hook) {\n    for path in paths { (hook.run)(path); }\n}\n',
            "constant holding a callback":
                'static READER: fn(&str) = load_file;\nfn fixture() {\n    for path in ["docs/x.md"] {\n        READER(path);\n    }\n}\nfn load_file(p: &str) {\n    let _ = std::fs::read(p);\n}\n',
            "implicit Drop of a project type":
                'struct Local {\n    path: &\'static str,\n}\nimpl Drop for Local {\n    fn drop(&mut self) {\n        let _ = std::fs::read(self.path);\n    }\n}\n'
                'fn fixture() {\n    for path in ["docs/x.md"] {\n        let local = Local { path };\n    }\n}\n',
            "implicit Deref of a project type":
                'struct Local {\n    path: String,\n}\nimpl std::ops::Deref for Local {\n    type Target = str;\n    fn deref(&self) -> &str {\n        let _ = std::fs::read(&self.path);\n        &self.path\n    }\n}\n'
                'fn fixture() {\n    for path in ["docs/x.md"] {\n        let local = Local { path: path.to_string() };\n        assert!(local.len() > 0);\n    }\n}\n',
            "implicit Index of a project type":
                'struct Local;\nimpl std::ops::Index<&str> for Local {\n    type Output = u8;\n    fn index(&self, path: &str) -> &u8 {\n        let _ = std::fs::read(path);\n        &0\n    }\n}\n'
                'fn fixture() {\n    let local = Local;\n    for path in ["docs/x.md"] {\n        let _ = local[path];\n    }\n}\n',
            "implicit IntoIterator of a project type":
                'struct Local {\n    path: &\'static str,\n}\nimpl IntoIterator for Local {\n    type Item = u8;\n    type IntoIter = std::vec::IntoIter<u8>;\n    fn into_iter(self) -> Self::IntoIter {\n        let _ = std::fs::read(self.path);\n        Vec::new().into_iter()\n    }\n}\n'
                'fn fixture() {\n    for path in ["docs/x.md"] {\n        for _ in (Local { path }) {}\n    }\n}\n',
            "method of a project type the file does not define":
                'struct Local;\nfn fixture() {\n    for path in ["docs/x.md"] {\n        let local = Local;\n        assert!(local.ready(path));\n    }\n}\n',
            "function value named but not called":
                'fn fixture() {\n    let paths = vec!["docs/x.md"];\n    let load = load_file;\n    check(&paths, &load);\n}\nfn check(paths: &[&str], load: &dyn Fn(&str)) {}\nfn load_file(p: &str) {\n    let _ = std::fs::read(p);\n}\n',
        }.items():
            with self.subTest(name):
                self.assertEqual(self.synthetic_path_filters(reader + "#[test]\nfn runs() {}\n"), ["rsid=bin:tool"])

    def test_a_list_fed_to_a_struct_with_a_closure_built_in_the_same_fixture_selects_no_bin(self):
        # The incident shape: the callback is a closure the fixture binds itself, so the walk scans it.
        filters = self.synthetic_path_filters(
            "struct Hook<'a> {\n    seen: &'a dyn Fn(&str) -> bool,\n}\n"
            '#[test]\nfn fixture() {\n    let seen = |path: &str| path.ends_with(".md");\n'
            '    assert!(check(&["docs/x.md"], &Hook { seen: &seen }));\n}\n'
            "fn check(paths: &[&str], hook: &Hook) -> bool {\n    paths.iter().all(|path| (hook.seen)(path))\n}\n")
        self.assertEqual(filters, [])

    def test_a_trait_a_sibling_file_implements_for_a_std_type_keeps_the_reader(self):
        # #199: `impl Probe for PathBuf { fn get(&self) }` in another file is project code behind a std name.
        extension = 'use std::path::PathBuf;\npub trait Probe {\n    fn get(&self);\n}\nimpl Probe for PathBuf {\n    fn get(&self) {\n        let _ = std::fs::read(self);\n    }\n}\n'
        user = 'use std::path::PathBuf;\nmod extension;\nuse extension::Probe;\n#[test]\nfn fixture() {\n    for path in ["docs/x.md"] {\n        let p = PathBuf::from(path);\n        p.%s();\n    }\n}\n'
        files = {"crates/rsid/src/bin/tool/extension.rs": extension}
        self.assertEqual(self.synthetic_path_filters(user % "get", files), ["rsid=bin:tool"])
        # Controls: a std method the extension does not define, in the same fixture, reads nothing.
        self.assertEqual(self.synthetic_path_filters(user % "clone", files), [])

    def test_a_re_exported_alias_of_an_extension_trait_keeps_the_reader(self):
        # The trait is imported under another name (`pub use self::Probe as Access`): no scope guess is needed,
        # because any method a trait declares is never trusted as the standard one.
        extension = ('use std::path::PathBuf;\npub trait Probe {\n    fn get(&self);\n}\n'
                     'impl Probe for PathBuf {\n    fn get(&self) {\n        let _ = std::fs::read(self);\n    }\n}\n'
                     'pub use self::Probe as Access;\n')
        user = ('use std::path::PathBuf;\nmod extension;\nuse extension::Access;\n#[test]\nfn fixture() {\n'
                '    for path in ["docs/x.md"] {\n        let p = PathBuf::from(path);\n        p.get();\n    }\n}\n')
        self.assertEqual(self.synthetic_path_filters(user, {"crates/rsid/src/bin/tool/extension.rs": extension}), ["rsid=bin:tool"])

    def test_a_default_method_of_a_trait_with_a_blanket_impl_keeps_the_reader(self):
        extension = ('pub trait Probe {\n    fn get(&self) {\n        let _ = std::fs::read("docs/x.md");\n    }\n}\n'
                     'impl<T> Probe for T {}\npub use self::Probe as Access;\n')
        user = ('mod extension;\nuse extension::Access;\n#[test]\nfn fixture() {\n    for path in ["docs/x.md"] {\n'
                '        let p = String::from(path);\n        p.get();\n    }\n}\n')
        self.assertEqual(self.synthetic_path_filters(user, {"crates/rsid/src/bin/tool/extension.rs": extension}), ["rsid=bin:tool"])

    def test_a_std_name_beside_an_import_from_a_foreign_crate_keeps_the_reader(self):
        # `use itertools::Itertools` could add `join` and friends; the walk cannot see an unloaded crate.
        reader = ('use itertools::Itertools;\n#[test]\nfn fixture() {\n    for path in ["docs/x.md"] {\n'
                  '        let names = vec![path];\n        assert!(!names.iter().join(",").is_empty());\n    }\n}\n')
        self.assertEqual(self.synthetic_path_filters(reader), ["rsid=bin:tool"])

    def test_a_trait_implemented_for_a_std_type_in_the_prelude_keeps_the_reader(self):
        # `impl From<Marker> for PathBuf` runs project code at `PathBuf::from(marker)`.
        extension = 'use std::path::PathBuf;\npub struct Marker(pub &\'static str);\nimpl From<Marker> for PathBuf {\n    fn from(marker: Marker) -> PathBuf {\n        let _ = std::fs::read(marker.0);\n        PathBuf::new()\n    }\n}\n'
        user = 'use std::path::PathBuf;\nmod extension;\n#[test]\nfn fixture() {\n    for path in ["docs/x.md"] {\n        let _ = PathBuf::from(extension::Marker(path));\n    }\n}\n'
        self.assertEqual(self.synthetic_path_filters(user, {"crates/rsid/src/bin/tool/extension.rs": extension}), ["rsid=bin:tool"])

    def test_a_path_list_handed_to_a_reader_in_another_file_selects_its_bin(self):
        filters = self.synthetic_path_filters(
            'mod loader;\n#[test]\nfn t() {\n    loader::load_docs(&["docs/x.md"]);\n}\n',
            {"crates/rsid/src/bin/tool/loader.rs": "pub fn load_docs(paths: &[&str]) {\n    for p in paths { let _ = std::fs::read_to_string(p); }\n}\n"})
        self.assertEqual(filters, ["rsid=bin:tool"])

    def test_a_path_list_handed_to_an_in_memory_helper_in_another_file_selects_no_bin(self):
        filters = self.synthetic_path_filters(
            'mod classify;\n#[test]\nfn t() {\n    assert!(classify::docs_only(&["docs/x.md"]));\n}\n',
            {"crates/rsid/src/bin/tool/classify.rs": 'pub fn docs_only(paths: &[&str]) -> bool {\n    paths.iter().all(|p| p.starts_with("docs/"))\n}\n'})
        self.assertEqual(filters, [])

    def test_a_synthetic_list_beside_a_real_read_of_the_same_file_selects_its_bin(self):
        filters = self.synthetic_path_filters(
            '#[test]\nfn fabricated() {\n    assert!(classify(&["docs/x.md"]));\n}\n'
            "fn classify(paths: &[&str]) -> bool { !paths.is_empty() }\n"
            '#[test]\nfn real() {\n    let _ = std::fs::read_to_string(root.join("docs/x.md"));\n}\n')
        self.assertEqual(filters, ["rsid=bin:tool"])

    def test_reader_bin_in_another_crate_keeps_its_own_package(self):
        # A changed file of crate A read by a bin of crate B selects `B=bin:NAME`, never
        # `A=bin:NAME` (the lander would run `-p A --bin NAME`, which does not exist).
        filters = self.filters_after(
            {"crates/rsi-common/AGENTS.md": "old\n",
             "crates/rsid/src/bin/tool.rs": 'fn main() {}\n#[test]\nfn t() { let _ = include_str!("../../../rsi-common/AGENTS.md"); }\n'},
            {"crates/rsi-common/AGENTS.md": "new\n"})
        self.assertEqual(filters, ["rsid=bin:tool"])

    def test_failed_shard_compile_fails_the_receipt(self):
        self.write("crates/rsid/src/foo.rs", GATE.format(shard="store-02", name="a") + "const EDIT: u8 = 1;\n")
        self.commit()
        code, summary, receipt = self.check(cargo="false")
        self.assertEqual(code, 1)
        self.assertEqual(receipt["shards"]["compiled_failed"], ["store-02"])
        self.assertIn("FAIL", summary)

    def test_new_migration_numbering_and_dynamic_sql_are_reported(self):
        self.write("crates/rsid/src/store/migrations/v152.rs", "// skips 151\n")
        self.write("crates/rsid/src/foo.rs", GATE.format(shard="store-02", name="a") + 'fn q(t: &str) -> String { format!("SELECT * FROM {t}") }\n')
        self.commit()
        code, _, receipt = self.check("--no-compile")
        self.assertEqual(receipt["migrations"], {"new": [152], "base_head": 150, "numbering_conflict": True, "pending_predecessors": [], "stray_pre_split": []})
        self.assertEqual(receipt["audits"]["dynamic_sql"]["verdict"], "flag")
        self.assertEqual(code, 1)

    def test_conflicting_candidate_is_not_merge_clean_and_pins_follow_touched_files(self):
        self.git("checkout", "-q", "base")
        self.write("crates/rsi/src/key_tables.rs", "// base\n")
        self.commit("base2")
        self.git("checkout", "-q", "feature")
        self.write("crates/rsi/src/key_tables.rs", "// feature\n")
        self.commit()
        code, _, receipt = self.check("--no-compile")
        self.assertFalse(receipt["merge_clean"])
        self.assertIn("rsi=manual::", receipt["suggested_filters"])
        self.assertEqual(code, 1)

    def test_a_path_mounted_test_module_gets_its_mounted_module_path(self):
        # #1260/#1253: `#[path = "manager_issue_worker.rs"] mod issue_worker;` in
        # tests/manager_actions.rs makes the tests `...::manager_actions::issue_worker`,
        # not `...::manager_issue_worker`; the suggested filter must select them.
        self.write("crates/rsid/src/lib.rs", "pub mod session;\n")
        self.write("crates/rsid/src/session/mod.rs", "mod tests;\n")
        self.write("crates/rsid/src/session/tests/mod.rs", "mod manager_actions;\n")
        self.write(
            "crates/rsid/src/session/tests/manager_actions.rs",
            GATE.format(shard="other-03", name="own")
            + '#[path = "manager_issue_worker.rs"]\nmod issue_worker;\n',
        )
        self.write("crates/rsid/src/session/tests/manager_issue_worker.rs", GATE.format(shard="other-03", name="w"))
        self.commit("mount")
        self.git("branch", "-f", "base")
        self.write("crates/rsid/src/session/tests/manager_issue_worker.rs", GATE.format(shard="other-03", name="w") + "const EDIT: u8 = 1;\n")
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(
            receipt["suggested_filters"],
            ["rsid=shard:other-03:test(session::tests::manager_actions::issue_worker)"],
        )
        self.assertEqual(receipt["dropped_filters"], [])

    def test_a_module_with_tests_mounted_by_path_from_its_parent_is_found(self):
        # global_manager_verbs.rs mounts global_manager_verbs_tests.rs: a change to
        # the parent still selects the mounted module's tests.
        self.write("crates/rsid/src/lib.rs", "pub mod session;\n")
        self.write("crates/rsid/src/session/mod.rs", "mod gm;\n")
        self.write("crates/rsid/src/session/gm.rs", 'pub fn f() {}\n#[cfg(test)]\n#[path = "gm_tests.rs"]\nmod tests;\n')
        self.write("crates/rsid/src/session/gm_tests.rs", GATE.format(shard="store-02", name="g"))
        self.commit("mount")
        self.git("branch", "-f", "base")
        self.write("crates/rsid/src/session/gm.rs", 'pub fn f() { }\n#[cfg(test)]\n#[path = "gm_tests.rs"]\nmod tests;\n')
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["shards"]["compiled_ok"], ["store-02"])
        self.assertEqual(receipt["suggested_filters"], ["rsid=shard:store-02:test(session::gm)"])

    def test_a_filter_that_matches_no_test_is_dropped_and_reported(self):
        # A helper gated for store-02 holds no test: `test(foo)` on store-02 would
        # select nothing and fail the gate after the build (#1260).
        helper = '#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]\npub fn helper() {}\n'
        self.write("crates/rsid/src/foo.rs", GATE.format(shard="store-01", name="a") + helper)
        self.commit("helper")
        self.git("branch", "-f", "base")
        self.write("crates/rsid/src/foo.rs", GATE.format(shard="store-01", name="a") + helper + "const EDIT: u8 = 1;\n")
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["suggested_filters"], ["rsid=shard:store-01:test(foo)"])
        self.assertEqual(
            [item["filter"] for item in receipt["dropped_filters"]],
            ["rsid=shard:store-02:test(foo)"],
        )
        self.assertIn("store-02", receipt["dropped_filters"][0]["reason"])

    def test_a_module_is_filtered_under_the_package_that_owns_it(self):
        # #1227: process_control lives in rsid-core; `rsi=process_control` names no module.
        self.write("crates/rsid-core/src/lib.rs", "pub mod process_control;\n")
        self.write("crates/rsid-core/src/process_control.rs", "pub fn f() {}\n#[cfg(test)]\nmod tests { #[test] fn a() {} }\n")
        self.write("crates/rsi/src/lib.rs", "pub mod view;\n")
        self.write("crates/rsi/src/view.rs", "pub fn v() {}\n#[cfg(test)]\nmod tests { #[test] fn a() {} }\n")
        self.commit("modules")
        self.git("branch", "-f", "base")
        self.write("crates/rsid-core/src/process_control.rs", "pub fn f() { }\n#[cfg(test)]\nmod tests { #[test] fn a() {} }\n")
        self.write("crates/rsi/src/view.rs", "pub fn v() { }\n#[cfg(test)]\nmod tests { #[test] fn a() {} }\n")
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["suggested_filters"], ["rsi=view", "rsid-core=process_control"])

    def test_filters_name_only_sources_that_hold_a_runnable_test(self):
        # #2338: a `#[test]` named in a doc comment or a fixture string is not a test, and a
        # source whose every test is `#[ignore]`d runs nothing; a filter naming either selects
        # no test and the lander refuses the whole derived gate (#2244: extract::rust, s1_corpus).
        mods = ("docs_only", "string_only", "ignored_only", "mixed", "real")
        self.write("crates/codegraph/src/lib.rs", "".join(f"pub mod {m};\n" for m in mods))
        sources = {
            "docs_only": "/// Attribute tails that mark a test (`#[test]`,\n/// `#[tokio::test]`).\npub fn f() {}\n",
            "string_only": 'pub const SRC: &str = "#[test]\\nfn x() {}";\npub fn f() {}\n',
            "ignored_only": "pub fn f() {}\n#[cfg(test)]\nmod tests {\n    #[test]\n    #[ignore = \"slow\"]\n    fn slow() {}\n}\n",
            "mixed": "pub fn f() {}\n#[cfg(test)]\nmod tests {\n    #[test]\n    #[ignore = \"slow\"]\n    fn slow() {}\n\n    #[test]\n    fn fast() {}\n}\n",
            "real": "pub fn f() {}\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn a() {}\n}\n",
        }
        targets = {
            "corpus": "#[test]\n#[ignore = \"full corpus\"]\nfn s1() {}\n",
            "docs": "//! Run with `cargo test` (a `#[test]` per case).\nfn helper() {}\n",
            "live": "#[tokio::test]\nasync fn live() {}\n",
        }
        for module, text in sources.items():
            self.write(f"crates/codegraph/src/{module}.rs", text)
        for target, text in targets.items():
            self.write(f"crates/codegraph/tests/{target}.rs", text)
        self.commit("sources")
        self.git("branch", "-f", "base")
        for module, text in sources.items():
            self.write(f"crates/codegraph/src/{module}.rs", text.replace("pub fn f() {}", "pub fn f() { }"))
        for target, text in targets.items():
            self.write(f"crates/codegraph/tests/{target}.rs", text + "const TOUCHED: u8 = 1;\n")
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(sorted(receipt["suggested_filters"]),
                         ["codegraph=mixed", "codegraph=real", "codegraph=test:live"])

    def test_a_crate_root_diff_selects_the_roots_own_inline_test_module(self):
        # #2331: lib.rs has no module path, so its own `mod tests` was never selected.
        root = "pub mod child;\npub fn f() {}\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn a() { assert_eq!(1, 1); }\n}\n"
        self.write("crates/codegraph/src/lib.rs", root)
        self.write("crates/codegraph/src/child.rs", "pub fn c() {}\n#[cfg(test)]\nmod tests { #[test] fn b() {} }\n")
        self.write("crates/plain/src/lib.rs", "pub fn p() {}\n")
        self.commit("crate roots")
        self.git("branch", "-f", "base")
        self.write("crates/codegraph/src/lib.rs", root.replace("assert_eq!(1, 1)", "assert_eq!(2, 2)"))
        self.write("crates/plain/src/lib.rs", "pub fn p() { }\n")
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["suggested_filters"], ["codegraph=tests::"])

    def test_a_catalog_change_with_no_catalog_test_pins_nothing(self):
        # #1440: the pins are named tests, so a tree without any has none to drop or run.
        self.write("crates/rsid/src/rpc/agent_x.rs", GATE.format(shard="other-03", name="p"))
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["suggested_filters"], ["rsid=shard:other-03:test(rpc::agent_x)"])
        self.assertEqual(receipt["dropped_filters"], [])
        self.assertFalse(receipt["broad"])

    def gated_tests(self, shard, names, calls=None):
        calls = calls or {}
        return "".join(
            GATE.format(shard=shard, name=name).replace("{}", "{\n    " + calls[name] + "();\n}" if name in calls else "{}")
            for name in names
        )

    def session_fixture(self, count=70):
        """A `session` module whose `big` submodule holds `count` shard tests (more than BROAD_TEST_LIMIT)."""
        self.write("crates/rsid/src/lib.rs", "pub mod session;\n")
        self.write("crates/rsid/src/session/mod.rs", "mod big;\npub const LIMIT: u32 = 1;\npub fn used() {}\npub fn unused() {}\n")
        names = [f"t{n}" for n in range(count)]
        self.write("crates/rsid/src/session/big.rs", self.gated_tests("other-03", names, {"t3": "used", "t9": "used"}))
        self.commit("session fixture")
        self.git("branch", "-f", "base")

    def test_one_function_edit_of_a_big_module_selects_only_the_tests_that_mention_it(self):
        # #1438: a one-line session/mod.rs edit pinned `test(session)` in every session shard.
        self.session_fixture()
        self.write("crates/rsid/src/session/mod.rs", "mod big;\npub const LIMIT: u32 = 1;\npub fn used() { }\npub fn unused() {}\n")
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["suggested_filters"], [
            "rsid=shard:other-03:test(/^session::big::(?:\\w+::)*(?:t3|t9)$/)",
        ])
        self.assertFalse(receipt["broad"])
        self.assertEqual(receipt["narrowed_filters"][0]["tests"], 2)

    def test_code_observer_trace_ignores_separate_block_doc_hunks(self):
        self.session_fixture()
        path = "crates/rsid/src/session/mod.rs"
        original = (self.root / path).read_text() + "/** old guide\n * old details\n */\n"
        self.write(path, original)
        self.commit("block docs fixture")
        self.git("branch", "-f", "base")
        self.write(path, original.replace("old", "new").replace("fn used() {}", "fn used() { changed(); }"))
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertFalse(receipt["broad"])
        self.assertEqual(receipt["suggested_filters"], [
            "rsid=shard:other-03:test(/^session::big::(?:\\w+::)*(?:t3|t9)$/)"])

    def private_registry_fixture(self, extra="", helper=False, observer=False):
        # #1850/#1711: keep the public name while moving issuance into a
        # private module. Generic insert calls of other types must not fan out.
        path = "crates/rsid/src/session/mod.rs"
        before = """use std::collections::HashMap;
use uuid::Uuid;
mod tests;
pub(crate) struct AgentTokenRegistry {
    by_session: HashMap<Uuid, String>,
}
impl AgentTokenRegistry {
    pub(crate) fn insert(&mut self, token: String, id: Uuid) {
        self.by_session.insert(id, token);
    }
}
fn remint_agent_token(registry: &mut AgentTokenRegistry) {
    registry.insert(token, id);
}
"""
        after = """use std::collections::HashMap;
use uuid::Uuid;
mod tests;
mod agent_token_registry {
    use std::collections::HashMap;
    use uuid::Uuid;
    #[derive(Debug, Clone, Copy)]
    pub(crate) struct AgentTransportWitness {
        incarnation_id: Uuid,
    }
    #[derive(Debug)]
    struct AgentTokenBinding {
        token: String,
        witness: AgentTransportWitness,
    }
    pub(crate) struct AgentTokenRegistry {
        by_session: HashMap<Uuid, AgentTokenBinding>,
    }
    impl AgentTokenRegistry {
        #[cfg(test)]
        pub(crate) fn insert(&mut self, token: String, id: Uuid) {
            self.insert_binding(token, id);
        }
        fn insert_binding(&mut self, token: String, id: Uuid) {
            let binding = AgentTokenBinding { token, witness };
            self.by_session.insert(id, binding);
        }
        pub(super) fn remint(&mut self, token: String, id: Uuid) {
            self.insert_binding(token, id);
        }
        pub(crate) fn binding_for_session(&self, id: Uuid) -> AgentTransportWitness {
            self.by_session.get(id).witness
        }
    }
}
pub(crate) use agent_token_registry::AgentTokenRegistry;
fn remint_agent_token(registry: &mut AgentTokenRegistry) {
    registry.remint(token, id);
}
"""
        self.write("crates/rsid/src/lib.rs", "pub mod session;\n")
        self.write(path, before)
        if observer:
            self.write("crates/rsid/src/session/observer.rs", """struct RegistryObserver {
    registry: AgentTokenRegistry,
}
impl RegistryObserver {
    fn reconcile(&self) {
        self.registry.binding_for_session(id);
    }
}
""" + GATE.format(shard="session-04", name="registry_observer").replace(
                "{}", "{\n    RegistryObserver::new().reconcile();\n}"))
            self.write("crates/rsid/src/session/unrelated.rs", """struct OtherObserver {
    value: String,
}
impl OtherObserver {
    fn reconcile(&self) {
        other_work();
    }
}
""" + GATE.format(shard="session-04", name="other_observer").replace(
                "{}", "{\n    OtherObserver::new().reconcile();\n}"))
        tests = "".join(GATE.format(shard="session-03", name=name).replace(
            "{}", "{\n    " + body + "\n}") for name, body in (
                ("registry_bijective", "AgentTokenRegistry::default().insert(token, id);"),
                ("registry_remint", "AgentTokenRegistry::default().insert(token, id);"),
                ("registry_witness", "registry.binding_for_session(id);"),
                ("registry_cleanup", "registry.binding_for_session(id);"),
                ("continue_registers_working_agent_token", "remint_agent_token(registry);"),
                ("remint_revokes_prior_token", "remint_agent_token(registry);")))
        tests += self.gated_tests("session-03", [f"unrelated_{i}" for i in range(70)],
                                 {f"unrelated_{i}": "unrelated_insert" for i in range(70)})
        tests += "fn unrelated_insert() {\n    map.insert(token, witness);\n}\n"
        if helper:
            tests = tests.replace("AgentTokenRegistry::default().insert(token, id);", "seed_registry();", 1)
            tests += "fn seed_registry() {\n    AgentTokenRegistry::default().insert(token, id);\n}\n"
        self.write("crates/rsid/src/session/tests.rs", tests)
        self.commit("registry fixture")
        self.git("branch", "-f", "base")
        self.write(path, after + extra)
        self.commit("private registry move")
        return self.check("--filters-only")

    def test_private_registry_move_selects_its_six_real_users(self):
        code, summary, receipt = self.private_registry_fixture()
        self.assertEqual(code, 0, summary)
        self.assertFalse(receipt["broad"], receipt)
        self.assertEqual(receipt["suggested_filters"], [
            "rsid=shard:session-03:test(/^session::tests::(?:\\w+::)*(?:continue_registers_working_agent_token|registry_bijective|registry_cleanup|registry_remint|registry_witness|remint_revokes_prior_token)$/)"])

    def test_private_test_method_users_trace_through_a_fixture_helper(self):
        code, summary, receipt = self.private_registry_fixture(helper=True)
        self.assertEqual(code, 0, summary)
        self.assertFalse(receipt["broad"])
        self.assertIn("registry_bijective", " ".join(receipt["suggested_filters"]))

    def test_private_member_user_traces_its_receiver_and_cross_module_test(self):
        code, summary, receipt = self.private_registry_fixture(observer=True)
        self.assertEqual(code, 0, summary)
        self.assertFalse(receipt["broad"])
        self.assertIn("rsid=shard:session-04:test(/^session::observer::(?:\\w+::)*registry_observer$/)",
                      receipt["suggested_filters"])
        self.assertEqual(len(receipt["suggested_filters"]), 2)

    def test_private_move_with_an_unrelated_const_keeps_the_broad_fallback(self):
        code, summary, receipt = self.private_registry_fixture("pub const LIMIT: u32 = 2;\n")
        self.assertEqual(code, 0, summary)
        self.assertTrue(receipt["broad"])
        self.assertEqual(receipt["suggested_filters"], ["rsid=shard:session-03:test(session)"])

    def test_a_function_no_test_mentions_selects_no_test_and_says_so(self):
        self.session_fixture()
        self.write("crates/rsid/src/session/mod.rs", "mod big;\npub const LIMIT: u32 = 1;\npub fn used() {}\npub fn unused() { }\n")
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["suggested_filters"], [])
        self.assertEqual(receipt["narrowed_filters"][0]["tests"], 0)
        self.assertIn("compile check", receipt["narrowed_filters"][0]["reason"])
        self.assertEqual(receipt["shards"]["compiled_ok"], ["other-03"])

    def test_an_untraceable_edit_of_a_big_module_is_flagged_broad(self):
        self.session_fixture()
        self.write("crates/rsid/src/session/mod.rs", "mod big;\npub const LIMIT: u32 = 2;\npub fn used() {}\npub fn unused() {}\n")
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["suggested_filters"], ["rsid=shard:other-03:test(session)"])
        self.assertTrue(receipt["broad"])
        self.assertEqual(receipt["broad_filters"][0]["filter"], "rsid=shard:other-03:test(session)")
        self.assertEqual(receipt["broad_filters"][0]["tests"], 70)
        self.assertIn(" BROAD ", summary + " ")

    def wide_module_fixture(self, per_shard=40):
        """A `session` module whose tests sit in two shards, each under BROAD_TEST_LIMIT alone."""
        self.write("crates/rsid/src/lib.rs", "pub mod session;\n")
        self.write("crates/rsid/src/session/mod.rs", "mod big;\nmod wide;\npub const LIMIT: u32 = 1;\n")
        self.write("crates/rsid/src/session/big.rs", self.gated_tests("other-03", [f"a{n}" for n in range(per_shard)]))
        self.write("crates/rsid/src/session/wide.rs", self.gated_tests("session-01", [f"b{n}" for n in range(per_shard)]))
        self.commit("wide module fixture")
        self.git("branch", "-f", "base")
        self.write("crates/rsid/src/session/mod.rs", "mod big;\nmod wide;\npub const LIMIT: u32 = 2;\n")
        self.commit()

    def test_a_module_filter_is_sized_by_every_shard_the_real_build_runs(self):
        # #1698: a focused filter builds without shard features, so `test(session)`
        # selects the tests of every shard whatever its shard label says: 40 + 40 here.
        # Identical expressions share one unsharded binary and one estimate.
        self.wide_module_fixture()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["suggested_filters"],
                         ["rsid=shard:other-03:test(session)"])
        self.assertTrue(receipt["broad"])
        self.assertEqual([(b["filter"], b["tests"]) for b in receipt["broad_filters"]],
                         [("rsid=shard:other-03:test(session)", 80)])

    def test_explicit_sharded_execution_keeps_both_shard_expressions(self):
        self.wide_module_fixture()
        code, summary, receipt = self.check("--run-filters")
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["filter_runs"]["ok"], [
            "rsid=shard:other-03:test(session)", "rsid=shard:session-01:test(session)"])

    def test_a_module_whose_shards_together_stay_small_is_not_flagged(self):
        self.wide_module_fixture(per_shard=20)
        code, summary, receipt = self.check()
        self.assertEqual(receipt["suggested_filters"],
                         ["rsid=shard:other-03:test(session)"])
        self.assertFalse(receipt["broad"])

    def anchored_module_fixture(self):
        """`deploy` (2 tests) edited while `bridge` has a test whose name contains `deploy`."""
        self.write("crates/rsid/src/lib.rs", "pub mod deploy;\npub mod bridge;\n")
        self.write("crates/rsid/src/deploy.rs", "pub const EDIT: u8 = 1;\n" + self.gated_tests("other-02", ["a", "b"]))
        self.write("crates/rsid/src/bridge.rs", self.gated_tests("store-01", ["v95_deployed_bridge_rolls_back"]))
        self.commit("deploy fixture")
        self.git("branch", "-f", "base")
        self.write("crates/rsid/src/deploy.rs", "pub const EDIT: u8 = 2;\n" + self.gated_tests("other-02", ["a", "b"]))
        self.commit()

    def test_a_module_filter_that_also_matches_unrelated_test_names_is_anchored(self):
        # #1653: nextest's `test(deploy)` is a substring match on the whole test name, so
        # it also selected `bridge::v95_deployed_bridge_rolls_back`, a migration test of
        # another module (28 s in the real run). The filter names the module's own tree.
        self.anchored_module_fixture()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["suggested_filters"], ["rsid=shard:other-02:test(/^deploy::/)"])
        self.assertEqual(receipt["dropped_filters"], [])
        self.assertFalse(receipt["broad"])
        anchored = receipt["narrowed_filters"][0]
        self.assertEqual((anchored["module"], anchored["filter"]), ("deploy", "test(/^deploy::/)"))
        self.assertIn("bridge::v95_deployed_bridge_rolls_back", anchored["reason"])

    def test_an_anchored_module_filter_is_sized_by_its_own_tree(self):
        # The 61 `deploy` tests are the module's; the unrelated matches no longer count.
        self.write("crates/rsid/src/lib.rs", "pub mod deploy;\npub mod bridge;\n")
        self.write("crates/rsid/src/deploy.rs", "pub const EDIT: u8 = 1;\n" + self.gated_tests("other-02", [f"d{n}" for n in range(40)]))
        self.write("crates/rsid/src/bridge.rs", self.gated_tests("store-01", [f"v95_deployed_{n}" for n in range(40)]))
        self.commit("deploy fixture")
        self.git("branch", "-f", "base")
        self.write("crates/rsid/src/deploy.rs", "pub const EDIT: u8 = 2;\n" + self.gated_tests("other-02", [f"d{n}" for n in range(40)]))
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(receipt["suggested_filters"], ["rsid=shard:other-02:test(/^deploy::/)"])
        self.assertFalse(receipt["broad"], receipt["broad_filters"])

    def test_topology_verify_filter_selects_its_full_module_path_and_descendants(self):
        # #381: preserve the verifier's whole subtree without selecting a same-named
        # module elsewhere or a module whose name merely starts with `verify`.
        self.write("crates/rsid/src/topology/verify.rs",
                   "pub const EDIT: u8 = 1;\n" + self.gated_tests("other-02", ["judge_test"]))
        self.write("crates/rsid/src/topology/verify/tests.rs", self.gated_tests("other-02", ["nested_test"]))
        for module in ("topology/verify_extra", "session/topology/verify", "store/cold_archive/tests"):
            self.write(f"crates/rsid/src/{module}.rs", self.gated_tests("other-02", ["verify"]))
        self.commit("verifier module path fixture")
        self.git("branch", "-f", "base")
        self.write("crates/rsid/src/topology/verify.rs",
                   "pub const EDIT: u8 = 2;\n" + self.gated_tests("other-02", ["judge_test"]))
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["suggested_filters"], ["rsid=shard:other-02:test(/^topology::verify::/)"])
        self.assertEqual(receipt["narrowed_filters"][0]["tests"], 2)

    def test_anchored_module_filters_cover_their_own_subtree_filters(self):
        covered = runpy.run_path(str(SCRIPT))["covered_filters"]
        anchored = "rsid=shard:other-02:test(/^deploy::/)"
        inner = "rsid=shard:other-02:test(deploy::tests)"
        named = "rsid=shard:other-02:test(/^deploy::(?:\\w+::)*retry$/)"
        elsewhere = "rsid=shard:other-02:test(satellite::deploy)"
        self.assertEqual(covered([inner, named, anchored, elsewhere]), [anchored, elsewhere])

    def test_a_small_module_keeps_its_plain_module_filter(self):
        self.session_fixture(count=5)
        self.write("crates/rsid/src/session/mod.rs", "mod big;\npub const LIMIT: u32 = 2;\npub fn used() {}\npub fn unused() {}\n")
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(receipt["suggested_filters"], ["rsid=shard:other-03:test(session)"])
        self.assertFalse(receipt["broad"])
        self.assertEqual(receipt["narrowed_filters"], [])

    def ledger_observation_fixture(self):
        """#1548: a .rs parent with 90 ledger tests and cross-module consumers."""
        root = "crates/rsid-store/src/store/manager_ledger.rs"
        self.write("crates/rsid-store/src/lib.rs", "pub mod store;\n")
        self.write("crates/rsid-store/src/store/mod.rs", "pub mod manager_ledger;\n")
        original = "mod review_source;\nmod tests;\npub struct LedgerObservation {\n    pub source_commit: Option<String>,\n}\n"
        self.write(root, original)
        self.write("crates/rsid-store/src/store/manager_ledger/tests.rs",
                   self.gated_tests("store-04", [f"ledger_{i}" for i in range(90)]))
        self.write("crates/rsid-store/src/store/manager_ledger/review_source.rs", """pub struct ReclaimedReviewSource {}
pub fn manager_review_reclaimed_source() -> Option<ReclaimedReviewSource> {
    None
}
""")
        self.write("crates/rsid/src/session/review_source.rs", """fn review_proof() {
    manager_review_reclaimed_source();
}
""" + self.gated_tests("session-05", ["source_custody", "retry", "unrelated"],
                       {"source_custody": "review_proof", "retry": "review_proof"}))
        self.commit("ledger observation fixture")
        self.git("branch", "-f", "base")
        return root, original

    def test_ledger_field_and_helper_export_trace_source_custody_and_retry(self):
        root, original = self.ledger_observation_fixture()
        self.write(root, original.replace("mod tests;", "pub use review_source::ReclaimedReviewSource;\nmod tests;")
                   .replace("    pub source_commit", "    pub reclaimed_review_source: Option<ReclaimedReviewSource>,\n    pub source_commit"))
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["suggested_filters"], [
            "rsid=shard:session-05:test(/^session::review_source::(?:\\w+::)*(?:retry|source_custody)$/)",
        ])
        self.assertFalse(receipt["broad"])
        self.assertEqual(receipt["narrowed_filters"][0]["tests"], 2)

    def test_field_replacement_traces_both_old_and_new_consumers(self):
        root, original = self.ledger_observation_fixture()
        self.write(root, original.replace("source_commit", "old_proof"))
        consumer = "crates/rsid/src/session/review_source.rs"
        tests = self.gated_tests("session-05", ["old_reader", "new_reader"],
                                 {"old_reader": "old_proof", "new_reader": "new_proof"})
        self.write(consumer, tests)
        self.commit("old field")
        self.git("branch", "-f", "base")
        self.write(root, original.replace("source_commit", "new_proof"))
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["suggested_filters"], [
            "rsid=shard:session-05:test(/^session::review_source::(?:\\w+::)*(?:new_reader|old_reader)$/)",
        ])

    def test_common_field_name_retains_the_broad_fallback(self):
        root, original = self.ledger_observation_fixture()
        self.write("crates/rsid/src/session/readers.rs", self.gated_tests(
            "session-05", [f"reader_{i}" for i in range(11)],
            {f"reader_{i}": "common_field" for i in range(11)}))
        self.commit("common consumers")
        self.git("branch", "-f", "base")
        self.write(root, original.replace("    pub source_commit", "    pub common_field: String,\n    pub source_commit"))
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["suggested_filters"], ["rsid=shard:store-04:test(store::manager_ledger)"])
        self.assertTrue(receipt["broad"])

    def test_field_addition_dry_run_names_split_type_observers_and_keeps_replay(self):
        # #2142: common observation fields must resolve through the journal
        # type, while the include!-mounted migration catalog keeps replay
        # coverage without selecting the whole store namespace.
        replay_path, replay = self.rewind_fixture()
        # This minimal repository tests derivation/runner wiring, not the
        # static guards (which have their own fixture suites). Supply the
        # guard entry points that a scoped-test dry run always invokes.
        for guard in ("scripts/check-rsid-test-shards.py",
                      "tools/check-released-migrations.py",
                      "tools/check_identity_assertions.py",
                      "tools/check_starter_pins.py",
                      "tools/check_portable_shell.py"):
            self.write(guard, "raise SystemExit(0)\n")
        self.write("scripts/check-rsid-test-shards.py", "import sys\n"
                   "if '--list-shard-packages' in sys.argv:\n"
                   "    print('store-01 rsid-store\\nstore-02 rsid-store\\nsession-05 rsid')\n")
        self.write("crates/rsid-store/src/lib.rs", "pub mod store;\n")
        self.write("crates/rsid-store/src/store/mod.rs",
                   'pub mod journal;\nmod tests;\ninclude!("migrations/altered_objects.rs");\n')
        catalog = "pub const MIGRATION_ALTERED_OBJECTS: &[u32] = &[150];\n"
        catalog_path = "crates/rsid-store/src/store/migrations/altered_objects.rs"
        self.write(catalog_path, catalog)
        self.write("crates/rsid-store/src/store/migrations/v150.rs", "// v150\n")
        root = "crates/rsid-store/src/store/journal.rs"
        original = "pub struct JournalItem {\n    pub sequence: u64,\n}\n"
        self.write(root, original)
        for shard in range(1, 5):
            self.write(f"crates/rsid-store/src/store/unrelated_{shard}.rs",
                       self.gated_tests(f"store-0{shard}", [f"unrelated_{shard}_{i}" for i in range(70)]))
        consumer = "crates/rsid/src/session/journal.rs"
        observers = """fn load_journal() -> JournalItem {
    load_from_store()
}
fn observe_journal() {
    load_journal();
}
""" + self.gated_tests("session-05", ["journal_observer", "other_test"],
                       {"journal_observer": "observe_journal"})
        self.write(consumer, observers)
        self.write("crates/rsid/src/common_fields.rs", self.gated_tests(
            "other-03", [f"common_{i}" for i in range(11)],
            {f"common_{i}": "refusal_code" for i in range(11)}))
        self.commit("split journal and replay catalog")
        self.git("branch", "-f", "base")
        self.write(root, original.replace("    pub sequence", "    pub refusal_code: Option<String>,\n    pub sequence"))
        self.commit("field addition")
        code, summary, field_only = self.check("--filters-only")
        self.assertEqual(code, 0, summary)
        self.assertEqual(field_only["suggested_filters"], [
            "rsid=shard:session-05:test(/^session::journal::(?:\\w+::)*journal_observer$/)"])
        self.assertFalse(field_only["broad"])
        self.write(catalog_path, catalog.replace("150]", "150, 151]"))
        self.write(consumer, observers.replace("fn other_test() {}", "fn other_test() { changed(); }")
                   + self.gated_tests("session-05", ["z_added_acceptance"],
                                      {"z_added_acceptance": "observe_journal"}))
        self.write(replay_path, replay.replace("156;", "157;").replace("old_tail();", "new_tail();"))
        self.commit()
        code, summary, receipt = self.check("--filters-only")
        self.assertEqual(code, 0, summary)
        expected = {
            "rsid=shard:session-05:test(session::journal)",
            *[f"rsid=shard:store-01:test({name})" for name in (
                "migration_old", "migration_tail", "migration_replay", "migration_indirect",
                "schema_rewinds_correctly", "fixture_guard_rejects_drift")],
            "rsid=shard:store-02:test(external_migration)",
        }
        # The changed test module covers the journal's named observers.
        self.assertEqual(set(receipt["suggested_filters"]), expected)
        self.assertEqual(receipt["priority_filters"], [
            "rsid=shard:session-05:test(/^session::journal::(?:\\w+::)*(?:other_test|z_added_acceptance)$/)"])
        self.assertFalse(receipt["broad"])
        result = subprocess.run([
            str(SCRIPT.with_name("scoped-test")), "--repo", str(self.root),
            "--base", "base", "--dry-run", "--runner", "libtest"],
            capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        line = next(line for line in result.stdout.splitlines() if line.startswith("SCOPED_TEST_RECEIPT "))
        dry_run = json.loads(line.split(" ", 1)[1])
        self.assertEqual(set(dry_run["filters"]), expected)
        self.assertEqual(dry_run["priority_filters"], receipt["priority_filters"])

    def rotation_observation_fixture(self, observers=14):
        """Two rotation shards sharing an enum and a large private test helper."""
        self.write("crates/rsid/src/lib.rs", "pub mod session;\n")
        self.write("crates/rsid/src/session/mod.rs", "mod rotation;\n")
        for guard in ("scripts/check-rsid-test-shards.py",
                      "tools/check-released-migrations.py",
                      "tools/check_identity_assertions.py",
                      "tools/check_starter_pins.py",
                      "tools/check_portable_shell.py"):
            self.write(guard, "raise SystemExit(0)\n")
        self.write("scripts/check-rsid-test-shards.py", "import sys\n"
                   "if '--list-shard-packages' in sys.argv:\n"
                   "    print('session-03 rsid\\nsession-04 rsid')\n")
        path = "crates/rsid/src/session/rotation.rs"
        body = """#[derive(Clone, Copy)]
enum PrePublicationFailure {
    Metadata,
}
impl PrePublicationFailure {
    fn refusal_code(self) -> &'static str {
        "metadata_failed"
    }
}
fn assert_rotation(failure: PrePublicationFailure) {
""" + "    check();\n" * 65 + "    failure.refusal_code();\n}\n"
        for shard in ("session-03", "session-04"):
            body += self.gated_tests(shard, [f"observer_{shard[-2:]}_{i}" for i in range(observers)],
                                    {f"observer_{shard[-2:]}_{i}": "assert_rotation" for i in range(observers)})
            body += self.gated_tests(shard, [f"unrelated_{shard[-2:]}_{i}" for i in range(40)])
        body += self.gated_tests("session-04", ["z_changed_acceptance"])
        original = "#[cfg(test)]\nmod tests {\n" + "".join("    " + row + "\n" for row in body.splitlines()) + "}\n"
        self.write(path, original)
        # These share a method spelling, but cannot observe this enum.
        self.write("crates/rsid/src/unrelated.rs", "fn refusal_code() {}\n" + self.gated_tests(
            "session-03", [f"other_refusal_{i}" for i in range(11)],
            {f"other_refusal_{i}": "refusal_code" for i in range(11)}))
        self.commit("rotation observation fixture")
        self.git("branch", "-f", "base")
        return path, original

    def test_rotation_enum_and_large_helper_dry_run_names_real_observers(self):
        path, original = self.rotation_observation_fixture()
        self.write(path, original.replace("        Metadata,", "        ContextRead,\n        Metadata,")
                   .replace('"metadata_failed"', '"context_custody_failed"')
                   .replace("        check();", "        verify();", 1)
                   .replace("fn z_changed_acceptance() {}", "fn z_changed_acceptance() { verify(); }"))
        self.commit()
        code, summary, receipt = self.check("--filters-only")
        self.assertEqual(code, 0, summary)
        traced = runpy.run_path(str(SCRIPT))["traced_filters"]
        expected = traced([("session::rotation", f"observer_{shard[-2:]}_{i}", shard)
                           for shard in ("session-03", "session-04") for i in range(14)]
                          + [("session::rotation", "z_changed_acceptance", "session-04")])
        self.assertEqual(receipt["suggested_filters"], expected)
        self.assertFalse(receipt["broad"])
        self.assertEqual(receipt["narrowed_filters"][0]["tests"], 29)
        result = subprocess.run([str(SCRIPT.with_name("scoped-test")), "--repo", str(self.root),
                                 "--base", "base", "--dry-run", "--runner", "libtest"],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        line = next(line for line in result.stdout.splitlines() if line.startswith("SCOPED_TEST_RECEIPT "))
        dry_run = json.loads(line.split(" ", 1)[1])
        self.assertEqual(dry_run["filters"], expected)
        self.assertEqual(dry_run["priority_filters"], [
            "rsid=shard:session-04:test(/^session::rotation::(?:\\w+::)*z_changed_acceptance$/)"])

    def test_enum_only_edit_traces_through_unchanged_large_test_helper(self):
        path, original = self.rotation_observation_fixture(observers=1)
        self.write(path, original.replace("        Metadata,", "        ContextRead,\n        Metadata,"))
        self.commit()
        code, summary, receipt = self.check("--filters-only")
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["suggested_filters"], [
            "rsid=shard:session-03:test(/^session::rotation::(?:\\w+::)*observer_03_0$/)",
            "rsid=shard:session-04:test(/^session::rotation::(?:\\w+::)*observer_04_0$/)"])
        self.assertFalse(receipt["broad"])

    def test_unresolved_rotation_enum_keeps_one_conservative_module_filter(self):
        path, original = self.rotation_observation_fixture()
        self.write(path, original.replace("        Metadata,", "        ContextRead(String),\n        Metadata,")
                   .replace("fn z_changed_acceptance() {}", "fn z_changed_acceptance() { verify(); }"))
        self.commit()
        code, summary, receipt = self.check("--filters-only")
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["suggested_filters"], ["rsid=shard:session-03:test(session::rotation)"])
        self.assertTrue(receipt["broad"])
        self.assertEqual(len(receipt["broad_filters"]), 1)
        self.assertEqual(receipt["broad_filters"][0]["tests"], 109)
        self.assertEqual(receipt["merged_filters"][0]["covered_by"], receipt["suggested_filters"][0])
        self.assertEqual(receipt["priority_filters"], [
            "rsid=shard:session-04:test(/^session::rotation::(?:\\w+::)*z_changed_acceptance$/)"])
        result = subprocess.run([str(SCRIPT.with_name("scoped-test")), "--repo", str(self.root),
                                 "--base", "base", "--dry-run", "--runner", "libtest"],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        line = next(line for line in result.stdout.splitlines() if line.startswith("SCOPED_TEST_RECEIPT "))
        dry_run = json.loads(line.split(" ", 1)[1])
        self.assertEqual(dry_run["broad_filter_count"], 1)
        self.assertEqual(dry_run["broad_tests"], 109)
        self.assertEqual(dry_run["priority_filters"], receipt["priority_filters"])

    def test_duplicate_expression_keeps_different_library_package_coverage(self):
        dedup = runpy.run_path(str(SCRIPT))["unsharded_filters"]
        first = "rsid=shard:session-03:test(rotation)"
        same = first.replace("session-03", "session-04")
        other = first.replace("session-03", "store-01")
        self.assertEqual(dedup([first, same, other], {
            "session-03": ["rsid"], "session-04": ["rsid"], "store-01": ["rsid-store"]}),
            [first, other])

    def test_aggregate_gate_budget_traces_small_modules_and_merges_coverage(self):
        root, original = self.ledger_observation_fixture()
        for i in range(13):
            self.write(f"crates/rsid/src/small_{i:02d}.rs", "pub fn unobserved_%d() {}\n" % i
                       + self.gated_tests("other-03", [f"t{i}"]))
        self.commit("small inventories")
        self.git("branch", "-f", "base")
        for i in range(13):
            self.write(f"crates/rsid/src/small_{i:02d}.rs", "pub fn unobserved_%d() { }\n" % i
                       + self.gated_tests("other-03", [f"t{i}"]))
        self.write(root, original.replace("mod tests;", "pub use review_source::ReclaimedReviewSource;\nmod tests;"))
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(len(receipt["suggested_filters"]), 12)
        self.assertEqual(receipt["narrowed_filters"][-1]["tests"], 0)
        self.assertIn("aggregate gate budget", receipt["narrowed_filters"][-1]["reason"])

    def test_uncertain_aggregate_coverage_can_exceed_the_budget(self):
        for i in range(13):
            self.write(f"crates/rsid/src/small_{i:02d}.rs", "pub const LIMIT: u32 = 1;\n"
                       + self.gated_tests("other-03", [f"t{i}"]))
        self.commit("uncertain inventories")
        self.git("branch", "-f", "base")
        for i in range(13):
            self.write(f"crates/rsid/src/small_{i:02d}.rs", "pub const LIMIT: u32 = 2;\n"
                       + self.gated_tests("other-03", [f"t{i}"]))
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["suggested_filters"], [
            f"rsid=shard:other-03:test(small_{i:02d})" for i in range(13)
        ])

    def test_filter_merging_preserves_shards_and_regex_coverage(self):
        merge = runpy.run_path(str(SCRIPT))["covered_filters"]
        broad = "rsid=shard:session-05:test(session::review)"
        child = "rsid=shard:session-05:test(session::review::source)"
        named = "rsid=shard:session-05:test(/^session::review::(?:\\w+::)*retry$/)"
        other_shard = named.replace("session-05", "session-01")
        other_module = named.replace("session::review", "session::custody")
        self.assertEqual(merge([child, named, other_shard, broad, broad, other_module]),
                         [other_shard, broad, other_module])

    def test_named_filter_chunks_preserve_all_observers_within_256_characters(self):
        traced = runpy.run_path(str(SCRIPT))["traced_filters"]
        names = [f"source_custody_retry_{i}_" + "x" * 65 for i in range(3)]
        filters = traced([("session::review", name, "session-05") for name in names])
        self.assertEqual(len(filters), 2)
        self.assertTrue(all(len(value) <= 256 for value in filters))
        self.assertTrue(all(any(name in value for value in filters) for name in names))

    def manager_actions_fixture(self, own_tests):
        self.write("crates/rsid/src/lib.rs", "pub mod session;\n")
        self.write("crates/rsid/src/session/mod.rs", "mod tests;\n")
        self.write("crates/rsid/src/session/tests/mod.rs", "mod manager_actions;\n")
        self.write(
            "crates/rsid/src/session/tests/manager_actions.rs",
            self.gated_tests("other-03", own_tests) + '#[path = "manager_issue_worker.rs"]\nmod issue_worker;\n',
        )
        self.write("crates/rsid/src/session/tests/manager_issue_worker.rs", self.gated_tests("other-03", ["w1", "w2"]))
        self.commit("manager actions fixture")
        self.git("branch", "-f", "base")

    def test_a_test_module_with_submodules_selects_only_its_own_tests(self):
        # #1418: `test(session::tests::manager_actions)` is a prefix and also selected
        # every mounted submodule: 252 tests for a change to the 93 in the file itself.
        self.manager_actions_fixture(["a1", "a2"])
        self.write("crates/rsid/src/session/tests/manager_actions.rs",
                   self.gated_tests("other-03", ["a1", "a2"]) + '#[path = "manager_issue_worker.rs"]\nmod issue_worker;\nconst EDIT: u8 = 1;\n')
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["suggested_filters"], [
            "rsid=shard:other-03:test(/^session::tests::manager_actions::[A-Za-z0-9_]+$/)",
        ])
        self.assertEqual(receipt["narrowed_filters"][0]["excludes"], ["session::tests::manager_actions::issue_worker"])
        self.assertEqual(receipt["dropped_filters"], [])

    def test_a_test_module_whose_tests_all_live_in_submodules_keeps_the_prefix(self):
        self.manager_actions_fixture([])
        self.write("crates/rsid/src/session/tests/manager_actions.rs", '#[path = "manager_issue_worker.rs"]\nmod issue_worker;\nconst EDIT: u8 = 1;\n')
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(receipt["suggested_filters"], ["rsid=shard:other-03:test(session::tests::manager_actions)"])

    def test_emitted_filters_are_the_shapes_the_lander_fixture_pins(self):
        # #1510: rsi-rolling-land parses every line of this fixture (its own test,
        # `touched_shard_filter_fixture_is_accepted`). A shape the script starts to
        # emit that is missing here fails this test, so the two cannot drift apart.
        fixture = (Path(__file__).resolve().parent / "fixtures" / "touched-shard-filters.txt").read_text().splitlines()
        emitted = set()
        self.write("crates/rsid/src/rpc/agent_x.rs", GATE.format(shard="other-03", name="p"))
        self.commit()
        emitted.update(self.check()[2]["suggested_filters"])
        self.session_fixture()
        self.write("crates/rsid/src/session/mod.rs", "mod big;\npub const LIMIT: u32 = 1;\npub fn used() { }\npub fn unused() {}\n")
        self.commit()
        emitted.update(self.check()[2]["suggested_filters"])
        self.git("branch", "-f", "base")
        self.manager_actions_fixture(["a1", "a2"])
        self.write("crates/rsid/src/session/tests/manager_actions.rs",
                   self.gated_tests("other-03", ["a1", "a2"]) + '#[path = "manager_issue_worker.rs"]\nmod issue_worker;\nconst EDIT: u8 = 1;\n')
        self.commit()
        emitted.update(self.check()[2]["suggested_filters"])
        self.git("branch", "-f", "base")
        self.anchored_module_fixture()
        emitted.update(self.check()[2]["suggested_filters"])
        self.assertTrue(emitted, "the synthetic diffs emitted no filter")
        self.assertLessEqual(emitted, set(fixture), "update scripts/tests/fixtures/touched-shard-filters.txt")

    def test_a_rpc_change_pins_the_catalog_tests_by_name_not_the_rpc_tests_module(self):
        # #1440: `rpc::tests` (and `test(rpc)`) is ~240 slow fixtures; the pin is the
        # catalog / operator_only tests, and an added dispatch arm selects the tests that name it.
        arms = "".join(f'            "Verb{n}" => self.handle_{n}(),\n' for n in range(70))
        rpc = "mod tests;\nimpl RpcServer {\n    fn dispatch(&self) {\n        match request.method.as_str() {\n" + arms + "        }\n    }\n}\n"
        self.write("crates/rsid/src/lib.rs", "pub mod rpc;\n")
        self.write("crates/rsid/src/rpc.rs", rpc)
        names = [f"t{n}" for n in range(70)] + ["login_is_operator_only", "verbs_are_cataloged"]
        self.write("crates/rsid/src/rpc/tests.rs", self.gated_tests("other-03", names) +
                   GATE.format(shard="other-03", name="export_thing_is_refused").replace("{}", '{ let _ = "ExportThing"; }'))
        self.commit("rpc fixture")
        self.git("branch", "-f", "base")
        self.write("crates/rsid/src/rpc.rs", rpc.replace(
            '        }\n    }\n}\n', '            "ExportThing" => self.handle_export(),\n            // AGENT_VERBS stays unchanged\n        }\n    }\n}\n'))
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        filters = receipt["suggested_filters"]
        self.assertEqual(set(filters), {
            "rsid=shard:other-03:test(/^rpc::tests::(?:\\w+::)*export_thing_is_refused$/)",
            "rsid=shard:other-03:test(/^rpc::tests::(?:\\w+::)*(?:login_is_operator_only|verbs_are_cataloged)$/)"})
        self.assertFalse(receipt["broad"])

    def test_catalog_pins_are_split_so_every_suggested_filter_fits_the_job_wire_cap(self):
        # #1705: the pin alternation of 56 catalog tests was one 3135-character filter, over the
        # 256-byte cap AgentSubmitJob and AgentEnqueueLandingSource enforce per filter.
        rpc = "mod tests;\nimpl RpcServer {\n    fn dispatch(&self) {\n        match request.method.as_str() {\n        }\n    }\n}\n"
        self.write("crates/rsid/src/lib.rs", "pub mod rpc;\n")
        self.write("crates/rsid/src/rpc.rs", rpc)
        names = [f"agent_verb_catalog_pin_{n:02d}_" + "x" * 40 for n in range(56)]
        self.write("crates/rsid/src/rpc/tests.rs", self.gated_tests("other-01", names))
        self.commit("rpc catalog fixture")
        self.git("branch", "-f", "base")
        self.write("crates/rsid/src/rpc.rs", rpc.replace("        }\n    }\n}\n", '            "ExportThing" => self.handle_export(),\n            // AGENT_VERBS stays unchanged\n        }\n    }\n}\n'))
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        pins = [f for f in receipt["suggested_filters"] if "agent_verb_catalog_pin" in f]
        self.assertGreater(len(pins), 1)
        self.assertTrue(all(len(f.encode()) <= 256 for f in receipt["suggested_filters"]),
                        [len(f) for f in receipt["suggested_filters"]])
        self.assertTrue(all(f.startswith("rsid=shard:other-01:test(/^rpc::tests::(?:\\w+::)*") for f in pins), pins)
        for name in names:
            self.assertEqual(sum(name in f for f in pins), 1, name)

    def test_a_pin_that_cannot_be_split_below_the_wire_cap_is_named(self):
        rpc = "mod tests;\nimpl RpcServer {\n    fn dispatch(&self) {\n        match request.method.as_str() {\n        }\n    }\n}\n"
        self.write("crates/rsid/src/lib.rs", "pub mod rpc;\n")
        self.write("crates/rsid/src/rpc.rs", rpc)
        name = "agent_verb_catalog_" + "y" * 260
        self.write("crates/rsid/src/rpc/tests.rs", self.gated_tests("other-01", [name]))
        self.commit("rpc long name fixture")
        self.git("branch", "-f", "base")
        self.write("crates/rsid/src/rpc.rs", rpc.replace("        }\n    }\n}\n", '            "ExportThing" => self.handle_export(),\n            // AGENT_VERBS stays unchanged\n        }\n    }\n}\n'))
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(len(receipt["oversize_filters"]), 1)
        self.assertIn(name, receipt["oversize_filters"][0])

    def manager_spend_observer_fixture(self, with_test=True):
        # #3076: both changed inputs and the external observer are in the split store crate.
        src = "crates/rsid-store/src/"
        self.write("scripts/check-rsid-test-shards.py", CHECKER.read_text())
        self.write("crates/rsid/Cargo.toml", RSID_MANIFEST)
        self.write("crates/rsid-store/Cargo.toml", STORE_MANIFEST)
        self.write(src + "lib.rs", "pub mod model_control;\npub mod store;\n")
        self.write(src + "model_control/mod.rs", "mod cost;\n")
        self.write(src + "store/mod.rs", "mod manager_resources;\nmod portfolio_nodes;\n")
        self.write(src + "store/portfolio_nodes.rs",
                   '#[cfg(test)]\n#[path = "portfolio_delegation_tests.rs"]\nmod delegation_tests;\n')
        name = "a_manager_pass_costs_a_bounded_number_of_instructions_per_member_2494"
        self.write(src + "store/portfolio_delegation_tests.rs",
                   self.gated_tests("store-01", [name if with_test else "other_delegation_test"]))
        for path in ("model_control/cost.rs", "store/manager_resources.rs"):
            self.write(src + path, "pub fn spend() -> u8 { 1 }\n" +
                       self.gated_tests("store-01", ["own_spend_test"]))
        self.commit("split store spend observer fixture")
        self.git("branch", "-f", "base")
        return src, name

    def test_each_manager_spend_input_selects_the_external_delegation_cost_bound(self):
        src, name = self.manager_spend_observer_fixture()
        # Small module filters alone omitted this claimed regression in the #3043 gate.
        for paths in (("model_control/cost.rs",), ("store/manager_resources.rs",),
                      ("model_control/cost.rs", "store/manager_resources.rs")):
            with self.subTest(paths=paths):
                self.git("reset", "-q", "--hard", "base")
                for path in paths:
                    self.write(src + path, "pub fn spend() -> u8 { 2 }\n" +
                               self.gated_tests("store-01", ["own_spend_test"]))
                self.commit()
                code, summary, receipt = self.check("--filters-only")
                self.assertEqual(code, 0, summary)
                pin = ("rsid=shard:store-01:test(/^store::portfolio_nodes::delegation_tests::"
                       "(?:\\w+::)*" + name + "$/)")
                self.assertEqual(receipt["suggested_filters"].count(pin), 1)
                self.assertEqual(receipt["source_observer_pins"],
                                 ["store::portfolio_nodes::delegation_tests::" + name])
                self.assertEqual(receipt["source_observer_missing"], [])
                checker = runpy.run_path(str(SCRIPT))
                mapping = checker["shard_packages"](self.root)
                planner = runpy.run_path(str(SCRIPT.with_name("scoped-test")))["selection_plan"]
                plan = planner([pin], mapping)
                self.assertEqual(set(plan), {"rsid-store"})
                self.assertIsNotNone(re.fullmatch(plan["rsid-store"][0]["regex"],
                                                 "store::portfolio_nodes::delegation_tests::" + name))
                for path in paths:
                    module = path[:-3].replace("/", "::")
                    self.assertIn("rsid=shard:store-01:test(" + module + ")",
                                  receipt["suggested_filters"])

    def test_a_missing_delegation_cost_observer_is_reported_for_the_spend_input(self):
        src, name = self.manager_spend_observer_fixture(with_test=False)
        self.write(src + "store/manager_resources.rs", "pub fn spend() -> u8 { 2 }\n" +
                   self.gated_tests("store-01", ["own_spend_test"]))
        self.commit()
        code, summary, receipt = self.check("--filters-only")
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["source_observer_missing"],
                         ["store::portfolio_nodes::delegation_tests::" + name])
        self.assertIn("rsid=shard:store-01:test(store::manager_resources)", receipt["suggested_filters"])

    def manifest_dir_guard_fixture(self):
        name = "workspace_code_never_bakes_the_manifest_path_at_compile_time"
        self.write("crates/rsi-common/tests/manifest_dir_guard.rs", "#[test]\nfn " + name + "() {}\n")
        self.write("crates/rsid/tests/e2e_seed.rs", "#[test]\nfn seed() {}\n")
        self.commit("workspace manifest guard fixture")
        self.git("branch", "-f", "base")
        return name, "rsi-common=test:manifest_dir_guard:test(=" + name + ")"

    def test_scanned_rust_changes_select_one_exact_workspace_manifest_guard(self):
        name, pin = self.manifest_dir_guard_fixture()
        for path, text in (
            ("crates/rsid/tests/e2e_seed.rs", "#[test]\nfn seed() { let _x = 1; }\n"),
            ("crates/rsid/tests/e2e_seed.rs", "// a comment is scanned too\n#[test]\nfn seed() {}\n"),
            ("crates/rsi-common/src/untested.rs", "pub fn u() { let _x = 1; }\n"),
            ("crates/new-crate/examples/example.rs", "fn main() {}\n"),
        ):
            with self.subTest(path=path, text=text):
                self.git("reset", "-q", "--hard", "base")
                self.write(path, text)
                self.commit()
                code, summary, receipt = self.check("--filters-only")
                self.assertEqual(code, 0, summary)
                self.assertEqual(receipt["suggested_filters"].count(pin), 1)
                self.assertEqual(receipt["workspace_source_observers"], [pin])
                planner = runpy.run_path(str(SCRIPT.with_name("scoped-test")))["selection_plan"]
                plan = planner(receipt["suggested_filters"], {})
                selection = next(s for s in plan["rsi-common"] if s["filter"] == pin)
                self.assertEqual((selection["kind"], selection["target"], selection["exact"]),
                                 ("test", "manifest_dir_guard", name))

    def test_manifest_guard_observer_respects_the_scanners_path_exemptions(self):
        _name, _pin = self.manifest_dir_guard_fixture()
        for path in ("crates/rsid/build.rs", "crates/rsi-common/src/manifest_dir.rs",
                     "crates/rsid/target/generated.rs", "tools/example.rs", "crates/rsid/notes.md"):
            with self.subTest(path=path):
                self.git("reset", "-q", "--hard", "base")
                self.write(path, "// outside the guard's scan\n")
                self.commit()
                code, summary, receipt = self.check("--filters-only")
                self.assertEqual(code, 0, summary)
                self.assertEqual(receipt["workspace_source_observers"], [])

    def agent_schema_observer_fixture(self, with_test=True):
        # #2949: the rsid test reads the shared catalog through a function call, never include_str!.
        self.write("crates/rsi-common/src/lib.rs", "pub mod agent_control_schema;\n")
        self.write("crates/rsi-common/src/agent_control_schema.rs", "pub fn descriptor() -> &'static str { \"old\" }\n")
        self.write("crates/rsid/src/lib.rs", "pub mod rpc;\n")
        self.write("crates/rsid/src/rpc.rs", "mod tests;\n")
        names = ["operator_daemon_config_is_absent_from_every_agent_surface"] if with_test else ["unrelated"]
        self.write("crates/rsid/src/rpc/tests.rs", self.gated_tests("other-01", names + ["other_fixture"]))
        self.commit("agent schema fixture")
        self.git("branch", "-f", "base")

    def test_an_agent_control_schema_edit_selects_the_operator_only_catalog_contract_test(self):
        self.agent_schema_observer_fixture()
        self.write("crates/rsi-common/src/agent_control_schema.rs", "pub fn descriptor() -> &'static str { \"new\" }\n")
        self.commit()
        code, summary, receipt = self.check("--no-compile")
        self.assertEqual(code, 0, summary)
        pin = ("rsid=shard:other-01:test(/^rpc::tests::(?:\\w+::)*"
               "operator_daemon_config_is_absent_from_every_agent_surface$/)")
        self.assertIn(pin, receipt["suggested_filters"])
        self.assertNotIn("other_fixture", "".join(receipt["suggested_filters"]))
        self.assertEqual(receipt["source_observer_pins"],
                         ["rpc::tests::operator_daemon_config_is_absent_from_every_agent_surface"])
        self.assertEqual(receipt["source_observer_missing"], [])

    def test_an_unrelated_rsi_common_edit_does_not_select_the_catalog_contract_test(self):
        self.agent_schema_observer_fixture()
        self.write("crates/rsi-common/src/types.rs", "pub fn t() { let _x = 1; }\n#[cfg(test)]\nmod tests { #[test] fn a() {} }\n")
        self.commit()
        code, summary, receipt = self.check("--no-compile")
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["source_observer_pins"], [])
        self.assertNotIn("operator_daemon_config", "".join(receipt["suggested_filters"]))

    def test_a_missing_source_observer_test_is_named_in_the_receipt(self):
        self.agent_schema_observer_fixture(with_test=False)
        self.write("crates/rsi-common/src/agent_control_schema.rs", "pub fn descriptor() -> &'static str { \"new\" }\n")
        self.commit()
        code, summary, receipt = self.check("--no-compile")
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["source_observer_missing"],
                         ["rpc::tests::operator_daemon_config_is_absent_from_every_agent_surface"])

    def rpc_dispatch_fixture(self, count=70):
        path = "crates/rsid/src/rpc.rs"
        rpc = ("mod tests;\nimpl RpcServer {\n    fn dispatch(&self) {\n"
               "        let result = match request.method.as_str() {\n" +
               "".join(f'            "Verb{n}" => self.handle_{n}(request).await,\n' for n in range(count)) +
               "            _ => unknown(),\n        };\n    }\n}\n")
        self.write("crates/rsid/src/lib.rs", "pub mod rpc;\n")
        self.write(path, rpc)
        tests = self.gated_tests("other-03", [f"unrelated_{n}" for n in range(count)],
                                {f"unrelated_{n}": "dispatch" for n in range(count)})
        tests += ('const RPC_SOURCE: &str = include_str!("../rpc.rs");\n'
                  'const EXAMPLE: &str = r#"include_str!("../rpc.rs")"#;\n'
                  "fn dispatch_methods() {\n    let source = RPC_SOURCE;\n}\n")
        tests += self.gated_tests("other-03", ["dispatch_table_complete", "export_works", "export_refused", "rpc_catalog"],
                                 {"dispatch_table_complete": "dispatch_methods", "rpc_catalog": "dispatch_methods", "export_works": "handle_export",
                                  "export_refused": "ExportThing"})
        self.write("crates/rsid/src/rpc/tests.rs", tests)
        self.commit("dispatch fixture")
        self.git("branch", "-f", "base")
        return path, rpc

    def test_multiline_dispatch_addition_selects_item_and_table_tests(self):
        for count in (5, 70):
            with self.subTest(count=count):
                path, rpc = self.rpc_dispatch_fixture(count)
                self.write(path, rpc.replace('            _ =>',
                    '            "ExportThing" => {\n                self.handle_export(request)\n                    .await\n            }\n            _ =>'))
                self.commit()
                code, summary, receipt = self.check()
                self.assertEqual(code, 0, summary)
                self.assertFalse(receipt["broad"])
                self.assertEqual(receipt["suggested_filters"], [
                    "rsid=shard:other-03:test(/^rpc::tests::(?:\\w+::)*(?:dispatch_table_complete|export_refused|export_works|rpc_catalog)$/)"])

    def test_dispatch_prelude_and_unknown_arm_changes_keep_explicit_broad_coverage(self):
        path, rpc = self.rpc_dispatch_fixture()
        for before, after in [("        let result =", "        shared_gate();\n        let result ="),
                              ("_ => unknown()", "_ => new_unknown()")]:
            with self.subTest(change=before):
                self.write(path, rpc.replace(before, after))
                self.commit()
                code, summary, receipt = self.check()
                self.assertEqual(code, 0, summary)
                self.assertTrue(receipt["broad"])
                self.assertIn("extends beyond simple", receipt["broad_filters"][0]["reason"])
                # No cap stopped this trace: it is untraceable, so the worker keeps the broad filter (#2592).
                self.assertEqual((receipt["broad_filters"][0]["stop"], receipt["broad_filters"][0]["worker_bounded"]),
                                 (None, False))
                self.assertEqual(receipt["suggested_filters"], ["rsid=shard:other-03:test(rpc)"])

    def test_family_registration_traces_qualified_predicate_and_private_request_helpers(self):
        path, rpc = self.rpc_dispatch_fixture()
        family = ('pub(super) fn is_operator_method(method: &str) -> bool {\n'
                  '    method == "AnswerPendingDecisionV1"\n}\n'
                  'fn request() {\n    let method = "AnswerPendingDecisionV1";\n}\n')
        self.write("crates/rsid/src/rpc/sessions.rs", family + self.gated_tests(
            "other-03", ["pending_answer_admission"], {"pending_answer_admission": "request"}))
        self.write("crates/rsid/src/rpc/unrelated.rs",
                   'fn is_operator_method() {\n    other_methods();\n}\n'
                   'fn request() {\n    unrelated();\n}\n'
                   'fn fixture() {\n    request();\n}\n' +
                   self.gated_tests("other-03", [f"peer_{n}" for n in range(70)],
                                    {f"peer_{n}": "fixture" for n in range(70)}))
        self.commit("family helpers")
        self.git("branch", "-f", "base")
        self.write(path, rpc.replace('            _ =>',
            '            method if sessions::is_operator_method(method) => {\n'
            '                self.handle_answer_pending_decision(request).await\n'
            '            }\n            _ =>'))
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertFalse(receipt["broad"])
        self.assertEqual(set(receipt["suggested_filters"]), {
            "rsid=shard:other-03:test(/^rpc::sessions::(?:\\w+::)*pending_answer_admission$/)",
            "rsid=shard:other-03:test(/^rpc::tests::(?:\\w+::)*(?:dispatch_table_complete|rpc_catalog)$/)"})
        # #2247: an exported helper whose name another module defines is followed by the calls
        # that provably reach it (a local call in its own module), not by the bare name: the
        # unrelated module's own `request` stays out, the real observer stays in.
        exported = (family + self.gated_tests(
            "other-03", ["pending_answer_admission"], {"pending_answer_admission": "request"})
                    ).replace("fn request()", "pub(crate) fn request()")
        self.write("crates/rsid/src/rpc/sessions.rs", exported)
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertFalse(receipt["broad"])
        self.assertEqual(set(receipt["suggested_filters"]), {
            "rsid=shard:other-03:test(rpc::sessions)",
            "rsid=shard:other-03:test(/^rpc::tests::(?:\\w+::)*(?:dispatch_table_complete|rpc_catalog)$/)"})
        # #2592: `use ..::request as other` is followed to its item, so an alias of the real
        # helper is one more caller (it has no test here, so the observers do not change).
        self.write("crates/rsid/src/rpc/loader.rs",
                   "use super::sessions::request as session_request;\npub(crate) fn load() {\n    session_request();\n}\n")
        self.commit("renaming import")
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertFalse(receipt["broad"], receipt["broad_filters"])
        self.assertEqual(set(receipt["suggested_filters"]), {
            "rsid=shard:other-03:test(rpc::sessions)",
            "rsid=shard:other-03:test(/^rpc::tests::(?:\\w+::)*(?:dispatch_table_complete|rpc_catalog)$/)"})

    def test_path_mounted_test_files_resolve_through_sibling_directories_and_chains(self):
        # #1263: `#[path]` files (a `../` mount, then a mount from that mounted file) get the
        # module path the compiler gives their tests, never a file-path filter that matches none.
        self.write("crates/rsid/src/lib.rs", "pub mod session;\n")
        self.write("crates/rsid/src/session/mod.rs", "mod tests;\n")
        self.write("crates/rsid/src/session/tests/mod.rs", '#[cfg(test)]\n#[path = "../shared_fixtures/portfolio.rs"]\nmod portfolio;\n')
        self.write("crates/rsid/src/session/shared_fixtures/portfolio.rs", '#[path = "portfolio_levels.rs"]\nmod levels;\n')
        self.write("crates/rsid/src/session/shared_fixtures/portfolio_levels.rs", self.gated_tests("other-03", ["l1"]))
        self.commit("mounts")
        self.git("branch", "-f", "base")
        self.write("crates/rsid/src/session/shared_fixtures/portfolio_levels.rs", self.gated_tests("other-03", ["l1"]) + "const EDIT: u8 = 1;\n")
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["suggested_filters"], ["rsid=shard:other-03:test(session::tests::portfolio::levels)"])
        self.assertEqual(receipt["dropped_filters"], [])

    def include_fixture(self, binding="const TEXT: &str = include_str!(\"guidance/w.md\");", render_tests=("t_render",), extra=None):
        # #2057: a guidance markdown file is observed through the const that include_str!s it.
        self.write("crates/rsid/src/lib.rs", "pub mod session;\n")
        self.write("crates/rsid/src/session/mod.rs", "mod preamble;\nmod other;\npub fn central() { preamble::render(); }\n")
        self.write("crates/rsid/src/session/guidance/w.md", "old\n")
        calls = {name: "render" for name in render_tests}
        self.write("crates/rsid/src/session/preamble.rs",
                   binding + "\npub fn render() -> usize { TEXT.len() }\n"
                   + self.gated_tests("other-03", [*render_tests, "t_unrelated"], calls))
        self.write("crates/rsid/src/session/other.rs",
                   self.gated_tests("other-03", ["t_central", "t_far", "t_other"], {"t_central": "central", "t_far": "t_central"}))
        for path, text in (extra or {}).items():
            self.write(path, text)
        self.commit("include fixture")
        self.git("branch", "-f", "base")
        self.write("crates/rsid/src/session/guidance/w.md", "new\n")
        self.commit()

    def test_included_data_file_selects_the_tests_of_its_const_instead_of_the_module_closure(self):
        self.include_fixture()
        code, summary, receipt = self.check("--filters-only")
        self.assertEqual(code, 0, summary)
        # t_render calls render, t_central calls the one outside caller; t_far and t_unrelated do not read the text.
        self.assertEqual(receipt["suggested_filters"], [
            "rsid=shard:other-03:test(/^session::other::(?:\\w+::)*t_central$/)",
            "rsid=shard:other-03:test(/^session::preamble::(?:\\w+::)*t_render$/)",
        ])
        self.assertFalse(receipt["broad"])

    def test_included_data_file_with_an_unresolvable_reader_keeps_the_lander_selection(self):
        self.include_fixture(binding='const TEXT: &str = concat!(include_str!("guidance/w.md"), "x");')
        code, summary, receipt = self.check("--filters-only")
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["suggested_filters"], [])

    def test_included_data_file_whose_tests_exceed_the_bound_keeps_the_lander_selection(self):
        self.include_fixture(render_tests=tuple(f"t{n}" for n in range(70)))
        code, summary, receipt = self.check("--filters-only")
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["suggested_filters"], [])

    def test_included_data_file_also_read_by_a_path_literal_keeps_the_lander_selection(self):
        self.include_fixture(extra={"crates/rsid/src/session/other.rs": self.gated_tests(
            "other-03", ["t_read"]).replace("fn t_read() {}", 'fn t_read() { let _ = "guidance/w.md"; }')})
        code, summary, receipt = self.check("--filters-only")
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["suggested_filters"], [])


CHECKER = Path(__file__).resolve().parents[1] / "check-rsid-test-shards.py"
RSID_MANIFEST = (
    '[features]\ndefault = []\ntest-shard-mode = []\n'
    'test-shard-store-02 = ["test-shard-mode"]\ntest-shard-other-03 = ["test-shard-mode"]\n'
)
STORE_MANIFEST = (
    '[features]\ndefault = []\ntest-shard-mode = []\n'
    'test-shard-store-01 = ["test-shard-mode"]\ntest-shard-store-02 = ["test-shard-mode"]\n'
)


class ExportedHelperIdentityTest(unittest.TestCase):
    """#2247: an exported helper is followed by the calls that provably reach it, not by its name.

    `dialectic::tools::execute_tool` and the private `openai::execute_tool` share a name; the
    trace used to give up on every such pair and keep the whole changed module as a broad
    filter. A call resolves from where it is written (a qualified path, an import, a definition in
    its own module, an inline module); only a reference this scan cannot place keeps the broad
    fallback, and it says which one."""

    HELPER = "pub fn changed_helper() -> u32 {\n    1\n}\n"
    UNRELATED = "".join(GATE.format(shard="other-01", name=f"core_filler_{n}") for n in range(70))

    def setUp(self):
        self.temp = scratch_tempdir(prefix="exported-helper-identity-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.git("init", "-q", "-b", "rolling")
        self.git("config", "user.name", "T")
        self.git("config", "user.email", "t@example.invalid")
        self.write("crates/rsid/src/lib.rs", "pub mod core;\npub mod dialectic;\npub mod local;\npub mod openai;\npub mod unique;\n")
        self.write("crates/rsid/src/core.rs", self.HELPER + self.UNRELATED)
        self.write("crates/rsid/src/unique.rs", "pub struct Unique;\n")
        self.write("crates/rsid/src/dialectic/mod.rs", "pub mod tools;\n\npub fn run_dialectic() {\n    tools::execute_tool();\n}\n"
                   + self.gated("dialectic_observes_the_helper", "run_dialectic"))
        self.write("crates/rsid/src/dialectic/tools.rs", "pub fn execute_tool() {\n    changed_helper();\n}\n")
        self.write("crates/rsid/src/openai.rs", "fn execute_tool() {\n    other();\n}\n\nfn openai_turn() {\n    execute_tool();\n}\n"
                   + self.gated("openai_turn_is_unrelated", "openai_turn"))
        self.write("crates/rsid/src/local.rs", "fn variable_user() {\n    let execute_tool = 1;\n    let _ = execute_tool + 1;\n}\n"
                   + self.gated("a_local_variable_is_unrelated", "variable_user"))
        self.git("add", ".")
        self.git("commit", "-q", "-m", "base")
        self.git("branch", "base")
        self.git("checkout", "-q", "-b", "feature")

    def gated(self, name, call):
        return GATE.format(shard="other-01", name=name).replace("{}", "{\n    " + call + "();\n}")

    def git(self, *args):
        scratch_git(self.root, *args)

    def write(self, rel, text):
        path = self.root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def commit(self, msg="change"):
        self.git("add", ".")
        self.git("commit", "-q", "-m", msg)

    def receipt(self):
        """Derive filters for the one-line edit of `changed_helper` (no compile)."""
        self.write("crates/rsid/src/core.rs", self.HELPER.replace("    1\n", "    2\n") + self.UNRELATED)
        self.commit()
        result = subprocess.run([str(SCRIPT), "--filters-only", "--repo", str(self.root), "--base", "base"],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout.partition("\n")[2])

    def observers(self, receipt):
        return " ".join(receipt["suggested_filters"])

    def test_a_qualified_call_reaches_its_helper_and_a_same_name_private_one_stays_out(self):
        receipt = self.receipt()
        self.assertFalse(receipt["broad"], receipt["broad_filters"])
        observed = self.observers(receipt)
        self.assertIn("dialectic_observes_the_helper", observed)
        self.assertNotIn("openai_turn_is_unrelated", observed)
        self.assertNotIn("a_local_variable_is_unrelated", observed)
        self.assertFalse(any("core_filler" in f for f in receipt["suggested_filters"]))

    def test_an_explicit_import_and_a_glob_of_the_owner_both_reach_the_helper(self):
        self.write("crates/rsid/src/imports.rs", "use crate::dialectic::tools::execute_tool;\n\npub fn via_import() {\n    execute_tool();\n}\n"
                   + self.gated("import_observes_the_helper", "via_import"))
        self.write("crates/rsid/src/dialectic/tools/tests.rs", "use super::*;\n\nfn drives() {\n    execute_tool();\n}\n"
                   + self.gated("glob_observes_the_helper", "drives"))
        self.write("crates/rsid/src/lib.rs", "pub mod core;\npub mod dialectic;\npub mod imports;\npub mod local;\npub mod openai;\npub mod unique;\n")
        self.commit("imports")
        self.git("branch", "-f", "base")
        receipt = self.receipt()
        self.assertFalse(receipt["broad"], receipt["broad_filters"])
        observed = self.observers(receipt)
        for name in ("dialectic_observes_the_helper", "import_observes_the_helper", "glob_observes_the_helper"):
            self.assertIn(name, observed)
        self.assertNotIn("openai_turn_is_unrelated", observed)

    def test_an_inline_module_helper_is_reached_through_its_qualified_path(self):
        self.write("crates/rsid/src/dialectic/mod.rs", "pub mod tools;\n\nfn archive() {\n    seam::execute_tool();\n}\n\n"
                   "pub(crate) mod seam {\n    pub(super) fn execute_tool() {\n        changed_helper();\n    }\n}\n"
                   + self.gated("seam_observes_the_helper", "archive"))
        self.write("crates/rsid/src/dialectic/tools.rs", "pub fn execute_tool() {\n    other();\n}\n")
        self.commit("inline seam")
        self.git("branch", "-f", "base")
        receipt = self.receipt()
        self.assertFalse(receipt["broad"], receipt["broad_filters"])
        observed = self.observers(receipt)
        self.assertIn("seam_observes_the_helper", observed)
        self.assertNotIn("openai_turn_is_unrelated", observed)

    BASE_MODULES = ("core", "dialectic", "local", "openai", "unique")

    def mods(self, *extra):
        names = sorted(set(self.BASE_MODULES) | set(extra))
        self.write("crates/rsid/src/lib.rs", "".join(f"pub mod {name};\n" for name in names))

    def consumer(self, module, path, test):
        """A module whose function reaches `execute_tool` through `path`, with a gated test over it."""
        self.write(f"crates/rsid/src/{module}.rs", f"pub fn via_{module}() {{\n    {path}();\n}}\n"
                   + self.gated(test, f"via_{module}"))

    def unrelated_same_name(self, module="stranger"):
        """A third `execute_tool`, exported by a module the changed helper never reaches."""
        self.write(f"crates/rsid/src/{module}.rs", "pub fn execute_tool() {\n    other();\n}\n")

    def resolved(self, receipt, *observers):
        self.assertFalse(receipt["broad"], receipt["broad_filters"])
        observed = self.observers(receipt)
        for name in observers:
            self.assertIn(name, observed)
        self.assertNotIn("openai_turn_is_unrelated", observed)
        self.assertNotIn("stranger_is_unrelated", observed)
        self.assertFalse(any("core_filler" in f for f in receipt["suggested_filters"]))

    def broad(self, receipt, *reasons):
        """The conservative answer: broad, the reason named, the changed module's own tests still selected."""
        self.assertTrue(receipt["broad"])
        reason = receipt["broad_filters"][0]["reason"]
        self.assertIn("ambiguous exported helper: dialectic::tools::execute_tool", reason)
        for part in reasons:
            self.assertIn(part, reason)
        self.assertTrue(any("core" in f for f in receipt["suggested_filters"]), receipt["suggested_filters"])

    def test_a_pub_use_reexport_chain_reaches_the_helper_and_a_same_name_item_stays_out(self):
        self.write("crates/rsid/src/dialectic/mod.rs", "pub mod tools;\npub use tools::execute_tool;\n\npub fn run_dialectic() {\n"
                   "    tools::execute_tool();\n}\n" + self.gated("dialectic_observes_the_helper", "run_dialectic"))
        self.write("crates/rsid/src/facade.rs", "pub use crate::dialectic::execute_tool;\n")
        self.consumer("plugins", "crate::dialectic::execute_tool", "plugins_observe_the_helper")
        self.consumer("deep", "crate::facade::execute_tool", "a_two_hop_reexport_observes_the_helper")
        self.unrelated_same_name()
        self.consumer("stray", "crate::stranger::execute_tool", "stranger_is_unrelated")
        self.mods("facade", "plugins", "deep", "stranger", "stray")
        self.commit("re-export chain")
        self.git("branch", "-f", "base")
        self.resolved(self.receipt(), "dialectic_observes_the_helper", "plugins_observe_the_helper",
                      "a_two_hop_reexport_observes_the_helper")

    def test_a_reexported_module_and_a_glob_chain_reach_the_helper(self):
        self.write("crates/rsid/src/api.rs", "pub use crate::dialectic::tools;\n")
        self.write("crates/rsid/src/facade.rs", "pub use crate::dialectic::tools::*;\n")
        self.write("crates/rsid/src/prelude.rs", "pub use crate::facade::*;\n")
        self.consumer("through_module", "crate::api::tools::execute_tool", "a_reexported_module_observes_the_helper")
        self.write("crates/rsid/src/through_glob.rs", "use crate::prelude::*;\n\npub fn via_through_glob() {\n    execute_tool();\n}\n"
                   + self.gated("a_glob_chain_observes_the_helper", "via_through_glob"))
        self.unrelated_same_name()
        self.write("crates/rsid/src/stray.rs", "use crate::stranger::*;\n\npub fn via_stray() {\n    execute_tool();\n}\n"
                   + self.gated("stranger_is_unrelated", "via_stray"))
        self.mods("api", "facade", "prelude", "through_module", "through_glob", "stranger", "stray")
        self.commit("module and glob chain")
        self.git("branch", "-f", "base")
        self.resolved(self.receipt(), "a_reexported_module_observes_the_helper", "a_glob_chain_observes_the_helper")

    def test_an_explicit_alias_of_the_helper_is_a_caller_and_an_alias_of_a_same_name_item_is_not(self):
        self.write("crates/rsid/src/plugins.rs", "use crate::dialectic::tools::execute_tool as run_tool;\n\npub fn via_plugins() {\n    run_tool();\n}\n"
                   + self.gated("an_alias_observes_the_helper", "via_plugins"))
        self.write("crates/rsid/src/aliased_facade.rs", "pub use crate::dialectic::tools::execute_tool as launch;\n")
        self.consumer("second_hop", "crate::aliased_facade::launch", "a_reexported_alias_observes_the_helper")
        self.unrelated_same_name()
        self.write("crates/rsid/src/stray.rs", "use crate::stranger::execute_tool as run_tool;\n\npub fn via_stray() {\n    run_tool();\n}\n"
                   + self.gated("stranger_is_unrelated", "via_stray"))
        self.mods("plugins", "aliased_facade", "second_hop", "stranger", "stray")
        self.commit("aliases")
        self.git("branch", "-f", "base")
        self.resolved(self.receipt(), "an_alias_observes_the_helper", "a_reexported_alias_observes_the_helper")

    def test_a_module_named_like_the_helper_is_a_path_segment_not_a_reference(self):
        # The #1186 shape: `crate::topology::resolve::summary` names the module `resolve`, which
        # is not the exported function `resolve` of another module.
        self.write("crates/rsid/src/exec/mod.rs", "pub mod execute_tool;\n\npub fn summarize() {\n    crate::exec::execute_tool::summary();\n}\n"
                   + self.gated("a_module_segment_is_unrelated", "summarize"))
        self.write("crates/rsid/src/exec/execute_tool.rs", "pub fn summary() {}\n")
        self.mods("exec")
        self.commit("module segment")
        self.git("branch", "-f", "base")
        receipt = self.receipt()
        self.resolved(receipt)
        self.assertNotIn("a_module_segment_is_unrelated", self.observers(receipt))

    def test_imported_topology_verify_module_preserves_observers_without_claiming_cold_archive_verify(self):
        # #381: `verify` in `verify::judge` names the imported module, even though
        # store::cold_archive::tests exports a helper with that same name.
        for module, children in (("store", "cold_archive"), ("store/cold_archive", "tests"),
                                 ("topology", "verify"), ("session", "manager_ledger"),
                                 ("session/manager_ledger", "job_evidence")):
            self.write(f"crates/rsid/src/{module}/mod.rs", f"pub mod {children};\n")
        self.write("crates/rsid/src/store/cold_archive/tests.rs",
                   "pub struct Fixture;\n\nimpl Fixture {\n    pub fn verify(&self) {\n        changed_helper();\n    }\n}\n"
                   + GATE.format(shard="other-01", name="cold_archive_observes_verify").replace(
                       "{}", "{\n    Fixture::verify(&Fixture);\n}"))
        self.write("crates/rsid/src/topology/verify.rs",
                   "pub fn judge() {\n    changed_helper();\n}\n")
        self.write("crates/rsid/src/unique.rs", "pub fn verify() {}\n")
        self.write("crates/rsid/src/session/manager_ledger/job_evidence.rs",
                   "use crate::topology::verify::{self, Reason};\n\n"
                   "pub fn judged() {\n    verify::judge();\n}\n"
                   + self.gated("job_evidence_observes_topology_verifier", "judged"))
        self.mods("store", "topology", "session")
        self.commit("topology verifier module and cold archive helper")
        self.git("branch", "-f", "base")
        receipt = self.receipt()
        self.resolved(receipt, "cold_archive_observes_verify", "job_evidence_observes_topology_verifier")
        planner = runpy.run_path(str(SCRIPT))
        sources = planner["SourceTree"](self.root, "HEAD", planner["git"](
            self.root, "ls-tree", "-r", "--name-only", "HEAD").stdout.splitlines())
        references = planner["ItemReferences"](sources)
        self.assertEqual(references.callers("store::cold_archive::tests", "verify"),
                         {("store::cold_archive::tests", "cold_archive_observes_verify")})
        self.assertEqual(references.callers("topology::verify", "judge"),
                         {("session::manager_ledger::job_evidence", "judged")})

    def test_a_reexport_cycle_keeps_the_broad_fallback_and_names_it(self):
        self.write("crates/rsid/src/ping.rs", "pub use crate::pong::execute_tool;\n")
        self.write("crates/rsid/src/pong.rs", "pub use crate::ping::execute_tool;\n")
        self.consumer("plugins", "crate::ping::execute_tool", "plugins_observe_the_helper")
        self.mods("ping", "pong", "plugins")
        self.commit("cycle")
        self.git("branch", "-f", "base")
        self.broad(self.receipt(), "plugins::via_plugins", "a re-export cycle")

    def test_a_chain_two_items_can_supply_keeps_the_broad_fallback_and_names_it(self):
        self.unrelated_same_name()
        self.write("crates/rsid/src/facade.rs", "pub use crate::dialectic::tools::*;\npub use crate::stranger::*;\n")
        self.consumer("plugins", "crate::facade::execute_tool", "plugins_observe_the_helper")
        self.mods("facade", "plugins", "stranger")
        self.commit("two suppliers")
        self.git("branch", "-f", "base")
        self.broad(self.receipt(), "plugins::via_plugins", "may name 2 different items")

    def test_a_reexport_chain_past_the_depth_bound_keeps_the_broad_fallback_and_names_it(self):
        hops = [f"hop{n}" for n in range(8)]
        for here, there in zip(hops, hops[1:] + ["dialectic"]):
            self.write(f"crates/rsid/src/{here}.rs", f"pub use crate::{there}::execute_tool;\n")
        self.write("crates/rsid/src/dialectic/mod.rs", "pub mod tools;\npub use tools::execute_tool;\n")
        self.consumer("plugins", "crate::hop0::execute_tool", "plugins_observe_the_helper")
        self.mods("plugins", *hops)
        self.commit("long chain")
        self.git("branch", "-f", "base")
        self.broad(self.receipt(), "plugins::via_plugins", "deeper than")

    def test_a_reexport_of_no_known_module_keeps_the_broad_fallback_and_names_it(self):
        self.write("crates/rsid/src/facade.rs", "pub use crate::missing::execute_tool;\n")
        self.consumer("plugins", "crate::facade::execute_tool", "plugins_observe_the_helper")
        self.mods("facade", "plugins")
        self.commit("unplaceable")
        self.git("branch", "-f", "base")
        self.broad(self.receipt(), "plugins::via_plugins", "that names no known module")

    def test_an_alias_of_a_module_this_scan_cannot_place_keeps_the_broad_fallback(self):
        self.write("crates/rsid/src/plugins.rs", "use crate::missing::execute_tool as run_tool;\n\npub fn load() {\n    run_tool();\n}\n")
        self.mods("plugins")
        self.commit("unplaceable alias")
        self.git("branch", "-f", "base")
        self.broad(self.receipt(), "renames it in a `use` of a module this scan cannot place")

    def test_a_type_declaration_quoted_in_a_string_does_not_make_the_type_ambiguous(self):
        # A source-scanner test quotes `pub struct Builder {`; that is not a second declaration.
        self.write("crates/rsid/src/builder.rs",
                   "pub struct Builder<T> {\n    value: T,\n}\n\nimpl<T> Builder<T>\nwhere\n    T: Copy,\n{\n"
                   "    pub fn new(value: T) -> Self {\n        changed_helper();\n        Self { value }\n    }\n}\n\n"
                   "pub fn make() {\n    Builder::new(1);\n}\n" + self.gated("builder_observes_the_helper", "make"))
        self.write("crates/rsid/src/other_kind.rs", "pub struct Other;\n\nimpl Other {\n    pub fn new() -> Self {\n        Self\n    }\n}\n\n"
                   "pub fn make_other() {\n    Other::new();\n}\n" + self.gated("other_kind_is_unrelated", "make_other"))
        self.write("crates/rsid/src/scan.rs", 'const QUOTED: &str = "pub struct Builder<T> {";\n')
        self.write("crates/rsid/src/lib.rs", "pub mod builder;\npub mod core;\npub mod dialectic;\npub mod local;\npub mod openai;\n"
                   "pub mod other_kind;\npub mod scan;\npub mod unique;\n")
        self.commit("constructors")
        self.git("branch", "-f", "base")
        receipt = self.receipt()
        self.assertFalse(receipt["broad"], receipt["broad_filters"])
        observed = self.observers(receipt)
        self.assertIn("builder_observes_the_helper", observed)
        self.assertNotIn("other_kind_is_unrelated", observed)


class HubBoundedTraceTest(unittest.TestCase):
    """#2588: a high-fan-in hub bounds the observer trace without losing a direct observer.

    `Hub::step` is a common method name of a type that more than HUB_FUNCTIONS functions mention
    (the `SessionManager` of #1186). Following the type reaches the whole namespace, so the walk
    follows the changed method by its own name and stops at the hub: every test that can call it
    stays named, nearest first, and the broad module filter keeps the far observers."""

    CHANGED = "pub mod chain;\npub mod direct;\npub mod far;\npub mod hub;\npub mod other;\n\npub fn changed_helper() -> u32 {\n    1\n}\n"
    UNRELATED = "".join(GATE.format(shard="other-01", name=f"core_filler_{n}") for n in range(70))

    def setUp(self):
        self.temp = scratch_tempdir(prefix="hub-bounded-trace-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.git("init", "-q", "-b", "rolling")
        self.git("config", "user.name", "T")
        self.git("config", "user.email", "t@example.invalid")
        self.write("crates/rsid/src/lib.rs", "pub mod openai;\npub mod session;\n")
        self.write("crates/rsid/src/session/mod.rs", self.CHANGED + self.UNRELATED)
        self.write("crates/rsid/src/openai.rs", "fn execute_tool() {\n    unrelated();\n}\n")
        self.write("crates/rsid/src/session/hub.rs",
                   "pub struct Hub;\n\nimpl Hub {\n    pub fn step(&self) {\n        changed_helper();\n    }\n}\n")
        self.write("crates/rsid/src/session/other.rs", "pub fn step() {}\n")
        self.write("crates/rsid/src/session/far.rs",
                   "".join(f"pub fn far_user_{n:03}(_hub: &Hub) {{}}\n\n" for n in range(160))
                   + self.gated("far_observer_a", "far_user_000(&Hub)") + self.gated("far_observer_b", "far_user_001(&Hub)"))
        self.write("crates/rsid/src/session/direct.rs",
                   "pub fn wrapper() {\n    Hub.step();\n}\n\nfn cyc_a() {\n    Hub.step();\n    cyc_b();\n}\n\n"
                   "fn cyc_b() {\n    cyc_a();\n}\n"
                   + self.gated("calls_the_changed_function", "changed_helper()")
                   + self.gated("calls_the_hub_method", "Hub.step()")
                   + self.gated("calls_the_unrelated_step", "other::step()")
                   + self.gated("calls_the_wrapper", "wrapper()")
                   + self.gated("calls_the_cycle", "cyc_b()"))
        self.write("crates/rsid/src/session/chain.rs", "")
        self.git("add", ".")
        self.git("commit", "-q", "-m", "base")
        self.git("branch", "base")
        self.git("checkout", "-q", "-b", "feature")

    def gated(self, name, call):
        return GATE.format(shard="other-01", name=name).replace("{}", "{\n    " + call + ";\n}")

    def git(self, *args):
        scratch_git(self.root, *args)

    def write(self, rel, text):
        path = self.root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def commit_to_base(self):
        """The fixture edits so far are the base; `receipt()` then changes only `changed_helper`."""
        self.write("crates/rsid/src/session/mod.rs", self.CHANGED + self.UNRELATED)
        self.git("add", ".")
        self.git("commit", "-q", "-m", "fixture")
        self.git("branch", "-f", "base")

    def receipt(self):
        self.write("crates/rsid/src/session/mod.rs", (self.CHANGED + self.UNRELATED).replace("    1\n", "    2\n"))
        self.git("add", ".")
        self.git("commit", "-q", "-m", "change")
        result = subprocess.run([str(SCRIPT), "--filters-only", "--repo", str(self.root), "--base", "base"],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout.partition("\n")[2])

    def distances(self, receipt, names):
        """{test: nearest distance} of the named tests among the receipt's direct-observer filters."""
        found = {}
        for item in receipt["direct_observer_filters"]:
            for name in names:
                if re.search(rf"(?<![A-Za-z0-9_]){name}(?![A-Za-z0-9_])", item["filter"]):
                    found[name] = min(item["distance"], found.get(name, item["distance"]))
        return found

    NEAR = ("calls_the_changed_function", "calls_the_hub_method", "calls_the_unrelated_step",
            "calls_the_wrapper", "calls_the_cycle", "far_observer_a", "far_observer_b")

    def test_every_direct_observer_stays_named_nearest_first_and_the_far_ones_stay_in_the_broad_filter(self):
        receipt = self.receipt()
        self.assertTrue(receipt["broad"])
        broad = receipt["broad_filters"][0]
        self.assertIn("observer trace stops at high-fan-in Hub", broad["reason"])
        self.assertEqual([hub["token"] for hub in broad["hubs"]], ["Hub"])
        self.assertGreater(broad["hubs"][0]["functions"], 150)
        # The test that names the change, the tests that name the hub method (also the unrelated
        # same-name function, which only the name tells apart), a caller chain and a caller cycle.
        self.assertEqual(self.distances(receipt, self.NEAR), {
            "calls_the_changed_function": 1, "calls_the_hub_method": 2, "calls_the_unrelated_step": 2,
            "calls_the_wrapper": 3, "calls_the_cycle": 4})
        self.assertEqual(receipt["direct_observers"], {"tests": 5, "by_distance": {"1": 1, "2": 2, "3": 1, "4": 1}})
        self.assertEqual(broad["direct_tests"], 5)
        # A hub is a cap, not an ambiguity: scoped-test bounds the worker's selection (#2592) while the
        # gate's suggested filters keep the module filter that selects the far observers.
        self.assertEqual((broad["stop"], broad["worker_bounded"]), ("hub", True))
        # The far observers are not named, and nothing narrowed the module filter that selects them.
        self.assertIn(broad["filter"], receipt["suggested_filters"])
        self.assertEqual(broad["tests"], 70 + 2 + 5)

    def test_an_identity_the_scan_cannot_place_stays_broad_and_names_the_settled_observers(self):
        self.write("crates/rsid/src/session/tools.rs", "pub fn execute_tool() {\n    changed_helper();\n}\n")
        self.write("crates/rsid/src/session/plugins.rs", "pub fn load() {\n    crate::session::execute_tool();\n}\n")
        self.write("crates/rsid/src/session/hub.rs",
                   "pub use tools::execute_tool;\n\npub struct Hub;\n\nimpl Hub {\n    pub fn step(&self) {\n        changed_helper();\n    }\n}\n")
        self.CHANGED = self.CHANGED.replace("pub mod hub;", "pub mod hub;\npub mod plugins;\npub mod tools;")
        self.commit_to_base()
        receipt = self.receipt()
        broad = receipt["broad_filters"][0]
        self.assertIn("ambiguous exported helper", broad["reason"])
        self.assertEqual(broad["trace_stopped"], "unresolved helper identity")
        # An unplaced identity is an ambiguity: the worker keeps the broad fallback, not a bounded plan.
        self.assertEqual((broad["stop"], broad["worker_bounded"]), ("unresolved helper identity", False))
        self.assertEqual(self.distances(receipt, self.NEAR), {"calls_the_changed_function": 1})
        self.assertIn(broad["filter"], receipt["suggested_filters"])

    def test_a_trace_that_outgrows_the_function_bound_keeps_the_layers_it_settled(self):
        links = "pub fn link_000() {\n    changed_helper();\n}\n\n" + "".join(
            f"pub fn link_{n:03}() {{\n    link_{n - 1:03}();\n}}\n\n" for n in range(1, 160))
        self.write("crates/rsid/src/session/chain.rs", links + self.gated("calls_the_chain_start", "link_000()")
                   + self.gated("calls_the_chain_end", "link_159()"))
        self.write("crates/rsid/src/session/hub.rs", "")
        self.write("crates/rsid/src/session/far.rs", "")
        self.commit_to_base()
        receipt = self.receipt()
        broad = receipt["broad_filters"][0]
        self.assertIn("observer trace exceeds 150 functions", broad["reason"])
        self.assertEqual(broad["trace_stopped"], "function bound")
        self.assertEqual((broad["stop"], broad["worker_bounded"]), ("function bound", True))
        found = self.distances(receipt, ("calls_the_changed_function", "calls_the_chain_start", "calls_the_chain_end"))
        self.assertEqual(found, {"calls_the_changed_function": 1, "calls_the_chain_start": 2})
        self.assertIn(broad["filter"], receipt["suggested_filters"])


class PrivateFieldInitializerTest(unittest.TestCase):
    """#213: a new private field beside mechanical constructor initializers.

    `Store` is a hub type (hundreds of functions name it) and its constructors are called by every
    test. A field that the tree proves is one private field of one type is traced by its own name
    to the code that reads it; the rows that only store its starting value are not observers.
    Anything the tree cannot prove keeps the broad module filter."""

    CACHE = "pub struct IdleCache {\n    pub hits: u32,\n}\n"
    STORE = ("pub mod idle;\nmod open_tests;\nmod users;\n\npub struct Store {\n    conn: u32,%(field)s\n}\n\n"
             "impl Store {\n    pub fn open() -> Store {\n        Store {\n            conn: 1,%(init)s\n        }\n    }\n\n"
             "    pub fn open_for_test() -> Self {\n        Self {\n            conn: 2,%(init)s\n        }\n    }\n}\n")
    FIELD = "\n    cache: RefCell<idle::IdleCache>,"
    INIT = "\n            cache: RefCell::new(idle::IdleCache::default()),"

    def store_text(self, field=FIELD, init=INIT):
        return self.STORE % {"field": field, "init": init}

    gated_tests = CheckTouchedShardsTest.gated_tests

    def setUp(self):
        self.temp = scratch_tempdir(prefix="private-field-initializer-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.git("init", "-q", "-b", "rolling")
        self.git("config", "user.name", "T")
        self.git("config", "user.email", "t@example.invalid")
        self.write("crates/rsid-store/src/lib.rs", "pub mod store;\n")
        self.write("crates/rsid-store/src/store/mod.rs", self.store_text("", ""))
        self.write("crates/rsid-store/src/store/idle.rs", "#[derive(Default)]\n" + self.CACHE + self.reader()
                   + self.gated_tests("store-01", ["idle_peek"], {"idle_peek": "Store::open().peek"}))
        self.write("crates/rsid-store/src/store/users.rs", "".join(
            f"fn user_{n}(_store: &Store) {{}}\n" for n in range(160)))
        self.write("crates/rsid-store/src/store/open_tests.rs", self.gated_tests(
            "store-01", [f"opens_{n}" for n in range(70)], {f"opens_{n}": "Store::open" for n in range(70)}))
        self.commit("store fixture")
        self.git("branch", "base")
        self.git("checkout", "-q", "-b", "feature")

    def git(self, *args):
        scratch_git(self.root, *args)

    def write(self, rel, text):
        path = self.root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def commit(self, msg="change"):
        self.git("add", ".")
        self.git("commit", "-q", "-m", msg)

    def check(self, *extra):
        result = subprocess.run(
            [str(SCRIPT), *with_mode(["--repo", str(self.root), "--base", "base", *extra])],
            capture_output=True, text=True, env=dict(os.environ, CHECK_TOUCHED_SHARDS_CARGO="true"),
        )
        summary, _, rest = result.stdout.partition("\n")
        return result.returncode, summary, json.loads(rest)

    def reader(self, body="self.cache.borrow().hits"):
        return f"impl Store {{\n    pub fn peek(&self) -> u32 {{\n        {body}\n    }}\n}}\n"

    def candidate(self, text=None, **kwargs):
        self.write("crates/rsid-store/src/store/mod.rs", text or self.store_text(**kwargs))
        self.commit("candidate")
        code, summary, receipt = self.check("--filters-only")
        self.assertEqual(code, 0, summary)
        return receipt

    def test_unique_private_field_and_mechanical_constructors_name_only_the_readers(self):
        receipt = self.candidate()
        self.assertFalse(receipt["broad"], receipt["broad_filters"])
        self.assertEqual(receipt["broad_filters"], [])
        self.assertEqual(len(receipt["suggested_filters"]), 1, receipt["suggested_filters"])
        self.assertIn("idle_peek", receipt["suggested_filters"][0])
        self.assertNotIn("opens_", json.dumps(receipt["suggested_filters"]))

    def test_a_non_mechanical_initializer_keeps_the_broad_module_filter(self):
        init = "\n            cache: RefCell::new(warm_up(1)),"
        receipt = self.candidate(init=init)
        self.assertTrue(receipt["broad"])

    def test_a_constructor_that_also_reads_the_field_keeps_its_callers(self):
        text = self.store_text().replace("        Store {\n            conn: 1,",
                                         "        let warm = Self::peek;\n        Store {\n            conn: 1,", 1)
        text = text.replace("pub fn open() -> Store {", "pub fn open() -> Store {\n        let seen = Store::seen_cache();", 1)
        self.write("crates/rsid-store/src/store/seen.rs", "impl Store {\n    fn seen_cache() -> u32 {\n        0\n    }\n}\n")
        text = text.replace("let seen = Store::seen_cache();", "let seen = Self::cache_len(&Self::open_for_test());", 1)
        self.write("crates/rsid-store/src/store/seen.rs", "impl Store {\n    fn cache_len(&self) -> u32 {\n        self.cache.borrow().hits\n    }\n}\n")
        receipt = self.candidate(text=text)
        self.assertTrue(receipt["broad"])

    def test_the_same_field_name_on_another_type_is_not_traced_by_name(self):
        self.write("crates/rsid-store/src/store/other.rs", "pub struct Other {\n    cache: u8,\n}\n")
        self.commit("second owner")
        self.git("branch", "-f", "base")
        receipt = self.candidate()
        self.assertTrue(receipt["broad"])

    def test_a_pub_field_keeps_the_broad_module_filter(self):
        receipt = self.candidate(field=self.FIELD.replace("    cache", "    pub cache"))
        self.assertTrue(receipt["broad"])

    def test_a_macro_that_can_spell_the_field_keeps_the_broad_module_filter(self):
        self.write("crates/rsid-store/src/store/macros.rs", "macro_rules! field {\n    ($name:ident) => {\n        $name: 0,\n    };\n}\n")
        self.commit("macro")
        self.git("branch", "-f", "base")
        self.write("crates/rsid-store/src/store/macros.rs",
                   "macro_rules! field {\n    ($name:ident) => {\n        cache: 0,\n    };\n}\n")
        self.commit("macro names the field")
        self.git("branch", "-f", "base")
        receipt = self.candidate()
        self.assertTrue(receipt["broad"])

    def test_a_destructuring_pattern_row_is_a_read_not_an_initializer(self):
        self.write("crates/rsid-store/src/store/pattern.rs",
                   "fn take(store: Store) {\n    let Store {\n        cache: seen,\n        ..\n    } = store;\n}\n")
        self.commit("pattern")
        self.git("branch", "-f", "base")
        receipt = self.candidate()
        self.assertTrue(receipt["broad"])

    def test_the_tree_proof_accepts_one_private_declaration_only(self):
        proof = runpy.run_path(str(SCRIPT))["unique_private_field_owner"]
        one = {"a.rs": "pub struct Store {\n    cache: u8,\n}\n",
               "b.rs": "fn open() -> Store {\n    Store {\n        cache: Cell::new(0),\n    }\n}\n"}
        self.assertEqual(proof(one, "cache", {"Store"}), "Store")
        self.assertIsNone(proof(one, "cache", set()))
        self.assertIsNone(proof({**one, "c.rs": "pub struct Other {\n    cache: u8,\n}\n"}, "cache", {"Store", "Other"}))
        self.assertIsNone(proof({**one, "a.rs": "pub struct Store {\n    pub cache: u8,\n}\n"}, "cache", {"Store"}))
        self.assertIsNone(proof({**one, "c.rs": "fn f() {\n    g(cache: 1);\n}\n"}, "cache", {"Store"}))
        self.assertIsNone(proof({"a.rs": "fn f() {\n    Store {\n        cache: Cell::new(0),\n    }\n}\n"}, "cache", {"Store"}))


class ChangedModulesReceiptTest(unittest.TestCase):
    """#2182: the receipt names the modules whose own source changed, so scoped-test can run their tests first."""

    def setUp(self):
        self.temp = scratch_tempdir(prefix="changed-modules-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.git("init", "-q", "-b", "rolling")
        self.git("config", "user.name", "T")
        self.git("config", "user.email", "t@example.invalid")
        gated = lambda shard, name: GATE.format(shard=shard, name=name)
        self.write("crates/rsid/src/lib.rs", "pub mod session;\n")
        self.write("crates/rsid/src/session/mod.rs", "mod rotation;\nmod tools;\npub fn central() {}\n")
        self.write("crates/rsid/src/session/rotation.rs", "mod recovery;\nmod fence_tests;\nmod abandon;\npub fn rot() {}\n")
        self.write("crates/rsid/src/session/rotation/recovery.rs", "pub fn recover() {}\n" + gated("session-03", "recovery_keeps_the_controller"))
        self.write("crates/rsid/src/session/rotation/fence_tests.rs", gated("session-03", "fence_holds"))
        self.write("crates/rsid/src/session/rotation/abandon.rs", gated("session-03", "abandon_is_a_separate_source"))
        self.write("crates/rsid/src/session/tools.rs", gated("session-01", "tools_are_unrelated"))
        self.write("crates/rsid/tests/it.rs", "#[test]\nfn t() {}\n")
        self.git("add", ".")
        self.git("commit", "-q", "-m", "base")
        self.git("branch", "base")
        self.git("checkout", "-q", "-b", "feature")

    def git(self, *args):
        scratch_git(self.root, *args)

    def write(self, rel, text):
        path = self.root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def receipt(self):
        self.git("add", ".")
        self.git("commit", "-q", "-m", "change")
        result = subprocess.run([str(SCRIPT), "--filters-only", "--repo", str(self.root), "--base", "base"],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout.partition("\n")[2])

    def test_changed_modules_name_their_own_tests_and_exclude_unchanged_sources(self):
        self.write("crates/rsid/src/session/rotation.rs", "mod recovery;\nmod fence_tests;\nmod abandon;\npub fn rot() -> u32 {\n    2\n}\n")
        self.write("crates/rsid/src/session/rotation/recovery.rs",
                   "pub fn recover() -> u32 {\n    3\n}\n" + GATE.format(shard="session-03", name="recovery_keeps_the_controller"))
        self.write("crates/rsid/src/session/mod.rs", "mod rotation;\nmod tools;\npub fn central() -> u32 {\n    1\n}\n")
        self.write("crates/rsid/tests/it.rs", "#[test]\nfn t() {\n    assert!(true);\n}\n")
        records = {(r["package"], r.get("module") or f'{r["kind"]}:{r["target"]}'): r
                   for r in self.receipt()["changed_modules"]}
        self.assertEqual(set(records), {("rsid", "session"), ("rsid", "session::rotation"),
                                        ("rsid", "session::rotation::recovery"), ("rsid", "test:it")})
        # `rotation` holds its own and its test modules' tests, never the unchanged `abandon` source's.
        self.assertEqual(records[("rsid", "session::rotation")]["excluded"], ["session::rotation::abandon"])
        self.assertEqual(records[("rsid", "session::rotation::recovery")]["excluded"], [])
        self.assertEqual(records[("rsid", "session::rotation::recovery")]["tests"], 1)
        self.assertEqual(records[("rsid", "session")]["excluded"], ["session::rotation::abandon", "session::tools"])

    def test_a_comment_only_edit_changes_no_module(self):
        self.write("crates/rsid/src/session/rotation.rs", "mod recovery;\nmod fence_tests;\nmod abandon;\n// a note\npub fn rot() {}\n")
        self.assertEqual(self.receipt()["changed_modules"], [])


class SplitStoreReceiptTest(unittest.TestCase):
    """#1021 S4: rsid-store changes are mapped, compiled and reported like rsid's."""

    def setUp(self):
        self.temp = scratch_tempdir(prefix="touched-shards-split-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / "repo"
        self.root.mkdir()
        self.log = Path(self.temp.name) / "cargo.log"
        self.cargo = Path(self.temp.name) / "cargo.sh"
        self.cargo.write_text('#!/bin/sh\necho "$@" >> "$CARGO_LOG"\n')
        self.cargo.chmod(0o755)
        self.git("init", "-q", "-b", "rolling")
        self.git("config", "user.name", "T")
        self.git("config", "user.email", "t@example.invalid")
        (self.root / "scripts").mkdir()
        shutil.copy(CHECKER, self.root / "scripts/check-rsid-test-shards.py")
        self.write("crates/rsid/Cargo.toml", RSID_MANIFEST)
        self.write("crates/rsid-store/Cargo.toml", STORE_MANIFEST)
        self.write("crates/rsid/src/lib.rs", "mod foo;\nmod bar;\n")
        self.write("crates/rsid-store/src/lib.rs", "mod store;\n")
        self.write("crates/rsid-store/src/store/mod.rs", "mod q;\nmod r;\n")
        self.write("crates/rsid/src/foo.rs", GATE.format(shard="store-02", name="a"))
        self.write("crates/rsid/src/bar.rs", "pub fn bar() {}\nmod tests;\n")
        self.write("crates/rsid/src/bar/tests.rs", GATE.format(shard="other-03", name="b"))
        self.write("crates/rsid-store/src/store/q.rs", GATE.format(shard="store-01", name="c"))
        self.write("crates/rsid-store/src/store/r.rs", GATE.format(shard="store-02", name="d"))
        self.write("crates/rsid-store/src/store/migrations/v150.rs", "// v150\n")
        self.git("add", ".")
        self.git("commit", "-q", "-m", "base")
        self.git("branch", "base")
        self.git("checkout", "-q", "-b", "feature")

    def git(self, *args):
        scratch_git(self.root, *args)

    def write(self, rel, text):
        path = self.root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def commit(self):
        self.git("add", ".")
        self.git("commit", "-q", "-m", "change")

    def check(self):
        env = dict(os.environ, CHECK_TOUCHED_SHARDS_CARGO=str(self.cargo), CARGO_LOG=str(self.log))
        result = subprocess.run(
            [str(SCRIPT), *with_mode(["--repo", str(self.root), "--base", "base"])],
            capture_output=True, text=True, env=env,
        )
        summary, _, rest = result.stdout.partition("\n")
        commands = self.log.read_text().splitlines() if self.log.exists() else []
        return result.returncode, summary, json.loads(rest), commands

    def test_an_rsid_store_source_change_compiles_its_shard_and_one_dependent_shard(self):
        self.write("crates/rsid-store/src/store/q.rs", GATE.format(shard="store-01", name="c") + "const EDIT: u8 = 1;\n")
        self.commit()
        code, summary, receipt, commands = self.check()
        self.assertEqual(code, 0, summary)
        # store-01 holds only rsid-store tests; other-03 is the rsid-only shard
        # that proves the daemon still compiles against the changed store.
        self.assertEqual(receipt["shards"]["compiled_ok"], ["other-03", "store-01"])
        self.assertIn(
            "check -p rsid-store --lib --tests --no-default-features --features test-shard-store-01",
            commands,
        )
        self.assertIn(
            "check -p rsid --lib --tests --no-default-features --features test-shard-other-03",
            commands,
        )
        self.assertEqual(receipt["suggested_filters"], ["rsid=shard:store-01:test(store::q)"])

    def test_a_shard_shared_by_both_packages_compiles_both(self):
        self.write("crates/rsid-store/src/store/r.rs", GATE.format(shard="store-02", name="d") + "const EDIT: u8 = 1;\n")
        self.commit()
        code, summary, receipt, commands = self.check()
        self.assertEqual(code, 0, summary)
        self.assertIn(
            "check -p rsid -p rsid-store --lib --tests --no-default-features --features test-shard-store-02",
            commands,
        )
        self.assertEqual(receipt["suggested_filters"], ["rsid=shard:store-02:test(store::r)"])

    def test_a_migration_only_store_change_compiles_a_fallback_shard_and_reports_the_number(self):
        self.write("crates/rsid-store/src/store/migrations/v151.rs", "// v151\n")
        self.commit()
        code, summary, receipt, commands = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["migrations"]["new"], [151])
        self.assertEqual(receipt["migrations"]["base_head"], 150)
        self.assertFalse(receipt["migrations"]["numbering_conflict"])
        self.assertEqual(receipt["shards"]["fallback"], "store-01")
        self.assertIn("check -p rsid-store --lib --tests --no-default-features --features test-shard-store-01", commands)
        self.assertIn("migrations=V151", summary)

    def test_a_migration_unit_added_to_the_pre_split_directory_fails_the_receipt(self):
        self.write("crates/rsid/src/store/migrations/v151.rs", "// v151 in the dead directory\n")
        self.commit()
        code, summary, receipt, _ = self.check()
        self.assertEqual(code, 1)
        self.assertEqual(receipt["migrations"]["stray_pre_split"], ["crates/rsid/src/store/migrations/v151.rs"])
        self.assertFalse(receipt["ok"])
        self.assertIn("STRAY_PRE_SPLIT_MIGRATION", summary)

    def test_a_pre_split_base_still_reports_its_head_from_the_old_directory(self):
        self.git("checkout", "-q", "base")
        self.git("rm", "-q", "-r", "crates/rsid-store/src/store/migrations")
        self.write("crates/rsid/src/store/migrations/v150.rs", "// v150\n")
        self.git("add", ".")
        self.git("commit", "-q", "-m", "pre-split base")
        self.git("checkout", "-q", "-b", "after-split")
        self.git("rm", "-q", "-r", "crates/rsid/src/store/migrations")
        self.write("crates/rsid-store/src/store/migrations/v150.rs", "// v150\n")
        self.write("crates/rsid-store/src/store/migrations/v151.rs", "// v151\n")
        self.commit()
        code, summary, receipt, _ = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["migrations"]["base_head"], 150)
        # The moved V150 unit is a rename, not a new migration.
        self.assertEqual(receipt["migrations"]["new"], [151])
        self.assertFalse(receipt["migrations"]["numbering_conflict"])
        self.assertEqual(receipt["migrations"]["stray_pre_split"], [])

    def check_with(self, **env):
        env = dict(os.environ, CHECK_TOUCHED_SHARDS_CARGO=str(self.cargo), CARGO_LOG=str(self.log), **env)
        result = subprocess.run([str(SCRIPT), *with_mode(["--repo", str(self.root), "--base", "base"])], capture_output=True, text=True, env=env)
        summary, _, rest = result.stdout.partition("\n")
        commands = self.log.read_text().splitlines() if self.log.exists() else []
        return result.returncode, summary, json.loads(rest), commands, result.stderr

    def edit_store_with_manifests(self):
        self.write("crates/rsid/Cargo.toml", '[package]\nname = "rsid"\n' + RSID_MANIFEST)
        self.write("crates/rsid-store/Cargo.toml", '[package]\nname = "rsid-store"\n' + STORE_MANIFEST)
        self.write("crates/rsid-store/src/store/q.rs", GATE.format(shard="store-01", name="c") + "const EDIT: u8 = 1;\n")
        self.commit()

    def test_workspace_packages_are_not_cleaned_by_default(self):
        # A shared target holds other agents' artifacts; the clean is opt-in.
        self.edit_store_with_manifests()
        code, summary, _, commands, _ = self.check_with(CARGO_TARGET_DIR=str(self.root / "target"))
        self.assertEqual(code, 0, summary)
        self.assertTrue(commands and all(c.startswith("check ") for c in commands), commands)

    def test_an_opted_in_clean_runs_first_for_a_target_under_the_repo(self):
        self.edit_store_with_manifests()
        code, summary, _, commands, _ = self.check_with(CHECK_TOUCHED_SHARDS_CLEAN="1", CARGO_TARGET_DIR=str(self.root / "target"))
        self.assertEqual(code, 0, summary)
        self.assertEqual(commands[0], "clean -p rsid -p rsid-store")
        self.assertTrue(all(c.startswith("check ") for c in commands[1:]), commands)

    def test_an_opted_in_clean_never_touches_a_target_outside_the_repo(self):
        self.edit_store_with_manifests()
        outside = Path(self.temp.name) / "shared-target"
        code, summary, _, commands, stderr = self.check_with(CHECK_TOUCHED_SHARDS_CLEAN="1", CARGO_TARGET_DIR=str(outside))
        self.assertEqual(code, 0, summary)
        self.assertTrue(commands and all(c.startswith("check ") for c in commands), commands)
        self.assertIn("not cleaning", stderr)

    def test_every_shard_failing_the_same_way_is_reported_as_a_suspected_stale_artifact(self):
        self.write("crates/rsid-store/src/store/q.rs", GATE.format(shard="store-01", name="c") + "const EDIT: u8 = 1;\n")
        self.write("crates/rsid-store/src/store/r.rs", GATE.format(shard="store-02", name="d") + "const EDIT: u8 = 1;\n")
        self.commit()
        self.cargo.write_text("#!/bin/sh\necho 'error[E0609]: no field continue_from on type X' >&2\nexit 101\n")
        code, _, receipt, _, stderr = self.check_with()
        self.assertEqual(code, 1)
        stale = receipt["shards"]["shard_compile_suspect_stale"]
        self.assertIn("CHECK_TOUCHED_SHARDS_CLEAN=1", stale["hint"])
        self.assertIn("continue_from", stale["error"])
        self.assertIn("shard_compile_suspect_stale", stderr)

    def test_failed_compile_reports_first_error_and_span_before_warning_tail(self):
        self.write("crates/rsid/src/foo.rs", GATE.format(shard="store-02", name="a") + "const EDIT: u8 = 1;\n")
        self.commit()
        for error in ("error[E0609]: no field missing on type X", "error: fixture compile failed"):
            with self.subTest(error=error):
                diagnostic = error + "\n  --> crates/rsid/src/foo.rs:12:7\n   |\n12 | value.missing\n   |       ^^^^^^^ unknown field\n\n"
                output = "warning: preceding noise\n" * 3000 + diagnostic + "warning: trailing noise\n" * 3000
                self.cargo.write_text("#!/bin/sh\ncat >&2 <<'DIAGNOSTICS'\n" + output + "DIAGNOSTICS\nexit 101\n")
                code, summary, receipt, _, stderr = self.check_with()
                self.assertEqual(code, 1)
                self.assertIn("FAIL", summary)
                self.assertEqual(receipt["shards"]["compiled_failed"], ["store-02"])
                self.assertIn(diagnostic.rstrip(), stderr)
                self.assertLess(stderr.index(error), stderr.index("warning: trailing noise"))
                self.assertIn("shard store-02 compile failed:", stderr)
                self.assertIn("command: " + str(self.cargo) + " check -p rsid -p rsid-store", stderr)
                self.assertIn("exit code: 101", stderr)
                self.assertLess(len(without_progress(stderr)), 6500)

    def test_failed_compile_bounds_error_blocks_by_lines_and_characters(self):
        self.write("crates/rsid/src/foo.rs", GATE.format(shard="store-02", name="a") + "const EDIT: u8 = 1;\n")
        self.commit()
        for width in (80, 500):
            with self.subTest(width=width):
                first = "error[E0609]: first error\n  --> foo.rs:12:7\n"
                second = "error: second error\n  --> bar.rs:20:3\n"
                output = first + "warning: between errors\n" + second + ("   | " + "x" * width + "\n") * 100
                output += "warning: final compiler context\n" * 100
                self.cargo.write_text("#!/bin/sh\ncat >&2 <<'DIAGNOSTICS'\n" + output + "DIAGNOSTICS\nexit 101\n")
                code, _, _, _, stderr = self.check_with()
                self.assertEqual(code, 1)
                excerpt = stderr[stderr.index(first):].partition("\n\nstderr tail:\n")[0]
                self.assertIn(first, excerpt)
                self.assertIn(second, excerpt)
                self.assertLessEqual(len(excerpt.splitlines()), 40)
                self.assertLessEqual(len(excerpt), 4000)
                self.assertIn("warning: final compiler context", stderr)
                self.assertLess(len(without_progress(stderr)), 6500)

    def test_failed_compile_without_error_diagnostics_keeps_stderr_tail(self):
        self.write("crates/rsid/src/foo.rs", GATE.format(shard="store-02", name="a") + "const EDIT: u8 = 1;\n")
        self.commit()
        self.cargo.write_text("#!/bin/sh\necho 'compiler wrapper unavailable' >&2\nexit 7\n")
        code, _, receipt, _, stderr = self.check_with()
        self.assertEqual(code, 1)
        self.assertEqual(receipt["shards"]["compiled_failed"], ["store-02"])
        self.assertIn("exit code: 7\ncompiler wrapper unavailable\n", stderr)

    def test_different_shard_failures_are_not_suspected_stale(self):
        self.write("crates/rsid-store/src/store/q.rs", GATE.format(shard="store-01", name="c") + "const EDIT: u8 = 1;\n")
        self.write("crates/rsid-store/src/store/r.rs", GATE.format(shard="store-02", name="d") + "const EDIT: u8 = 1;\n")
        self.commit()
        self.cargo.write_text('#!/bin/sh\necho "error: broke $*" >&2\nexit 101\n')
        code, _, receipt, _, _ = self.check_with()
        self.assertEqual(code, 1)
        self.assertIsNone(receipt["shards"]["shard_compile_suspect_stale"])

    def run_check(self, *args, **env):
        env = dict(os.environ, CHECK_TOUCHED_SHARDS_CARGO=str(self.cargo), CARGO_LOG=str(self.log), **env)
        return subprocess.run([str(SCRIPT), *with_mode(["--repo", str(self.root), "--base", "base", *args])],
                              capture_output=True, text=True, env=env)

    def test_a_full_check_logs_each_phase_and_shard_with_its_build_slot_wait(self):
        # #1657/#1718: a check that takes minutes must say where it is and how long
        # each shard waited for a build slot, apart from how long it built.
        self.write("crates/rsid-store/src/store/q.rs", GATE.format(shard="store-01", name="c") + "const EDIT: u8 = 1;\n")
        self.commit()
        self.cargo.write_text('#!/bin/sh\necho "$@" >> "$CARGO_LOG"\n'
                              'echo "rsi-phase 1700000000 slot-granted: waited 7s (build); running" >&2\n')
        result = self.run_check()
        self.assertEqual(result.returncode, 0, result.stderr)
        receipt = json.loads(result.stdout.partition("\n")[2])
        self.assertIn("compiling shard store-01 (2/2)", result.stderr)
        self.assertIn("shard store-01 ok in", result.stderr)
        self.assertIn("(build slot wait 7s)", result.stderr)
        self.assertIn("diff_and_sources done in", result.stderr)
        runs = receipt["shards"]["runs"]
        self.assertEqual(sorted(runs), ["other-03", "store-01"])
        self.assertEqual(runs["store-01"]["verdict"], "ok")
        self.assertEqual(runs["store-01"]["slot_wait_sec"], 7)
        self.assertLessEqual(runs["store-01"]["build_sec"], runs["store-01"]["seconds"])
        self.assertFalse(receipt["partial"])

    def test_filters_only_stays_quiet_on_stderr(self):
        self.write("crates/rsid-store/src/store/q.rs", GATE.format(shard="store-01", name="c") + "const EDIT: u8 = 1;\n")
        self.commit()
        result = self.run_check("--filters-only", "--json")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(without_progress(result.stderr), result.stderr)
        self.assertEqual(result.stderr, "")

    def test_a_killed_run_leaves_a_partial_receipt_of_the_shards_it_finished(self):
        # #1718: a unit budget that SIGTERMs the check mid-shard must not discard the
        # shards that already compiled; a successor resumes only the pending ones.
        self.write("crates/rsid-store/src/store/q.rs", GATE.format(shard="store-01", name="c") + "const EDIT: u8 = 1;\n")
        self.commit()
        self.cargo.write_text('#!/bin/sh\necho "$@" >> "$CARGO_LOG"\n'
                              'case "$*" in *test-shard-store-01*) exec sleep 60;; esac\n')
        receipt_path = Path(self.temp.name) / "partial.json"
        env = dict(os.environ, CHECK_TOUCHED_SHARDS_CARGO=str(self.cargo), CARGO_LOG=str(self.log))
        process = subprocess.Popen(
            [str(SCRIPT), *with_mode(["--repo", str(self.root), "--base", "base", "--receipt", str(receipt_path), "--receipt-line"])],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env)
        try:
            deadline = time.monotonic() + 60
            while time.monotonic() < deadline:
                commands = self.log.read_text().splitlines() if self.log.exists() else []
                if any("test-shard-store-01" in command for command in commands):
                    break
                time.sleep(0.05)
            else:
                self.fail("the second shard never started")
            process.send_signal(signal.SIGTERM)
            stdout, stderr = process.communicate(timeout=30)
        finally:
            if process.poll() is None:
                process.kill()
        self.assertEqual(process.returncode, 128 + signal.SIGTERM)
        partial = json.loads(receipt_path.read_text())
        self.assertTrue(partial["partial"])
        self.assertEqual(partial["phase"], "interrupted")
        self.assertEqual(partial["shards"]["compiled_ok"], ["other-03"])
        self.assertEqual(partial["shards"]["compile_pending"], ["store-01"])
        self.assertEqual(sorted(partial["shards"]["runs"]), ["other-03"])
        line = next(line for line in stdout.splitlines() if line.startswith("RECEIPT_JSON "))
        self.assertEqual(json.loads(line[len("RECEIPT_JSON "):]), partial)
        self.assertIn("interrupted by signal", stderr)


class CodeShardsTest(unittest.TestCase):
    """#1818: a shard named only in a fixture string or comment is not a gate."""

    def code_shards(self, text):
        return runpy.run_path(str(SCRIPT), run_name="check_touched_shards")["code_shards"](text)

    def test_only_cfg_gates_in_code_count(self):
        fixture = (
            '#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]\n'
            "#[test]\nfn t() {\n"
            '    let script = "#[cfg(any(feature = \\"test-shard-store-04\\"))]";\n'
            '    let raw = r#"feature = "test-shard-session-05""#;\n'
            '    let ch = \'"\'; let lt: &\'a str = "x";\n'
            "}\n"
            '// feature = "test-shard-store-01"\n'
            '/* /* nested */ feature = "test-shard-other-05" */\n'
        )
        self.assertEqual(self.code_shards(fixture), {"other-01"})

    def test_helper_gate_listing_several_shards_all_count(self):
        text = '#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01", feature = "test-shard-store-02"))]\nfn h() {}\n'
        self.assertEqual(self.code_shards(text), {"store-01", "store-02"})


class LexemeTest(unittest.TestCase):
    """#2434: a line comment is closed by its newline, whatever character ends it."""

    def lexemes(self, text):
        lex = runpy.run_path(str(SCRIPT), run_name="check_touched_shards")["rust_lexemes"]
        return [(kind, text[start:end]) for kind, start, end in lex(text)]

    def test_a_comment_ending_in_a_quote_does_not_open_a_string(self):
        text = '// echoes it as a "user"\nlet a = "user".to_string();\nlet b = "x";\n'
        self.assertEqual(self.lexemes(text), [
            ("comment", '// echoes it as a "user"'), ("literal", '"user"'), ("literal", '"x"')])


class HoldsRunnableTestFailsSafeTest(unittest.TestCase):
    """#2361: a filter is dropped only when the scan proves the source holds no runnable test."""

    def holds(self, text):
        return runpy.run_path(str(SCRIPT), run_name="check_touched_shards")["holds_runnable_test"](text)

    def test_unrecognized_test_attribute_beside_an_ignored_test_keeps_the_filter(self):
        ignored = "#[test]\n#[ignore = \"slow\"]\nfn slow() {}\n"
        for other in ("#[cfg_attr(test, test)]\nfn live() {}\n", "#[rstest]\nfn live() {}\n",
                      "#[test_case(1)]\nfn live(n: u8) {}\n", "#[proptest]\nfn live() {}\n",
                      "#[some::path::test]\nfn live() {}\n", "proptest! { fn live() {} }\n"):
            with self.subTest(other=other):
                self.assertTrue(self.holds(ignored + other))
                self.assertTrue(self.holds(other))

    def test_only_proven_empty_sources_drop_the_filter(self):
        for text in ("#[test]\n#[ignore]\nfn a() {}\n",
                     "#[tokio::test]\n#[ignore = \"x\"]\nasync fn a() {}\n",
                     "/// a `#[rstest]` and `#[cfg_attr(test, test)]` in prose\nfn a() {}\n",
                     'const S: &str = "#[test] #[rstest]";\n',
                     "#[cfg_attr(test, derive(Debug))]\nstruct S;\n#[cfg(test)]\nmod t {}\n"):
            with self.subTest(text=text):
                self.assertFalse(self.holds(text))
        self.assertTrue(self.holds("#[test]\n#[ignore]\nfn a() {}\n#[test]\nfn b() {}\n"))


class OtherOsTestsAreNotSelectedTest(unittest.TestCase):
    """#2291: a test gated to another OS is not compiled here, so no filter may name it."""

    SOURCE = (
        '#[cfg(target_os = "macos")]\n#[test]\nfn mac_only() {}\n'
        '#[cfg(not(target_os = "linux"))]\n#[test]\nfn not_linux() {}\n'
        '#[cfg(target_os = "linux")]\n#[test]\nfn linux_only() {}\n'
        '#[cfg(all(target_os = "macos", unix))]\n#[test]\nfn compound() {}\n'
        '#[test]\nfn everywhere() {}\n'
    )

    def names(self, host):
        mod = runpy.run_path(str(SCRIPT), run_name="check_touched_shards")
        # run_path hands back a copy of the globals the functions read.
        mod["rust_functions"].__globals__["HOST_OS"] = host
        return [name for name, *_ in mod["rust_functions"](self.SOURCE)]

    def test_a_linux_host_keeps_only_the_tests_it_compiles(self):
        # `compound` stays: only a plain target_os gate is read.
        self.assertEqual(self.names("linux"), ["linux_only", "compound", "everywhere"])

    def test_a_macos_host_keeps_the_macos_tests(self):
        self.assertEqual(self.names("macos"), ["mac_only", "not_linux", "compound", "everywhere"])


class OtherOsGatesReachEveryOwnerTest(unittest.TestCase):
    """#2291: a gate on an enclosing inline `mod`, on an external `mod x;` or `#![cfg]` in the file
    itself leaves a test out of this host exactly as a gate on the test does. A condition the
    scan does not read (`any`, a feature, an `all` with another predicate) keeps the test."""

    INLINE = (
        '#[test]\nfn everywhere() {}\n\n'
        '#[cfg(target_os = "macos")]\nmod mac_only {\n    use super::*;\n\n'
        '    #[test]\n    fn mac_nested() {}\n\n'
        '    mod deeper {\n        #[test]\n        fn mac_deeper() {}\n    }\n}\n\n'
        '#[cfg(all(test, target_os = "windows"))]\nmod win_only {\n    #[test]\n    fn win_nested() {}\n}\n\n'
        '#[cfg(not(target_os = "windows"))]\n#[allow(dead_code)]\nmod not_windows {\n'
        '    #[test]\n    fn not_win_nested() {}\n}\n\n'
        '#[cfg(feature = "x")]\nmod unknown_feature {\n    #[test]\n    fn feature_gated() {}\n}\n\n'
        '#[cfg(any(target_os = "macos", target_os = "windows"))]\nmod any_os {\n'
        '    #[test]\n    fn any_nested() {}\n}\n\n'
        '#[cfg(target_os = "macos")]\nmod never_closed {\n    #[test]\n    fn unclosed() {}\n'
        '#[test]\nfn after_modules() {}\n'
    )
    EXTERNAL_ROOT = (
        "mod live;\n"
        '#[cfg(target_os = "macos")]\nmod mac_dir;\n'
        '#[cfg(target_os = "windows")]\n#[path = "win_impl.rs"]\nmod win;\n'
        '#[cfg(target_os = "macos")] mod same_line;\n'
        '#[cfg(target_os = "macos")]\n#[path = "shared.rs"]\nmod mac_view;\n'
        "mod shared;\n"
        '#[cfg(target_os = "windows")]\nmod win_wrap {\n    mod inner;\n}\n'
        '#[cfg(feature = "x")]\nmod unknown;\n'
    )

    def module(self, host):
        mod = runpy.run_path(str(SCRIPT), run_name="check_touched_shards")
        mod["rust_functions"].__globals__["HOST_OS"] = host
        return mod

    def names(self, host, text):
        return [name for name, *_ in self.module(host)["rust_functions"](text)]

    def test_an_inline_module_gate_leaves_out_its_tests_on_each_host(self):
        # `never_closed` has no closing brace at its indentation: nothing is proven, so its test stays.
        common = ["everywhere", "feature_gated", "any_nested", "unclosed", "after_modules"]
        expected = {
            "linux": ["everywhere", "not_win_nested", "feature_gated", "any_nested", "unclosed", "after_modules"],
            "macos": ["everywhere", "mac_nested", "mac_deeper", "not_win_nested", *common[1:]],
            "windows": ["everywhere", "win_nested", *common[1:]],
        }
        for host, names in expected.items():
            with self.subTest(host=host):
                self.assertEqual(self.names(host, self.INLINE), names)

    def test_a_files_own_inner_gate_leaves_out_all_its_tests(self):
        text = "// header\n\n#![allow(dead_code)]\n#![cfg(target_os = \"macos\")]\n#[test]\nfn mac_file() {}\n"
        self.assertEqual(self.names("linux", text), [])
        self.assertEqual(self.names("macos", text), ["mac_file"])
        # An inner attribute below the first item is not a file gate.
        late = '#[test]\nfn first() {}\n#![cfg(target_os = "macos")]\n'
        self.assertEqual(self.names("linux", late), ["first"])

    def gated_off(self, host):
        mod = self.module(host)
        files = {"crates/x/src/lib.rs": self.EXTERNAL_ROOT}
        for rel in ("live", "mac_dir", "mac_dir/child", "win_impl", "same_line", "shared", "win_wrap/inner", "unknown"):
            files[f"crates/x/src/{rel}.rs"] = '#[test]\nfn t() {}\n'
        files["crates/x/src/mac_dir.rs"] = "mod child;\n#[test]\nfn t() {}\n"
        gated = set()
        mod["module_map"](files, gated)
        return sorted(path.removeprefix("crates/x/src/") for path in gated)

    def test_an_external_module_declaration_gate_leaves_out_the_file_and_its_children(self):
        self.assertEqual(self.gated_off("linux"),
                         ["mac_dir.rs", "mac_dir/child.rs", "same_line.rs", "win_impl.rs", "win_wrap/inner.rs"])
        self.assertEqual(self.gated_off("macos"), ["win_impl.rs", "win_wrap/inner.rs"])
        self.assertEqual(self.gated_off("windows"), ["mac_dir.rs", "mac_dir/child.rs", "same_line.rs"])

    def test_a_file_also_mounted_without_a_gate_stays_in(self):
        # shared.rs is mounted by the macOS-only `mac_view` and by the ungated `shared`.
        for host in ("linux", "macos", "windows"):
            with self.subTest(host=host):
                self.assertNotIn("shared.rs", self.gated_off(host))

    def test_a_marked_source_yields_no_functions_and_no_runnable_test(self):
        mod = self.module("linux")
        text = "#[test]\nfn t() {}\n"
        marked = text + mod["HOST_GATED_OFF"]
        self.assertEqual([n for n, *_ in mod["rust_functions"](marked)], [])
        self.assertTrue(mod["holds_runnable_test"](text))
        self.assertFalse(mod["holds_runnable_test"](marked))
        # The marker is appended, so a changed line keeps its number.
        self.assertTrue(marked.startswith(text))

    def test_a_source_whose_only_tests_are_gated_off_holds_no_runnable_test(self):
        mac_test = '#[cfg(target_os = "macos")]\n#[test]\nfn a() {}\n'
        mac_mod = '#[cfg(all(test, target_os = "macos"))]\nmod t {\n    #[test]\n    fn a() {}\n}\n'
        live = "#[test]\nfn b() {}\n"
        unknown = '#[cfg(feature = "x")]\n#[test]\nfn c() {}\n'
        ignored = "#[test]\n#[ignore]\nfn d() {}\n"
        cases = {
            "linux": [(mac_test, False), (mac_mod, False), (mac_test + live, True), (mac_mod + live, True),
                      (mac_test + unknown, True), (mac_test + ignored, False)],
            "macos": [(mac_test, True), (mac_mod, True), (mac_test + ignored, True)],
        }
        for host, rows in cases.items():
            holds = self.module(host)["holds_runnable_test"]
            for text, expected in rows:
                with self.subTest(host=host, text=text):
                    self.assertEqual(holds(text), expected)

    def test_the_gate_grammar_answers_as_check_rsid_test_shards_does(self):
        spec = importlib.util.spec_from_file_location("check_rsid_test_shards", SCRIPT.parent / "check-rsid-test-shards.py")
        shards = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(shards)
        rows = [
            '#[cfg(target_os = "macos")]', '#[cfg(target_os = "linux")]', '#[cfg(target_os = "windows")]',
            '#[cfg(not(target_os = "macos"))]', '#[cfg(not(target_os = "linux"))]',
            '#[cfg(all(test, target_os = "macos"))]', '#[cfg(all(test, not(target_os = "linux")))]',
            '#[cfg(all(target_os = "macos", unix))]', '#[cfg(any(target_os = "macos", unix))]',
            '#[cfg(feature = "x")]', '#[cfg(target_os = "macos"]', '#[cfg(test)]', '#[test]',
        ]
        for host in ("linux", "macos", "windows"):
            mod = self.module(host)
            with mock.patch.object(shards, "HOST_TARGET_OS", host):
                for row in rows:
                    with self.subTest(host=host, row=row):
                        self.assertEqual(mod["other_os_gate"](row), shards.host_excluded(row))


class OtherOsFilterDerivationTest(unittest.TestCase):
    """#2291: the derived filters of a production edit name only the tests this host compiles.

    Reproduces SystemdJobRuntime::ensure_build_slice (agent_jobs.rs): its observers include tests
    behind a macOS gate on the test, on an enclosing inline `mod` and on an external `mod x;`. A
    Linux host names the Linux observers, whose same-module neighbours and external file stay."""

    GATE = '#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]'

    def setUp(self):
        self.temp = scratch_tempdir(prefix="other-os-filters-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.git("init", "-q", "-b", "rolling")
        self.git("config", "user.name", "T")
        self.git("config", "user.email", "t@example.invalid")
        gate = self.GATE
        filler = "".join(f"    {gate}\n    #[test]\n    fn filler_{i}() {{}}\n" for i in range(70))
        files = {
            "crates/rsid/src/store/migrations/v150.rs": "// v150\n",
            "crates/rsid/src/lib.rs": (
                "mod agent_jobs;\n"
                '#[cfg(target_os = "macos")]\nmod mac_jobs;\n'
                '#[cfg(target_os = "linux")]\nmod linux_jobs;\n'),
            "crates/rsid/src/mac_jobs.rs": f"{gate}\n#[test]\nfn mac_file_test() {{ super::agent_jobs::ensure_build_slice(); }}\n",
            "crates/rsid/src/linux_jobs.rs": f"{gate}\n#[test]\nfn linux_file_test() {{ super::agent_jobs::ensure_build_slice(); }}\n",
            "crates/rsid/src/agent_jobs.rs": (
                "pub fn ensure_build_slice() {\n    1;\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n"
                f"{filler}\n"
                f"    {gate}\n    #[test]\n    fn linux_probe() {{ ensure_build_slice(); }}\n\n"
                f'    {gate}\n    #[cfg(target_os = "macos")]\n    #[test]\n    fn macos_direct() {{ ensure_build_slice(); }}\n\n'
                '    #[cfg(target_os = "macos")]\n    mod macos_only {\n        use super::*;\n'
                f"        {gate}\n        #[test]\n        fn macos_inline_nested() {{ ensure_build_slice(); }}\n    }}\n"
                f'    #[cfg(feature = "x")]\n    mod feature_only {{\n        use super::*;\n'
                f"        {gate}\n        #[test]\n        fn feature_nested() {{ ensure_build_slice(); }}\n    }}\n}}\n"),
        }
        for rel, text in files.items():
            path = self.root / rel
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text)
        self.git("add", ".")
        self.git("commit", "-q", "-m", "base")
        self.git("branch", "base")
        edited = self.root / "crates/rsid/src/agent_jobs.rs"
        edited.write_text(edited.read_text().replace("    1;", "    2;"))
        self.git("commit", "-q", "-a", "-m", "change")

    def git(self, *args):
        scratch_git(self.root, *args)

    @unittest.skipUnless(sys.platform.startswith("linux"), "the CLI reads the real host OS")
    def test_a_linux_host_derives_the_linux_runnable_observers_only(self):
        result = subprocess.run(
            [str(SCRIPT), "--repo", str(self.root), "--base", "base", "--no-compile"],
            capture_output=True, text=True, env=dict(os.environ, CHECK_TOUCHED_SHARDS_CARGO="true"))
        self.assertEqual(result.returncode, 0, result.stdout)
        receipt = json.loads(result.stdout.partition("\n")[2])
        self.assertEqual(receipt["suggested_filters"], [
            "rsid=shard:other-01:test(/^agent_jobs::(?:\\w+::)*(?:feature_nested|linux_probe)$/)",
            "rsid=shard:other-01:test(/^linux_jobs::(?:\\w+::)*linux_file_test$/)",
        ])


class FastPathEquivalenceTest(unittest.TestCase):
    """#2026: the difflib-skipping fast paths answer what the full matching answers."""

    @classmethod
    def setUpClass(cls):
        cls.module = runpy.run_path(str(SCRIPT), run_name="check_touched_shards")

    def reference_rewind_only_change(self, before, after):
        import difflib
        mod = self.module
        def covered(text):
            result = set()
            for name, start, end, _, _ in mod["rust_functions"](text):
                if name in mod["REWIND_HELPERS"]:
                    result.update(range(start, end))
            result.update(i for i, line in enumerate(text.splitlines()) if line.startswith("const REWIND_TEARDOWN_COVERED_THROUGH"))
            return result
        old, new = before.splitlines(), after.splitlines()
        old_ok, new_ok = covered(before), covered(after)
        changes = [op for op in difflib.SequenceMatcher(None, old, new, autojunk=False).get_opcodes() if op[0] != "equal"]
        return bool(changes) and all(
            set(range(i, j)) <= old_ok and set(range(k, l)) <= new_ok for _, i, j, k, l in changes)

    def test_rewind_only_change_matches_full_matching(self):
        helper = "fn rewind_post_v121_tail_to() {\n    step_a();\n    step_b();\n}\n"
        other = "fn unrelated() {\n    step_a();\n}\n"
        const = "const REWIND_TEARDOWN_COVERED_THROUGH: i32 = {};\n"
        cases = [
            (other + helper, other + helper.replace("step_b", "step_c")),
            (other + helper, other.replace("step_a", "step_z") + helper),
            (other + helper + const.format(1), other + helper + const.format(2)),
            (other + helper, other + helper),
            (other + helper, helper + other),
            (other + other + helper, other + helper),
            (other + helper, other + helper + other),
            (other + helper, "fn extra() {}\n" + other + helper.replace("step_b", "step_c")),
        ]
        for before, after in cases:
            with self.subTest(before=before, after=after):
                self.assertEqual(self.module["rewind_only_change"](before, after),
                                 self.reference_rewind_only_change(before, after))

    def test_mod_declarations_is_stable_across_calls(self):
        text = '#[path = "x.rs"]\nmod a;\nmod b {\n    mod c;\n}\ninclude!("y.rs");\nlet s = 1;\n'
        first = self.module["mod_declarations"](text)
        # The last field is whether a gate leaves the declaration out of this host (#2291).
        self.assertEqual(first, (("a", "x.rs", [], False), ("c", None, ["b"], False), (None, "y.rs", [], False)))
        self.assertEqual(self.module["mod_declarations"](text), first)


class PathMountedModuleDirectoryTest(unittest.TestCase):
    """#1697: rustc gives a `#[path]`-mounted file module-root directory ownership, so its own
    `mod tests;` loads `tests.rs` beside it, not `<stem>/tests.rs`."""

    @classmethod
    def setUpClass(cls):
        cls.module = runpy.run_path(str(SCRIPT), run_name="check_touched_shards")

    def closure(self, declaration, extra):
        src = "crates/x/src/"
        files = {
            f"{src}lib.rs": "pub mod store;\n",
            f"{src}store/mod.rs": "pub mod pending;\n",
            f"{src}store/pending.rs": '#[path = "remote_answers.rs"]\npub mod remote_answers;\n',
            f"{src}store/remote_answers.rs": declaration,
            **{src + name: "// file\n" for name in extra},
        }
        return {k.removeprefix(src): v for k, v in self.module["mount_closure"](files, f"{src}lib.rs").items()}

    def test_plain_mod_tests_in_a_path_mounted_file_resolves_beside_it(self):
        mapped = self.closure("#[cfg(test)]\nmod tests;\n", ["store/tests.rs", "store/remote_answers/tests.rs"])
        self.assertEqual(mapped["store/tests.rs"], "store::pending::remote_answers::tests")
        self.assertNotIn("store/remote_answers/tests.rs", mapped)

    def test_an_explicit_path_test_mount_resolves_to_the_module_directory_file(self):
        mapped = self.closure('#[cfg(test)]\n#[path = "remote_answers/tests.rs"]\nmod tests;\n',
                              ["store/tests.rs", "store/remote_answers/tests.rs"])
        self.assertEqual(mapped["store/remote_answers/tests.rs"], "store::pending::remote_answers::tests")
        self.assertNotIn("store/tests.rs", mapped)

    def test_an_ordinary_mounted_file_keeps_its_stem_directory(self):
        src = "crates/x/src/"
        files = {f"{src}lib.rs": "pub mod ordinary;\n", f"{src}ordinary.rs": "mod tests;\n",
                 f"{src}ordinary/tests.rs": "// t\n", f"{src}tests.rs": "// not this one\n"}
        mapped = self.module["mount_closure"](files, f"{src}lib.rs")
        self.assertEqual(mapped.get(f"{src}ordinary/tests.rs"), "ordinary::tests")
        self.assertNotIn(f"{src}tests.rs", mapped)


class LeafCrateObserverTest(unittest.TestCase):
    """#2410: `rsid-remote-read` is a leaf crate split out of rsid. Its own module filter is never
    enough for a production edit, because its callers live in rsid: the retained facade module's
    tests and the sharded tests that name the change observe it too."""

    LEAF = "crates/rsid-remote-read/src/projection.rs"
    PROJECTION = """pub fn project_pending() -> u32 {
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_one_pending_item() {
        assert_eq!(project_pending(), 1);
    }
}
"""
    OWNER_PATH = "crates/rsid/src/remote_read/owner.rs"
    OWNER = GATE.format(shard="store-04", name="owner_reads_the_projection").replace(
        "fn owner_reads_the_projection() {}", "fn owner_reads_the_projection() { assert_eq!(spawn_info_read(), 1); }"
    ) + "pub fn spawn_info_read() -> u32 {\n    project_pending()\n}\n"

    def setUp(self):
        self.temp = scratch_tempdir(prefix="touched-shards-leaf-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.git("init", "-q", "-b", "rolling")
        self.git("config", "user.name", "T")
        self.git("config", "user.email", "t@example.invalid")
        self.write("crates/rsid-remote-read/src/lib.rs", "mod projection;\npub use projection::project_pending;\n")
        self.write(self.LEAF, self.PROJECTION)
        self.write("crates/rsid/src/lib.rs", "pub mod remote_read;\npub mod session;\npub mod other;\n")
        self.write("crates/rsid/src/remote_read/mod.rs", "mod owner;\npub use rsid_remote_read::*;\n")
        self.write(self.OWNER_PATH, self.OWNER)
        self.write("crates/rsid/src/session/mod.rs", "mod queries;\n")
        self.write(
            "crates/rsid/src/session/queries.rs",
            GATE.format(shard="session-01", name="session_probe_sees_the_projection").replace(
                "fn session_probe_sees_the_projection() {}", "fn session_probe_sees_the_projection() { assert_eq!(session_probe(), 1); }"
            ) + "pub fn session_probe() -> u32 {\n    project_pending()\n}\n",
        )
        self.write("crates/rsid/src/other.rs", GATE.format(shard="other-01", name="unrelated_daemon_test"))
        self.git("add", ".")
        self.git("commit", "-q", "-m", "base")
        self.git("branch", "base")
        self.git("checkout", "-q", "-b", "feature")

    def git(self, *args):
        scratch_git(self.root, *args)

    def write(self, rel, text):
        path = self.root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def edit(self, rel, old, new):
        path = self.root / rel
        text = path.read_text()
        self.assertIn(old, text)
        path.write_text(text.replace(old, new, 1))
        self.git("add", ".")
        self.git("commit", "-q", "-m", "change")

    def receipt(self):
        env = dict(os.environ, CHECK_TOUCHED_SHARDS_CARGO="true")
        result = subprocess.run(
            [str(SCRIPT), "--repo", str(self.root), "--base", "base", "--filters-only", "--json"],
            capture_output=True, text=True, env=env,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return json.loads(result.stdout)

    def test_a_leaf_production_edit_selects_its_tests_and_the_daemon_observers(self):
        self.edit(self.LEAF, "    1\n}", "    2\n}")
        filters = self.receipt()["suggested_filters"]
        self.assertIn("rsid-remote-read=projection", filters)
        # The retained facade module owns the runtime integration of the leaf.
        self.assertIn("rsid=shard:store-04:test(remote_read)", filters)
        # A sharded test elsewhere in rsid that names the changed function joins them.
        self.assertTrue(any("session_probe_sees_the_projection" in f for f in filters), filters)
        self.assertFalse(any("unrelated_daemon_test" in f for f in filters), filters)

    def test_a_leaf_production_edit_records_why_in_the_receipt(self):
        self.edit(self.LEAF, "    1\n}", "    2\n}")
        notes = self.receipt()["leaf_observers"]
        self.assertEqual([n["path"] for n in notes], [self.LEAF])
        self.assertEqual(notes[0]["package"], "rsid-remote-read")
        self.assertIn("rsid=shard:store-04:test(remote_read)", notes[0]["filters"])
        self.assertIn("production edit", notes[0]["reason"])

    def test_harness_leaf_selects_its_tests_and_retained_daemon_observers(self):
        leaf = "crates/rsid-harness/src/arguments.rs"
        self.write("crates/rsid-harness/src/lib.rs", "pub mod arguments;\n")
        self.write(leaf, self.PROJECTION)
        self.write("crates/rsid/src/session/mod.rs", "mod harness; mod queries;\n")
        self.write(
            "crates/rsid/src/session/harness/mod.rs",
            "pub use rsid_harness::arguments::*;\n" +
            GATE.format(shard="session-05", name="dispatch_validated_arguments").replace(
                "fn dispatch_validated_arguments() {}",
                "fn dispatch_validated_arguments() { assert_eq!(project_pending(), 1); }",
            ),
        )
        self.git("add", ".")
        self.git("commit", "-q", "-m", "harness fixture")
        self.git("branch", "-f", "base")
        self.edit(leaf, "    1\n}", "    2\n}")
        receipt = self.receipt()
        self.assertIn("rsid-harness=arguments", receipt["suggested_filters"])
        self.assertIn("rsid=shard:session-05:test(session::harness)", receipt["suggested_filters"])
        self.assertEqual(receipt["leaf_observers"][0]["package"], "rsid-harness")

    def test_nested_harness_tool_edit_selects_the_leaf_module_and_daemon_observers(self):
        # #2616: the harness tools are a nested leaf module (`tools::file`) observed through
        # the same `session::harness` facade as the leaf's top-level modules.
        leaf = "crates/rsid-harness/src/tools/file.rs"
        self.write("crates/rsid-harness/src/lib.rs", "pub mod tools;\n")
        self.write("crates/rsid-harness/src/tools/mod.rs", "pub mod file;\n")
        self.write(leaf, self.PROJECTION)
        self.write("crates/rsid/src/session/mod.rs", "mod harness; mod queries;\n")
        self.write(
            "crates/rsid/src/session/harness/mod.rs",
            "pub use rsid_harness::tools::file::*;\n" +
            GATE.format(shard="session-05", name="dispatch_validated_arguments").replace(
                "fn dispatch_validated_arguments() {}",
                "fn dispatch_validated_arguments() { assert_eq!(project_pending(), 1); }",
            ),
        )
        self.git("add", ".")
        self.git("commit", "-q", "-m", "harness tools fixture")
        self.git("branch", "-f", "base")
        self.edit(leaf, "    1\n}", "    2\n}")
        receipt = self.receipt()
        self.assertIn("rsid-harness=tools::file", receipt["suggested_filters"])
        self.assertIn("rsid=shard:session-05:test(session::harness)", receipt["suggested_filters"])
        self.assertEqual(receipt["leaf_observers"][0]["package"], "rsid-harness")

    def test_a_leaf_test_only_edit_selects_only_the_leaf_tests(self):
        self.edit(self.LEAF, "assert_eq!(project_pending(), 1);", 'assert_eq!(project_pending(), 1, "still one");')
        receipt = self.receipt()
        self.assertEqual(receipt["suggested_filters"], ["rsid-remote-read=projection"])
        self.assertEqual(receipt["leaf_observers"][0]["filters"], [])
        self.assertIn("test code", receipt["leaf_observers"][0]["reason"])

    def test_a_leaf_comment_only_edit_selects_no_daemon_observer(self):
        self.edit(self.LEAF, "pub fn project_pending", "// The projection of one pending item.\npub fn project_pending")
        receipt = self.receipt()
        self.assertFalse(any(f.startswith("rsid=shard:") for f in receipt["suggested_filters"]), receipt["suggested_filters"])
        self.assertEqual(receipt["leaf_observers"], [])

    def test_a_leaf_export_edit_derives_no_filter_when_the_trace_is_unprovable(self):
        self.edit("crates/rsid-remote-read/src/lib.rs", "pub use projection::project_pending;", "pub use projection::{project_pending, other_pending};")
        receipt = self.receipt()
        # #2441: an unprovable trace derives no filter, so the lander's complete closure applies.
        self.assertEqual(receipt["suggested_filters"], [])
        self.assertEqual(receipt["unfiltered_leaf_paths"], ["crates/rsid-remote-read/src/lib.rs"])
        self.assertTrue(receipt["leaf_observers"][0]["unfiltered"])

    def test_unprovable_leaf_keeps_its_changed_tests_for_worker_verification(self):
        self.edit(self.LEAF, "    1\n}", "    2\n}")
        self.edit("crates/rsid-remote-read/src/lib.rs", "pub use projection::project_pending;",
                  "pub use projection::{project_pending, other_pending};")
        receipt = self.receipt()
        self.assertEqual(receipt["suggested_filters"], [])
        self.assertIn("rsid-remote-read=projection", receipt["worker_filters"])

    def rebase(self, rel, text):
        """Replace a fixture file on the base commit (the feature branch still sits on it)."""
        self.write(rel, text)
        self.git("add", ".")
        self.git("commit", "-q", "--amend", "-m", "base")
        self.git("branch", "-f", "base", "HEAD")

    def test_a_private_leaf_helper_reaches_the_daemon_observer_through_its_public_wrapper(self):
        # #2441: the changed leaf function is private; only the public wrapper is named in rsid.
        self.rebase(self.LEAF, """fn private_projection() -> u32 {
    1
}

pub fn project_pending() -> u32 {
    private_projection()
}
""")
        self.edit(self.LEAF, "    1\n}\n\npub fn", "    2\n}\n\npub fn")
        receipt = self.receipt()
        filters = receipt["suggested_filters"]
        self.assertIn("rsid=shard:store-04:test(remote_read)", filters)
        self.assertTrue(any("session_probe_sees_the_projection" in f and "shard:session-01:" in f for f in filters), filters)
        self.assertFalse(any("unrelated_daemon_test" in f for f in filters), filters)
        self.assertIn("sharded tests that name the change", receipt["leaf_observers"][0]["reason"])

    def test_a_deep_private_leaf_chain_reaches_the_session_observer(self):
        # #2441: coverage_from_read -> pending_source_coverage -> finish_runtime_only_pending_sources
        # (pub) -> rsid session::queries -> the session test that asserts the coverage state.
        self.rebase(self.LEAF, """fn coverage_from_read() -> u32 {
    1
}

fn pending_source_coverage() -> u32 {
    coverage_from_read()
}

pub fn finish_runtime_only_pending_sources() -> u32 {
    pending_source_coverage()
}
""")
        self.rebase("crates/rsid/src/session/queries.rs", GATE.format(shard="session-01", name="remote_runtime_candidate_snapshot_is_scalar").replace(
            "fn remote_runtime_candidate_snapshot_is_scalar() {}",
            "fn remote_runtime_candidate_snapshot_is_scalar() { assert_eq!(remote_runtime_only_pending_finish(), 1); }",
        ) + "pub fn remote_runtime_only_pending_finish() -> u32 {\n    finish_runtime_only_pending_sources()\n}\n")
        self.edit(self.LEAF, "fn coverage_from_read() -> u32 {\n    1", "fn coverage_from_read() -> u32 {\n    2")
        receipt = self.receipt()
        self.assertTrue(any("remote_runtime_candidate_snapshot_is_scalar" in f for f in receipt["suggested_filters"]),
                        receipt["suggested_filters"])
        self.assertFalse(any("unrelated_daemon_test" in f for f in receipt["suggested_filters"]), receipt["suggested_filters"])

    def test_a_private_leaf_helper_nothing_observes_selects_the_facade_tests_only(self):
        self.rebase(self.LEAF, """fn dead_helper() -> u32 {
    1
}

pub fn project_pending() -> u32 {
    1
}
""")
        self.edit(self.LEAF, "fn dead_helper() -> u32 {\n    1", "fn dead_helper() -> u32 {\n    2")
        receipt = self.receipt()
        self.assertFalse(any("session_probe_sees_the_projection" in f for f in receipt["suggested_filters"]))
        self.assertIn("rsid=shard:store-04:test(remote_read)", receipt["suggested_filters"])
        self.assertIn("no other sharded test named the change", receipt["leaf_observers"][0]["reason"])

    def test_an_ambiguous_public_leaf_name_derives_no_filter_and_says_why(self):
        # A pub leaf helper whose name another module also defines cannot be followed by name.
        self.rebase(self.LEAF, """fn shared() -> u32 {
    1
}

pub fn shared_pending() -> u32 {
    shared()
}

pub fn shared_wrapper() -> u32 {
    shared_pending()
}
""")
        self.rebase("crates/rsid/src/other.rs", GATE.format(shard="other-01", name="unrelated_daemon_test")
                    + "pub fn shared_pending() -> u32 {\n    0\n}\n")
        self.edit(self.LEAF, "fn shared() -> u32 {\n    1", "fn shared() -> u32 {\n    2")
        receipt = self.receipt()
        self.assertEqual(receipt["suggested_filters"], [])
        self.assertIn("ambiguous exported helper", receipt["leaf_observers"][0]["reason"])

    def test_a_retained_owner_edit_selects_the_owner_tests_and_no_leaf_filter(self):
        self.edit(self.OWNER_PATH, "    project_pending()\n", "    project_pending() + 0\n")
        receipt = self.receipt()
        self.assertIn("rsid=shard:store-04:test(remote_read::owner)", receipt["suggested_filters"])
        self.assertFalse(any(f.startswith("rsid-remote-read=") for f in receipt["suggested_filters"]), receipt["suggested_filters"])
        self.assertEqual(receipt["leaf_observers"], [])


class RealRemoteReadChainTest(unittest.TestCase):
    """#2446: the production observer path of #2441, from trimmed copies of the real files.

    `fixtures/touched_shards_remote_chain/` holds the real (multi-screen) bodies of
    `finish_runtime_only_pending_sources`, `coverage_from_read`, `coverage_from_key_page` and
    `pending_source_coverage` from `rsid-remote-read/src/projection.rs`, the real
    `SessionManager::remote_runtime_only_pending_{capture,finish}` wrappers from
    `rsid/src/session/queries.rs`, and the real session test
    `remote_runtime_candidate_snapshot_is_scalar_and_nonblocking` from `rsid/src/session/tests.rs`.
    The chain under test is `coverage_from_read` (private) -> `pending_source_coverage` ->
    `finish_runtime_only_pending_sources` (pub, re-exported by the facade) ->
    `remote_runtime_only_pending_finish` (method) -> the session test.
    """

    FIXTURES = Path(__file__).resolve().parent / "fixtures" / "touched_shards_remote_chain"
    LEAF = "crates/rsid-remote-read/src/projection.rs"
    SESSION_TEST = "remote_runtime_candidate_snapshot_is_scalar_and_nonblocking"

    def fixture(self, name):
        return (self.FIXTURES / name).read_text()

    def setUp(self):
        self.temp = scratch_tempdir(prefix="touched-shards-real-chain-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.git("init", "-q", "-b", "rolling")
        self.git("config", "user.name", "T")
        self.git("config", "user.email", "t@example.invalid")
        projection = self.fixture("projection.rs.txt")
        # The fixture is only meaningful while it keeps the long production bodies.
        self.assertGreater(len(projection.splitlines()), 200)
        self.write(
            "crates/rsid-remote-read/src/lib.rs",
            "mod projection;\npub use projection::{finish_runtime_only_pending_sources, pending_source_coverage};\n",
        )
        self.write(self.LEAF, projection)
        self.write("crates/rsid/src/lib.rs", "pub mod remote_read;\npub mod session;\npub mod other;\n")
        self.write("crates/rsid/src/remote_read/mod.rs", "mod owner;\npub use rsid_remote_read::*;\n")
        self.write("crates/rsid/src/remote_read/owner.rs", GATE.format(shard="store-04", name="owner_reads_the_projection"))
        self.write("crates/rsid/src/session/mod.rs", "mod queries;\n#[cfg(test)]\nmod tests;\n")
        self.write("crates/rsid/src/session/queries.rs", self.fixture("queries.rs.txt"))
        self.write("crates/rsid/src/session/tests.rs", self.fixture("session_tests.rs.txt"))
        self.write("crates/rsid/src/other.rs", GATE.format(shard="other-01", name="unrelated_daemon_test"))
        self.git("add", ".")
        self.git("commit", "-q", "-m", "base")
        self.git("branch", "base")
        self.git("checkout", "-q", "-b", "feature")

    def git(self, *args):
        scratch_git(self.root, *args)

    def write(self, rel, text):
        path = self.root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def edit(self, rel, old, new):
        path = self.root / rel
        text = path.read_text()
        self.assertIn(old, text)
        path.write_text(text.replace(old, new, 1))
        self.git("add", ".")
        self.git("commit", "-q", "-m", "change")

    def receipt(self):
        env = dict(os.environ, CHECK_TOUCHED_SHARDS_CARGO="true")
        result = subprocess.run(
            [str(SCRIPT), "--repo", str(self.root), "--base", "base", "--filters-only", "--json"],
            capture_output=True, text=True, env=env,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return json.loads(result.stdout)

    def test_the_fixture_keeps_the_real_chain_and_long_bodies(self):
        projection = (self.root / self.LEAF).read_text()
        for name in ("coverage_from_read", "pending_source_coverage", "finish_runtime_only_pending_sources"):
            self.assertIn("fn " + name, projection)
        self.assertIn("remote_runtime_only_pending_finish(", (self.root / "crates/rsid/src/session/tests.rs").read_text())
        self.assertIn("finish_runtime_only_pending_sources(", (self.root / "crates/rsid/src/session/queries.rs").read_text())

    def test_a_body_edit_in_coverage_from_read_reaches_the_session_observer(self):
        self.edit(self.LEAF, "if available { count } else { 0 },", "if available { count } else { 1 },")
        receipt = self.receipt()
        filters = receipt["suggested_filters"]
        self.assertTrue(
            any(self.SESSION_TEST in f and "shard:session-03:" in f for f in filters),
            (filters, receipt.get("unfiltered_leaf_paths")),
        )
        self.assertEqual(receipt["unfiltered_leaf_paths"], [])
        self.assertFalse(any("unrelated_daemon_test" in f for f in filters), filters)
        self.assertIn("rsid=shard:store-04:test(remote_read)", filters)


class PlanningLeafObserverTest(unittest.TestCase):
    """#2416: `rsi-graph::planning` (pure topology planning, #2415) is a leaf whose callers live in the
    retained rsid facades `topology` and `session::graph_runner`. A production edit selects the daemon
    tests that name it; a test-only or retained daemon-owner edit does not."""

    LEAF = "crates/rsi-graph/src/planning/graph.rs"
    PLANNING = """pub fn plan_edges() -> u32 {
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_one_edge() {
        assert_eq!(plan_edges(), 1);
    }
}
"""
    TOPOLOGY_TESTS = GATE.format(shard="store-04", name="executor_runs_the_planned_edges").replace(
        "fn executor_runs_the_planned_edges() {}", "fn executor_runs_the_planned_edges() { assert_eq!(run_plan(), 1); }"
    ) + "pub fn run_plan() -> u32 {\n    plan_edges()\n}\n"
    RUNNER_PATH = "crates/rsid/src/session/graph_runner.rs"
    RUNNER = GATE.format(shard="session-01", name="graph_runner_uses_the_planned_edges").replace(
        "fn graph_runner_uses_the_planned_edges() {}", "fn graph_runner_uses_the_planned_edges() { assert_eq!(runner_edges(), 1); }"
    ) + "pub fn runner_edges() -> u32 {\n    plan_edges()\n}\n"

    def setUp(self):
        self.temp = scratch_tempdir(prefix="touched-shards-planning-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.git("init", "-q", "-b", "rolling")
        self.git("config", "user.name", "T")
        self.git("config", "user.email", "t@example.invalid")
        self.write("crates/rsi-graph/src/lib.rs", "pub mod planning;\n")
        self.write("crates/rsi-graph/src/planning/mod.rs", "pub mod graph;\npub use graph::plan_edges;\n")
        self.write(self.LEAF, self.PLANNING)
        self.write("crates/rsi-graph/src/compiler.rs", "pub fn compile_graph() -> u32 {\n    1\n}\n")
        self.write("crates/rsid/src/lib.rs", "pub mod topology;\npub mod session;\npub mod other;\n")
        self.write("crates/rsid/src/topology/mod.rs", "mod tests;\n")
        self.write("crates/rsid/src/topology/tests.rs", self.TOPOLOGY_TESTS)
        self.write("crates/rsid/src/session/mod.rs", "pub(crate) mod graph_runner;\n")
        self.write(self.RUNNER_PATH, self.RUNNER)
        self.write("crates/rsid/src/other.rs", GATE.format(shard="other-01", name="unrelated_daemon_test"))
        self.git("add", ".")
        self.git("commit", "-q", "-m", "base")
        self.git("branch", "base")
        self.git("checkout", "-q", "-b", "feature")

    def git(self, *args):
        scratch_git(self.root, *args)

    def write(self, rel, text):
        path = self.root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def edit(self, rel, old, new):
        path = self.root / rel
        text = path.read_text()
        self.assertIn(old, text)
        path.write_text(text.replace(old, new, 1))
        self.git("add", ".")
        self.git("commit", "-q", "-m", "change")

    def receipt(self):
        env = dict(os.environ, CHECK_TOUCHED_SHARDS_CARGO="true")
        result = subprocess.run(
            [str(SCRIPT), "--repo", str(self.root), "--base", "base", "--filters-only", "--json"],
            capture_output=True, text=True, env=env,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return json.loads(result.stdout)

    def test_a_planning_production_edit_selects_the_topology_and_graph_runner_observers(self):
        self.edit(self.LEAF, "    1\n}", "    2\n}")
        receipt = self.receipt()
        filters = receipt["suggested_filters"]
        self.assertIn("rsi-graph=planning::graph", filters)
        self.assertTrue(any("executor_runs_the_planned_edges" in f for f in filters), filters)
        self.assertTrue(any("graph_runner_uses_the_planned_edges" in f for f in filters), filters)
        self.assertFalse(any("unrelated_daemon_test" in f for f in filters), filters)
        notes = receipt["leaf_observers"]
        self.assertEqual([n["path"] for n in notes], [self.LEAF])
        self.assertEqual(notes[0]["package"], "rsi-graph")
        self.assertIn("production edit", notes[0]["reason"])

    def test_an_unprovable_planning_edit_derives_no_filter(self):
        self.edit("crates/rsi-graph/src/planning/mod.rs", "pub use graph::plan_edges;", "pub use graph::{plan_edges, other_edges};")
        receipt = self.receipt()
        # #2441: an unprovable trace derives no filter, so the lander's complete closure applies.
        self.assertEqual(receipt["suggested_filters"], [])
        self.assertEqual(receipt["unfiltered_leaf_paths"], ["crates/rsi-graph/src/planning/mod.rs"])

    def test_a_planning_test_only_edit_selects_only_the_planning_tests(self):
        self.edit(self.LEAF, "assert_eq!(plan_edges(), 1);", 'assert_eq!(plan_edges(), 1, "one edge");')
        receipt = self.receipt()
        self.assertEqual(receipt["suggested_filters"], ["rsi-graph=planning::graph"])
        self.assertEqual(receipt["leaf_observers"][0]["filters"], [])
        self.assertIn("test code", receipt["leaf_observers"][0]["reason"])

    def test_a_retained_daemon_owner_edit_selects_the_owner_tests_and_no_planning_filter(self):
        self.edit("crates/rsid/src/topology/tests.rs", "    plan_edges()\n", "    plan_edges() + 0\n")
        receipt = self.receipt()
        self.assertTrue(any("executor_runs_the_planned_edges" in f or "topology" in f for f in receipt["suggested_filters"]),
                        receipt["suggested_filters"])
        self.assertFalse(any(f.startswith("rsi-graph=") for f in receipt["suggested_filters"]), receipt["suggested_filters"])
        self.assertEqual(receipt["leaf_observers"], [])

    def test_an_edit_outside_planning_keeps_the_generic_dependency_treatment(self):
        self.edit("crates/rsi-graph/src/compiler.rs", "    1\n}", "    2\n}")
        self.assertEqual(self.receipt()["leaf_observers"], [])


class PlanningNeverCompilesTest(unittest.TestCase):
    """#2564/#222: planning starts no cargo build/check/test; compiling is an explicit, labelled mode.

    The scratch repository lives outside this sandbox and every Git call names it with `-C`.
    The cargo stub records each call and refuses everything except offline `--no-deps` metadata
    (and `check` when a test allows it), so hidden compilation turns these tests red.
    """

    STUB = ("#!/bin/sh\n"
            'echo "$*" >> "$CARGO_LOG"\n'
            'case "$*" in "metadata --no-deps --offline --format-version 1") '
            "echo '{\"target_directory\": \"/nonexistent-target\"}'; exit 0;; esac\n"
            'if [ -n "$STUB_ALLOW_CHECK" ]; then case "$1" in check) exit 0;; esac; fi\n'
            'echo "stub cargo: refusing $*" >&2\n'
            "exit 97\n")

    def setUp(self):
        self.temp = scratch_tempdir(prefix="planning-no-compile-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.assertTrue(self.root.is_dir())
        self.assertFalse(self.root.is_relative_to(Path(__file__).resolve().parents[2]))
        self.repo = self.root / "repo"
        self.repo.mkdir()
        self.log = self.root / "cargo.log"
        self.bin = self.root / "bin"
        self.bin.mkdir()
        for name in ("cargo", "cargo-stub"):
            (self.bin / name).write_text(self.STUB)
            (self.bin / name).chmod(0o755)
        self.git("init", "-q", "-b", "rolling")
        self.git("config", "user.name", "T")
        self.git("config", "user.email", "t@example.invalid")
        self.write("crates/rsid/src/foo.rs", GATE.format(shard="store-02", name="a"))
        self.git("add", "crates")
        self.git("commit", "-q", "-m", "base")
        self.git("branch", "base")
        self.write("crates/rsid/src/foo.rs", GATE.format(shard="store-02", name="a") + "pub fn changed() {}\n")
        self.git("add", "crates")
        self.git("commit", "-q", "-m", "change")

    def git(self, *args):
        scratch_git(self.repo, *args)

    def write(self, rel, text):
        path = self.repo / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def environment(self, **extra):
        env = {k: v for k, v in os.environ.items() if k not in ("CARGO_TARGET_DIR", "RSI_CARGO_SLOT_HELD")}
        env.update(CHECK_TOUCHED_SHARDS_CARGO=str(self.bin / "cargo-stub"), CARGO_LOG=str(self.log),
                   PATH=str(self.bin) + os.pathsep + env.get("PATH", ""), HOME=str(self.root), **extra)
        return env

    def check(self, *args, **env):
        return subprocess.run([str(SCRIPT), "--repo", str(self.repo), "--base", "base", *args], cwd=self.root,
                              capture_output=True, text=True, env=self.environment(**env))

    def cargo_calls(self):
        return self.log.read_text().splitlines() if self.log.exists() else []

    def test_filters_only_plans_the_filters_without_any_cargo_call(self):
        done = self.check("--filters-only", "--json")
        self.assertEqual(done.returncode, 0, done.stderr)
        receipt = json.loads(done.stdout)
        self.assertEqual(self.cargo_calls(), [])
        self.assertTrue(receipt["suggested_filters"], receipt)
        self.assertEqual(receipt["compile"], {
            "mode": "filters_only", "status": "skipped", "cargo_check_invoked": False, "shards_checked": [],
            "detail": "planning only: compile validation belongs to the supervised scoped_test job"})

    def test_no_compile_maps_the_shards_without_any_cargo_call_and_says_skipped(self):
        done = self.check("--no-compile", "--json")
        self.assertEqual(done.returncode, 0, done.stderr)
        receipt = json.loads(done.stdout)
        self.assertEqual(self.cargo_calls(), [])
        self.assertEqual(receipt["shards"]["map"], {"store-02": ["crates/rsid/src/foo.rs"]})
        self.assertEqual((receipt["compile"]["mode"], receipt["compile"]["status"]), ("no_compile", "skipped"))
        self.assertTrue(receipt["shards"]["compile_skipped"])

    def test_a_bare_run_is_refused_instead_of_silently_compiling(self):
        done = self.check()
        self.assertEqual(done.returncode, 2, done.stdout)
        self.assertIn("explicit verifier mode", done.stderr)
        self.assertIn("--filters-only", done.stderr)
        self.assertEqual(done.stdout, "")
        self.assertEqual(self.cargo_calls(), [])

    def test_compile_cannot_be_combined_with_a_planning_mode(self):
        for planning in ("--filters-only", "--no-compile"):
            with self.subTest(planning=planning):
                done = self.check("--compile", planning)
                self.assertEqual(done.returncode, 2, done.stdout)
                self.assertEqual(self.cargo_calls(), [])

    def test_compile_mode_runs_the_shard_check_and_labels_it_executed(self):
        done = self.check("--compile", "--json", STUB_ALLOW_CHECK="1")
        self.assertEqual(done.returncode, 0, done.stderr)
        receipt = json.loads(done.stdout)
        calls = self.cargo_calls()
        self.assertEqual(len(calls), 1, calls)
        self.assertRegex(calls[0], r"^check -p rsid --lib --tests --no-default-features --features test-shard-store-02$")
        self.assertEqual(receipt["compile"], {"mode": "compile", "status": "executed",
                                              "cargo_check_invoked": True, "shards_checked": ["store-02"]})
        self.assertEqual(receipt["shards"]["compiled_ok"], ["store-02"])

    def test_a_failing_compile_still_fails_the_gate(self):
        done = self.check("--compile", "--json")
        self.assertEqual(done.returncode, 1, done.stdout)
        receipt = json.loads(done.stdout)
        self.assertFalse(receipt["ok"])
        self.assertEqual(receipt["shards"]["compiled_failed"], ["store-02"])
        self.assertEqual(receipt["compile"]["status"], "executed")

    def test_the_target_directory_lookup_is_offline_metadata_only(self):
        module = runpy.run_path(str(SCRIPT), run_name="check_touched_shards")
        with mock.patch.dict(os.environ, self.environment(), clear=True):
            self.assertEqual(module["target_directory"](self.repo), Path("/nonexistent-target"))
        self.assertEqual(self.cargo_calls(), ["metadata --no-deps --offline --format-version 1"])


class CargoPrefixSlotTest(unittest.TestCase):
    """#1560: a job recipe already holds a governor slot; do not take a second."""

    def prefix(self, **env):
        keep = {k: v for k, v in os.environ.items() if k not in ("CHECK_TOUCHED_SHARDS_CARGO", "RSI_CARGO_SLOT_HELD")}
        old = dict(os.environ)
        os.environ.clear()
        os.environ.update(keep, **env)
        try:
            return runpy.run_path(str(SCRIPT), run_name="check_touched_shards")["cargo_prefix"]()
        finally:
            os.environ.clear()
            os.environ.update(old)

    def test_held_slot_is_not_acquired_again(self):
        self.assertEqual(self.prefix(RSI_CARGO_SLOT_HELD="1")[-1], "cargo")
        self.assertFalse(any(part.endswith("cargo-slot") for part in self.prefix(RSI_CARGO_SLOT_HELD="1")))

    def test_without_marker_the_slot_wrapper_is_used_when_installed(self):
        slot = Path.home() / ".rsi/bin/cargo-slot"
        expected = [str(slot)] if slot.exists() else []
        self.assertEqual(self.prefix()[: len(expected)], expected)


if __name__ == "__main__":
    unittest.main()
