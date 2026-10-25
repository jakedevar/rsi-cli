use super::*;
use crate::testutil::{self, write};
const PATH: &str = "crates/rsid/src/rogue.rs";
fn refused(path: &str, source: &str) {
    let findings = scan(path, source).unwrap();
    assert!(!findings.is_empty(), "{path}: {source}");
    assert!(findings.iter().all(|s| s.contains(FIX)));
}
#[test]
fn ported_raw_connections_and_dynamic_keys() {
    for call in [
        "rusqlite::Connection::open(path)",
        "Connection::open_with_flags(path, flags)",
        "Connection::open_in_memory()",
        "Connection::open_with_flags_and_vfs(path, flags, vfs)",
    ] {
        refused(PATH, &format!("fn f() {{ {call}; }}"));
    }
    refused(
        PATH,
        "use rusqlite::Connection as Raw; fn f() { Raw::open(path); }",
    );
    for key in [
        "&format!(\"preamble:{}\", session)",
        "concat!(\"prefix\", session)",
        "&(PREFIX.to_owned() + &uuid.to_string())",
        "&session.to_string()",
        "&key_for(session)",
    ] {
        refused(
            PATH,
            &format!("fn f() {{ store.set_daemon_setting({key}, \"value\"); }}"),
        );
    }
    for sql in [
        "INSERT INTO daemon_settings(key,value) VALUES(?1,?2)",
        "INSERT OR REPLACE INTO daemon_settings(key,value) VALUES(?1,?2)",
        "UPDATE daemon_settings SET value=?2 WHERE key=?1",
    ] {
        refused(
            PATH,
            &format!(
                "fn f() {{ let key = format!(\"session:{{uuid}}\"); conn.execute(\"{sql}\", params![key, value]); }}"
            ),
        );
    }
    refused(
        "crates/rsi/src/rogue.rs",
        "fn f() { store.set_daemon_setting(&format!(\"{uuid}\"), \"\"); }",
    );
    refused(
        "crates/rsid/src/codegraph/new_store.rs",
        "fn f() { Connection::open(path); }",
    );
}
#[test]
fn ported_constants_accessors_and_separate_store() {
    let source = r#"fn f() {
        store.set_daemon_setting("fixed", &format!("value:{uuid}"));
        store.set_daemon_setting(PROBE_KEY, &nonce);
        store.set_daemon_setting(crate::config::KEY, "value");
        store.persist_preamble(session);
    }"#;
    assert_eq!(scan(PATH, source).unwrap(), Vec::<String>::new());
    assert!(
        scan(
            "crates/rsid-store/src/store/daemon_settings.rs",
            "fn f() { self.set_daemon_setting(&format!(\"{id}\"), \"\"); }"
        )
        .unwrap()
        .is_empty()
    );
    assert!(
        scan(
            "crates/rsid/src/memory/store.rs",
            "fn open() { Connection::open(path); }"
        )
        .unwrap()
        .is_empty()
    );
}
#[test]
fn ported_test_items_comments_strings_and_sql_fixtures() {
    let tests = r#"#[cfg(test)]
        mod tests { fn fixture() { Connection::open("rsi.db");
            store.set_daemon_setting(&format!("{uuid}"), ""); } }
        #[cfg(test)] fn helper() { Connection::open_in_memory(); }
        #[tokio::test] async fn fixture() { Connection::open("rsi.db"); }
    "#;
    assert!(scan(PATH, tests).unwrap().is_empty());
    refused(
        PATH,
        &format!("{tests}\nfn production() {{ Connection::open(\"rsi.db\"); }}"),
    );
    assert!(
        scan(
            "crates/rsid/src/session/tests.rs",
            "fn f() { Connection::open(\"rsi.db\"); }"
        )
        .unwrap()
        .is_empty()
    );
    let comments = r####"// Connection::open("rsi.db");
        /* nested /* Connection::open(path); */ comment */
        fn f() { let example = r###"Connection::open("rsi.db");"###;
            let text = "store.set_daemon_setting(&key, value)"; }
        fn live() {
            // INSERT INTO daemon_settings(key,value) VALUES(?1,?2)
            /* nested /* "UPDATE daemon_settings SET value=?1" */ comment */
            let quote = '"';
        }
        #[cfg(test)] mod tests { fn fixture() { conn.execute(r#"UPDATE daemon_settings SET value=?1"#, []); } }
    "####;
    assert!(scan(PATH, comments).unwrap().is_empty());
}
#[test]
fn ported_moved_writers_require_accessors() {
    for (path, call) in [
        (
            "session/context_cap.rs",
            "store.set_daemon_setting(&marker_key(session_id), \"value\")",
        ),
        (
            "session/context_cap.rs",
            "store.set_daemon_setting(&handoff_key(session_id), \"value\")",
        ),
        (
            "session/context_succession.rs",
            "store.set_daemon_setting(&marker_key(session_id), \"value\")",
        ),
        ("mcp_config.rs", "store.set_daemon_setting(&key, \"value\")"),
        (
            "operator_config/custom_providers.rs",
            "store.set_daemon_setting(&setting_key(row.id), \"value\")",
        ),
        (
            "operator_config/custom_providers.rs",
            "store.set_daemon_setting(&route_key(role), \"value\")",
        ),
        (
            "recursive_dag/smoke_fixture.rs",
            "Connection::open(output_db)",
        ),
        (
            "recursive_dag/smoke_fixture.rs",
            "Connection::open_with_flags(source_db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,)",
        ),
    ] {
        refused(&format!("{RSID}{path}"), &format!("fn old() {{ {call}; }}"));
    }
}
#[test]
fn ported_cli_archive_and_tracked_sources() {
    let temp = testutil::scratch_repository();
    write(
        temp.path(),
        PATH,
        "fn f() { rusqlite::Connection::open(\"rsi.db\"); }",
    );
    for tracked in [false, true] {
        if tracked {
            testutil::git(temp.path(), &["init", "-q"]);
            testutil::git(temp.path(), &["add", PATH]);
        }
        let (code, out, err) = testutil::run(
            main,
            &["--root", temp.path().to_str().unwrap()],
            &Ctx::new(temp.path(), &[]),
        );
        assert_eq!(code, 1, "{err}");
        assert!(out.is_empty());
        assert_eq!(
            err,
            format!(
                "{PATH}:1: store boundary rule: rsid must not open raw SQLite connections to rsi.db; {FIX}\n"
            )
        );
    }
}
#[test]
fn ported_current_tree_passes() {
    assert_eq!(check(&testutil::repo_root()).unwrap(), Vec::<String>::new());
}
