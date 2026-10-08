"""Intent checks for scripts/check-touched-shards (#1099): one typed receipt per candidate."""

import json
import os
from pathlib import Path
import shutil
import runpy
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "check-touched-shards"
GATE = '#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-{shard}"))]\n#[test]\nfn {name}() {{}}\n'


class CheckTouchedShardsTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="touched-shards-test-")
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
        self.git("add", ".")
        self.git("commit", "-q", "-m", "base")
        self.git("branch", "base")
        self.git("checkout", "-q", "-b", "feature")

    def git(self, *args):
        subprocess.run(["git", *args], cwd=self.root, check=True, capture_output=True)

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
            [str(SCRIPT), "--repo", str(self.root), "--base", "base", *extra],
            capture_output=True, text=True, env=env,
        )
        summary, _, rest = result.stdout.partition("\n")
        return result.returncode, summary, json.loads(rest)

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
            (("pub struct Store;", "pub struct Store; // changed"),),
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
        self.write("crates/rsid/src/foo.rs", GATE.format(shard="store-02", name="a") + "// edit\n")
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
            "rsid=shard:other-03:test(session)", "rsid=shard:store-02:test(session)",
        ])

    def test_removing_code_from_mod_rs_retains_the_root_filter(self):
        root = self.registration_fixture()
        self.write(root, "mod old;\nmod new;\n")
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["suggested_filters"], [
            "rsid=shard:other-03:test(session)", "rsid=shard:store-02:test(session)",
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

    def test_failed_shard_compile_fails_the_receipt(self):
        self.write("crates/rsid/src/foo.rs", GATE.format(shard="store-02", name="a") + "// edit\n")
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
        self.assertEqual(receipt["migrations"], {"new": [152], "base_head": 150, "numbering_conflict": True, "stray_pre_split": []})
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
        self.write("crates/rsid/src/session/tests/manager_issue_worker.rs", GATE.format(shard="other-03", name="w") + "// edit\n")
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
        self.write("crates/rsid/src/foo.rs", GATE.format(shard="store-01", name="a") + helper + "// edit\n")
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

    def test_aggregate_gate_budget_traces_small_modules_and_merges_coverage(self):
        root, original = self.ledger_observation_fixture()
        for i in range(12):
            self.write(f"crates/rsid/src/small_{i:02d}.rs", "pub fn unobserved_%d() {}\n" % i
                       + self.gated_tests("other-03", [f"t{i}"]))
        self.commit("small inventories")
        self.git("branch", "-f", "base")
        for i in range(12):
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
                   self.gated_tests("other-03", ["a1", "a2"]) + '#[path = "manager_issue_worker.rs"]\nmod issue_worker;\n// edit\n')
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
        self.write("crates/rsid/src/session/tests/manager_actions.rs", '#[path = "manager_issue_worker.rs"]\nmod issue_worker;\n// edit\n')
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
                   self.gated_tests("other-03", ["a1", "a2"]) + '#[path = "manager_issue_worker.rs"]\nmod issue_worker;\n// edit\n')
        self.commit()
        emitted.update(self.check()[2]["suggested_filters"])
        self.assertTrue(emitted, "the synthetic diffs emitted no filter")
        self.assertLessEqual(emitted, set(fixture), "update scripts/tests/fixtures/touched-shard-filters.txt")

    def test_a_rpc_change_pins_the_catalog_tests_by_name_not_the_rpc_tests_module(self):
        # #1440: `rpc::tests` (and `test(rpc)`) is ~240 slow fixtures; the pin is the
        # catalog / operator_only tests, and an added dispatch arm selects the tests that name it.
        arms = "".join(f'            "Verb{n}" => self.handle_{n}(),\n' for n in range(70))
        rpc = "mod tests;\nimpl RpcServer {\n    fn dispatch(&self) {\n        match m {\n" + arms + "        }\n    }\n}\n"
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
        self.assertIn("rsid=shard:other-03:test(/^rpc::tests::(?:\\w+::)*export_thing_is_refused$/)", filters)
        self.assertIn("rsid=shard:other-03:test(/^rpc::tests::(?:\\w+::)*(?:login_is_operator_only|verbs_are_cataloged)$/)", filters)
        self.assertEqual(len(filters), 2, filters)
        self.assertNotIn("rsid=shard:other-03:test(rpc)", filters)
        self.assertNotIn("rsid=shard:other-03:test(rpc::tests)", filters)
        self.assertFalse(receipt["broad"])

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
        self.write("crates/rsid/src/session/shared_fixtures/portfolio_levels.rs", self.gated_tests("other-03", ["l1"]) + "// edit\n")
        self.commit()
        code, summary, receipt = self.check()
        self.assertEqual(code, 0, summary)
        self.assertEqual(receipt["suggested_filters"], ["rsid=shard:other-03:test(session::tests::portfolio::levels)"])
        self.assertEqual(receipt["dropped_filters"], [])


CHECKER = Path(__file__).resolve().parents[1] / "check-rsid-test-shards.py"
RSID_MANIFEST = (
    '[features]\ndefault = []\ntest-shard-mode = []\n'
    'test-shard-store-02 = ["test-shard-mode"]\ntest-shard-other-03 = ["test-shard-mode"]\n'
)
STORE_MANIFEST = (
    '[features]\ndefault = []\ntest-shard-mode = []\n'
    'test-shard-store-01 = ["test-shard-mode"]\ntest-shard-store-02 = ["test-shard-mode"]\n'
)


