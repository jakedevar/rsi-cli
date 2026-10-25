"""Fixtures for the SQLite/store ownership guard (#453)."""

import importlib.util
from pathlib import Path
import subprocess
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from scratch_git import scratch_git, scratch_tempdir

SCRIPT = Path(__file__).resolve().parents[1] / "check-store-boundary.py"
SPEC = importlib.util.spec_from_file_location("store_boundary", SCRIPT)
GUARD = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(GUARD)
PATH = "crates/rsid/src/new_state.rs"
FIX = "add an rsid-store accessor or derive the state from existing records"


class StoreBoundaryTest(unittest.TestCase):
    def assert_refused(self, source, path=PATH):
        findings = GUARD.scan(path, source)
        self.assertTrue(findings)
        for finding in findings:
            self.assertIn("store boundary rule:", finding)
            self.assertIn(FIX, finding)
        return findings

    def test_raw_rsi_db_connection_fixture_fails(self):
        self.assert_refused('fn persist() { rusqlite::Connection::open("rsi.db"); }')

    def test_open_variants_and_import_alias_fail(self):
        for call in ('Connection::open(path)', 'Connection::open_with_flags(path, flags)',
                     'rusqlite::Connection::open_in_memory()',
                     'Db::open(path)'):
            with self.subTest(call=call):
                self.assert_refused('use rusqlite::Connection as Db; fn f() { ' + call + '; }')

    def test_per_session_key_fixture_fails(self):
        self.assert_refused('''fn persist(store: &Store, session: Uuid) {
            let key = format!("preamble:{session}");
            store.set_daemon_setting(&key, "seen");
        }''')

    def test_computed_key_forms_fail(self):
        for key in ('&format!("preamble:{}", session)', 'concat!("prefix", session)',
                    '&(PREFIX.to_owned() + &uuid.to_string())', '&session.to_string()',
                    '&key_for(session)'):
            with self.subTest(key=key):
                self.assert_refused(f'fn f() {{ store.set_daemon_setting({key}, "value"); }}')

    def test_raw_sql_with_computed_key_fails(self):
        for sql in ('INSERT INTO daemon_settings(key,value) VALUES(?1,?2)',
                    'INSERT OR REPLACE INTO daemon_settings(key,value) VALUES(?1,?2)',
                    'UPDATE daemon_settings SET value=?2 WHERE key=?1'):
            with self.subTest(sql=sql):
                self.assert_refused('fn f() { let key = format!("session:{uuid}"); '
                                    f'conn.execute("{sql}", params![key, value]); }}')

    def test_constant_settings_and_store_accessors_pass(self):
        source = '''fn f() {
            store.set_daemon_setting("fixed", &format!("value:{uuid}"));
            store.set_daemon_setting(PROBE_KEY, &nonce);
            store.set_daemon_setting(crate::config::KEY, "value");
            store.persist_preamble(session);
        }'''
        self.assertEqual(GUARD.scan(PATH, source), [])
        self.assertEqual(GUARD.scan("crates/rsid-store/src/store/daemon_settings.rs",
                                   'fn f() { self.set_daemon_setting(&format!("{id}"), ""); }'), [])

    def test_other_crates_cannot_write_dynamic_settings(self):
        self.assert_refused('fn f() { store.set_daemon_setting(&format!("{uuid}"), ""); }',
                            "crates/rsi/src/rogue.rs")

    def test_separate_memory_store_is_explicitly_allowed(self):
        self.assertEqual(GUARD.scan("crates/rsid/src/memory/store.rs",
                                   'fn open() { Connection::open(path); }'), [])
        self.assert_refused('fn f() { Connection::open(path); }',
                            "crates/rsid/src/codegraph/new_store.rs")

    def test_test_modules_and_items_pass_but_later_production_fails(self):
        test_source = '''#[cfg(test)]
            mod tests { fn fixture() { Connection::open("rsi.db");
                store.set_daemon_setting(&format!("{uuid}"), ""); } }
            #[cfg(test)] fn helper() { Connection::open_in_memory(); }
            #[tokio::test] async fn fixture() { Connection::open("rsi.db"); }
        '''
        self.assertEqual(GUARD.scan(PATH, test_source), [])
        self.assert_refused(test_source + '\nfn production() { Connection::open("rsi.db"); }')
        self.assertEqual(GUARD.scan("crates/rsid/src/session/tests.rs",
                                   'fn f() { Connection::open("rsi.db"); }'), [])

    def test_comments_and_strings_pass(self):
        source = '''// Connection::open("rsi.db");
            /* nested /* Connection::open(path); */ comment */
            fn f() { let example = r###"Connection::open("rsi.db");"###;
                let text = "store.set_daemon_setting(&key, value)"; }
        '''
        self.assertEqual(GUARD.scan(PATH, source), [])

    def test_sql_comments_and_test_fixtures_pass(self):
        source = '''fn live() {
            // INSERT INTO daemon_settings(key,value) VALUES(?1,?2)
            /* nested /* "UPDATE daemon_settings SET value=?1" */ comment */
            let quote = '"';
        }
        #[cfg(test)] mod tests { fn fixture() {
            conn.execute(r#"UPDATE daemon_settings SET value=?1"#, []);
        } }
        '''
        self.assertEqual(GUARD.scan(PATH, source), [])

    def test_moved_writers_and_copier_require_store_accessors(self):
        for path, call in (
            ("session/context_cap.rs", 'store.set_daemon_setting(&marker_key(session_id), "value")'),
            ("session/context_cap.rs", 'store.set_daemon_setting(&handoff_key(session_id), "value")'),
            ("session/context_succession.rs", 'store.set_daemon_setting(&marker_key(session_id), "value")'),
            ("mcp_config.rs", 'store.set_daemon_setting(&key, "value")'),
            ("operator_config/custom_providers.rs",
             'store.set_daemon_setting(&setting_key(row.id), "value")'),
            ("operator_config/custom_providers.rs",
             'store.set_daemon_setting(&route_key(role), "value")'),
            ("recursive_dag/smoke_fixture.rs", 'Connection::open(output_db)'),
            ("recursive_dag/smoke_fixture.rs",
             'Connection::open_with_flags(source_db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,)'),
        ):
            with self.subTest(path=path, call=call):
                self.assert_refused('fn old() {' + call + ';}', "crates/rsid/src/" + path)

    def test_cli_tracked_fixture_and_archive_both_fail(self):
        with scratch_tempdir(prefix="rsi-store-boundary-test-") as temp:
            root = Path(temp)
            path = root / PATH
            path.parent.mkdir(parents=True)
            path.write_text('fn f() { rusqlite::Connection::open("rsi.db"); }')
            for tracked in (False, True):
                if tracked:
                    scratch_git(root, "init", "-q")
                    scratch_git(root, "add", PATH)
                result = subprocess.run([sys.executable, str(SCRIPT), "--root", str(root)],
                                        text=True, capture_output=True)
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertIn(FIX, result.stderr)
                self.assertIn(PATH + ":1:", result.stderr)

    def test_current_tree_passes(self):
        self.assertEqual(GUARD.check(SCRIPT.parents[1]), [])

    def test_guard_is_wired_into_both_static_sets(self):
        root = SCRIPT.parents[1]
        self.assertIn('"store_boundary": [sys.executable, "scripts/check-store-boundary.py"]',
                      (root / "scripts/scoped-test").read_text())
        self.assertIn('"$py" scripts/check-store-boundary.py',
                      (root / "tools/git-hooks/pre-push").read_text())


if __name__ == "__main__":
    unittest.main()