class SplitStoreReceiptTest(unittest.TestCase):
    """#1021 S4: rsid-store changes are mapped, compiled and reported like rsid's."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="touched-shards-split-")
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
        self.write("crates/rsid/src/foo.rs", GATE.format(shard="store-02", name="a"))
        self.write("crates/rsid/src/bar.rs", "pub fn bar() {}\n")
        self.write("crates/rsid/src/bar/tests.rs", GATE.format(shard="other-03", name="b"))
        self.write("crates/rsid-store/src/store/q.rs", GATE.format(shard="store-01", name="c"))
        self.write("crates/rsid-store/src/store/r.rs", GATE.format(shard="store-02", name="d"))
        self.write("crates/rsid-store/src/store/migrations/v150.rs", "// v150\n")
        self.git("add", ".")
        self.git("commit", "-q", "-m", "base")
        self.git("branch", "base")
        self.git("checkout", "-q", "-b", "feature")

    def git(self, *args):
        subprocess.run(["git", *args], cwd=self.root, check=True, capture_output=True)

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
            [str(SCRIPT), "--repo", str(self.root), "--base", "base"],
            capture_output=True, text=True, env=env,
        )
        summary, _, rest = result.stdout.partition("\n")
        commands = self.log.read_text().splitlines() if self.log.exists() else []
        return result.returncode, summary, json.loads(rest), commands

    def test_an_rsid_store_source_change_compiles_its_shard_and_one_dependent_shard(self):
        self.write("crates/rsid-store/src/store/q.rs", GATE.format(shard="store-01", name="c") + "// edit\n")
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
        self.write("crates/rsid-store/src/store/r.rs", GATE.format(shard="store-02", name="d") + "// edit\n")
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
        result = subprocess.run([str(SCRIPT), "--repo", str(self.root), "--base", "base"], capture_output=True, text=True, env=env)
        summary, _, rest = result.stdout.partition("\n")
        commands = self.log.read_text().splitlines() if self.log.exists() else []
        return result.returncode, summary, json.loads(rest), commands, result.stderr

    def edit_store_with_manifests(self):
        self.write("crates/rsid/Cargo.toml", '[package]\nname = "rsid"\n' + RSID_MANIFEST)
        self.write("crates/rsid-store/Cargo.toml", '[package]\nname = "rsid-store"\n' + STORE_MANIFEST)
        self.write("crates/rsid-store/src/store/q.rs", GATE.format(shard="store-01", name="c") + "// edit\n")
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
        self.write("crates/rsid-store/src/store/q.rs", GATE.format(shard="store-01", name="c") + "// edit\n")
        self.write("crates/rsid-store/src/store/r.rs", GATE.format(shard="store-02", name="d") + "// edit\n")
        self.commit()
        self.cargo.write_text("#!/bin/sh\necho 'error[E0609]: no field continue_from on type X' >&2\nexit 101\n")
        code, _, receipt, _, stderr = self.check_with()
        self.assertEqual(code, 1)
        stale = receipt["shards"]["shard_compile_suspect_stale"]
        self.assertIn("CHECK_TOUCHED_SHARDS_CLEAN=1", stale["hint"])
        self.assertIn("continue_from", stale["error"])
        self.assertIn("shard_compile_suspect_stale", stderr)

    def test_failed_compile_reports_first_error_and_span_before_warning_tail(self):
        self.write("crates/rsid/src/foo.rs", GATE.format(shard="store-02", name="a") + "// edit\n")
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
                self.assertLess(len(stderr), 6500)

    def test_failed_compile_bounds_error_blocks_by_lines_and_characters(self):
        self.write("crates/rsid/src/foo.rs", GATE.format(shard="store-02", name="a") + "// edit\n")
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
                self.assertLess(len(stderr), 6500)

    def test_failed_compile_without_error_diagnostics_keeps_stderr_tail(self):
        self.write("crates/rsid/src/foo.rs", GATE.format(shard="store-02", name="a") + "// edit\n")
        self.commit()
        self.cargo.write_text("#!/bin/sh\necho 'compiler wrapper unavailable' >&2\nexit 7\n")
        code, _, receipt, _, stderr = self.check_with()
        self.assertEqual(code, 1)
        self.assertEqual(receipt["shards"]["compiled_failed"], ["store-02"])
        self.assertIn("exit code: 7\ncompiler wrapper unavailable\n", stderr)

    def test_different_shard_failures_are_not_suspected_stale(self):
        self.write("crates/rsid-store/src/store/q.rs", GATE.format(shard="store-01", name="c") + "// edit\n")
        self.write("crates/rsid-store/src/store/r.rs", GATE.format(shard="store-02", name="d") + "// edit\n")
        self.commit()
        self.cargo.write_text('#!/bin/sh\necho "error: broke $*" >&2\nexit 101\n')
        code, _, receipt, _, _ = self.check_with()
        self.assertEqual(code, 1)
        self.assertIsNone(receipt["shards"]["shard_compile_suspect_stale"])


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
