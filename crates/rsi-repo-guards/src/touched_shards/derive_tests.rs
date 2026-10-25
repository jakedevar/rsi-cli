//! End-to-end intents of the legacy `test_check_touched_shards.py` suite, ported
//! as behaviour over real temporary Git repositories. The legacy program is
//! never executed: expectations restate the stated selection principle.
use super::derive::{self, Failure, Loaded, Options};
use super::tests::Fixture;
use crate::rsid_test_shards::Shard;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub const GATE: &str = "#[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-{shard}\"))]\n#[test]\nfn {name}() {{}}\n";

pub fn gate(shard: &str, name: &str) -> String {
    GATE.replace("{shard}", shard)
        .replace("{name}", name)
        .replace("{{}}", "{}")
}

pub struct Checked {
    pub code: i32,
    pub summary: String,
    pub receipt: Value,
    pub stderr: Vec<String>,
}

pub struct Case {
    pub fx: Fixture,
    files: BTreeMap<String, String>,
    pub base: String,
    pub head: String,
    pub shards: Vec<Shard>,
    /// Read the shard inventory from the working-tree manifests instead of `shards`.
    pub manifests: bool,
    /// The cargo command used by compile mode (`true` unless a test fakes it).
    pub cargo: String,
    pub env: Vec<(String, String)>,
    /// The host platform the cfg gates are evaluated for.
    pub host: String,
}

pub fn default_shards() -> Vec<Shard> {
    [
        "store-01",
        "store-02",
        "store-03",
        "store-04",
        "session-01",
        "session-02",
        "session-03",
        "session-04",
        "session-05",
        "memory-01",
        "other-01",
        "other-02",
        "other-03",
        "other-04",
    ]
    .iter()
    .map(|name| Shard {
        name: (*name).into(),
        packages: vec!["rsid".into()],
    })
    .collect()
}

impl Case {
    pub fn empty() -> Self {
        Self {
            fx: Fixture::new(),
            files: BTreeMap::new(),
            base: String::new(),
            head: String::new(),
            shards: default_shards(),
            manifests: false,
            cargo: "true".into(),
            env: Vec::new(),
            host: derive::host_os().to_owned(),
        }
    }

    /// The legacy `setUp` repository: a base commit and a feature branch tip.
    pub fn new() -> Self {
        let mut case = Self {
            fx: Fixture::new(),
            files: BTreeMap::new(),
            base: String::new(),
            head: String::new(),
            shards: default_shards(),
            manifests: false,
            cargo: "true".into(),
            env: Vec::new(),
            host: derive::host_os().to_owned(),
        };
        case.write("crates/rsid/src/store/migrations/v150.rs", "// v150\n");
        case.write("crates/rsid/src/foo.rs", &gate("store-02", "a"));
        case.write("crates/rsid/src/bar.rs", "pub fn bar() {}\n");
        case.write("crates/rsid/src/bar/tests.rs", &gate("other-03", "b"));
        case.write(
            "crates/rsi-common/src/types.rs",
            "pub fn t() {}\n#[cfg(test)]\nmod tests { #[test] fn a() {} }\n",
        );
        case.write("crates/rsi-common/src/untested.rs", "pub fn u() {}\n");
        case.write(
            "crates/rsid/src/bin/tool.rs",
            "fn main() {}\n#[test]\nfn t() {}\n",
        );
        case.write("crates/rsid/tests/it.rs", "#[test]\nfn t() {}\n");
        case.write(
            "scripts/desktop-test-gate.py",
            "# the desktop lane's selector (stub)\n",
        );
        case.commit();
        case.base = case.head.clone();
        case
    }

    pub fn write(&mut self, path: &str, text: &str) {
        self.files.insert(path.into(), text.into());
    }
    pub fn remove(&mut self, path: &str) {
        self.files.remove(path);
    }
    pub fn read(&self, path: &str) -> String {
        self.files.get(path).cloned().unwrap_or_default()
    }
    fn listing(&self) -> Vec<(&str, &str)> {
        self.files
            .iter()
            .map(|(p, t)| (p.as_str(), t.as_str()))
            .collect()
    }
    /// Commit the current files on top of the current tip.
    pub fn commit(&mut self) -> String {
        let parents: Vec<&str> = if self.head.is_empty() {
            vec![]
        } else {
            vec![self.head.as_str()]
        };
        let sha = self.fx.commit(&self.listing(), &parents);
        self.head = sha.clone();
        sha
    }
    /// A commit with explicit parents and files, leaving `files` as the new tree.
    pub fn commit_on(&mut self, parents: &[&str]) -> String {
        let sha = self.fx.commit(&self.listing(), parents);
        self.head = sha.clone();
        sha
    }
    /// `git branch -f base HEAD`.
    pub fn rebase_base_here(&mut self) {
        self.base = self.head.clone();
    }

    pub fn load(&self, base: &str, head: &str) -> Result<Loaded, Failure> {
        Loaded::load_on(
            self.fx.path(),
            base,
            head,
            (!self.manifests).then(|| self.shards.clone()),
            &self.host,
        )
    }

    pub fn check_args(&self, args: &[&str]) -> Checked {
        self.check_between(&self.base.clone(), &self.head.clone(), args)
    }

    pub fn check_between(&self, base: &str, head: &str, args: &[&str]) -> Checked {
        let moded = ["--compile", "--no-compile", "--filters-only"]
            .iter()
            .any(|m| args.contains(m));
        let options = Options {
            filters_only: args.contains(&"--filters-only"),
            run_filters: args.contains(&"--run-filters"),
            compile: args.contains(&"--compile") || !moded,
            no_compile: args.contains(&"--no-compile") || args.contains(&"--filters-only"),
            env: Some(
                self.env
                    .clone()
                    .into_iter()
                    .chain([("CHECK_TOUCHED_SHARDS_CARGO".to_owned(), self.cargo.clone())])
                    .collect(),
            ),
            ..Options::default()
        };
        let loaded = match self.load(base, head) {
            Ok(loaded) => loaded,
            Err(failure) => return failed(failure),
        };
        match derive::derive(&loaded, &options) {
            Ok(derived) => Checked {
                code: i32::from(derived.failed),
                summary: derived.summary,
                receipt: derived.receipt,
                stderr: derived.stderr,
            },
            Err(failure) => failed(failure),
        }
    }

    /// `check("--filters-only")` of the legacy helper.
    pub fn check(&self, args: &[&str]) -> Checked {
        self.check_args(args)
    }
}

fn failed(failure: Failure) -> Checked {
    let (code, message) = match failure {
        Failure::Refusal(m) => (2, m),
        Failure::Fatal(m) => (1, m),
        Failure::Interrupted { code, message, .. } => (code, message),
    };
    Checked {
        code,
        summary: message,
        receipt: Value::Null,
        stderr: Vec::new(),
    }
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn shard_named_only_in_a_fixture_string_is_not_compiled() {
    let mut case = Case::new();
    let text = gate("store-02", "real")
        + "#[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-store-02\"))]\n#[test]\nfn fixture() {\n    let gate = \"feature = \\\"test-shard-other-03\\\"\";\n}\n";
    case.write("crates/rsid/src/foo.rs", &text);
    case.commit();
    let r = case.check(&["--no-compile"]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(
        r.receipt["shards"]["map"],
        json!({"store-02": ["crates/rsid/src/foo.rs"]})
    );
}

#[test]
fn a_desktop_only_diff_selects_the_desktop_lane_and_no_cargo_filter() {
    let mut case = Case::new();
    case.write("desktop/src/model.ts", "export const a = 1;\n");
    case.write("desktop/src-tauri/src/lib.rs", "pub fn a() {}\n");
    case.commit();
    let r = case.check(&["--filters-only"]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(
        r.receipt["desktop"],
        json!({"selected": true, "total": 2, "paths": ["desktop/src-tauri/src/lib.rs", "desktop/src/model.ts"]})
    );
    assert_eq!(
        (&r.receipt["suggested_filters"], &r.receipt["shards"]["map"]),
        (&json!([]), &json!({}))
    );
}

#[test]
fn a_diff_outside_desktop_does_not_select_the_desktop_lane() {
    let mut case = Case::new();
    case.write(
        "crates/rsi-common/src/types.rs",
        "pub fn t() { let _x = 1; }\n#[cfg(test)]\nmod tests { #[test] fn a() {} }\n",
    );
    case.write("docs/desktop/notes.md", "# notes\n");
    case.commit();
    let r = case.check(&["--filters-only"]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(
        r.receipt["desktop"],
        json!({"selected": false, "total": 0, "paths": []})
    );
    assert_eq!(r.receipt["suggested_filters"], json!(["rsi-common=types"]));
}

#[test]
fn a_mixed_diff_keeps_its_cargo_filters_and_adds_the_desktop_lane() {
    let mut case = Case::new();
    case.write(
        "crates/rsi-common/src/types.rs",
        "pub fn t() { let _x = 1; }\n#[cfg(test)]\nmod tests { #[test] fn a() {} }\n",
    );
    case.write("desktop/package.json", "{}\n");
    case.commit();
    let r = case.check(&["--filters-only"]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(
        (
            &r.receipt["desktop"]["selected"],
            &r.receipt["desktop"]["paths"]
        ),
        (&json!(true), &json!(["desktop/package.json"]))
    );
    assert_eq!(r.receipt["suggested_filters"], json!(["rsi-common=types"]));
}

#[test]
fn a_keybindings_or_tui_source_or_path_dependency_diff_selects_the_desktop_lane() {
    let mut case = Case::new();
    case.write(
        "desktop/src-tauri/Cargo.toml",
        "[package]\nname = \"d\"\n[dependencies]\nrsi-graph = { path = \"../../crates/rsi-graph\" }\n",
    );
    case.write("crates/rsi-graph/src/lib.rs", "pub fn g() {}\n");
    case.write("crates/rsi/src/keybindings.rs", "// keys\n");
    case.write("docs/keybindings.md", "# keys\n");
    case.commit();
    for (name, text) in [
        ("docs/keybindings.md", "# keys changed\n"),
        ("crates/rsi/src/keybindings.rs", "// keys changed\n"),
        (
            "crates/rsi-graph/src/lib.rs",
            "pub fn g() { let _x = 1; }\n",
        ),
    ] {
        case.rebase_base_here();
        case.write(name, text);
        case.commit();
        let r = case.check(&["--filters-only"]);
        assert_eq!(r.code, 0, "{}", r.summary);
        assert_eq!(
            r.receipt["desktop"],
            json!({"selected": true, "total": 1, "paths": [name]}),
            "{name}"
        );
    }
    case.rebase_base_here();
    case.write("crates/rsid/src/bar.rs", "pub fn bar() { let _x = 1; }\n");
    case.commit();
    let r = case.check(&["--filters-only"]);
    assert_eq!(
        (r.code, &r.receipt["desktop"]["selected"]),
        (0, &json!(false)),
        "{}",
        r.summary
    );
}

#[test]
fn a_candidate_without_the_desktop_selector_fails_closed_only_when_its_diff_needs_it() {
    let mut case = Case::new();
    case.remove("scripts/desktop-test-gate.py");
    case.commit();
    let r = case.check(&["--filters-only"]);
    assert_eq!(r.code, 1, "{}", r.summary);
    assert_eq!(r.receipt["desktop"]["selected"], json!(true));
    assert!(
        r.receipt["desktop"]["error"]
            .as_str()
            .unwrap()
            .contains("scripts/desktop-test-gate.py is missing from the candidate")
    );
    case.rebase_base_here();
    case.write("docs/notes.md", "# notes\n");
    case.commit();
    let r = case.check(&["--filters-only"]);
    assert_eq!(
        (r.code, &r.receipt["desktop"]),
        (0, &json!({"selected": false, "total": 0, "paths": []})),
        "{}",
        r.summary
    );
    case.write("desktop/src/model.ts", "export const a = 1;\n");
    case.commit();
    let r = case.check(&["--filters-only"]);
    assert_eq!(r.code, 1, "{}", r.summary);
    assert!(r.receipt["desktop"].get("error").is_some());
}

/// rolling and the worker branch each merged the other's earlier commit (#1633).
fn criss_cross(case: &mut Case) -> (String, String) {
    case.write("crates/rsid/src/beta.rs", &gate("store-02", "beta"));
    let worker_first = case.commit();
    let base_files = case.files.clone();
    // rolling: its own first commit on the base, then a merge of the worker's first.
    case.files.remove("crates/rsid/src/beta.rs");
    case.write("crates/rsid/src/alpha.rs", &gate("other-03", "alpha"));
    let rolling_first = case.commit_on(&[&case.base.clone()]);
    case.write("crates/rsid/src/beta.rs", &gate("store-02", "beta"));
    let rolling_merged = case.commit_on(&[&rolling_first, &worker_first]);
    // worker: takes rolling's first commit.
    case.files = base_files;
    case.write("crates/rsid/src/alpha.rs", &gate("other-03", "alpha"));
    let worker_merged = case.commit_on(&[&worker_first, &rolling_first]);
    case.base = rolling_merged;
    case.head = worker_merged;
    (worker_first, rolling_first)
}

#[test]
fn several_merge_bases_select_only_the_real_candidate_diff() {
    let mut case = Case::new();
    criss_cross(&mut case);
    case.write(
        "crates/rsi-common/src/types.rs",
        "pub fn t() { let _x = 1; }\n#[cfg(test)]\nmod tests { #[test] fn a() {} }\n",
    );
    case.commit();
    let bases = case
        .fx
        .git(&["merge-base", "--all", &case.base, &case.head]);
    assert_eq!(bases.split_whitespace().count(), 2, "{bases}");
    let r = case.check(&["--no-compile"]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(r.receipt["changed_files"], json!(1));
    assert_eq!(r.receipt["shards"]["map"], json!({}));
    assert_eq!(r.receipt["suggested_filters"], json!(["rsi-common=types"]));
    let mut found: Vec<String> = strings(&r.receipt["merge_bases"]);
    found.sort();
    let mut expected: Vec<String> = bases.split_whitespace().map(str::to_owned).collect();
    expected.sort();
    assert_eq!(found, expected);
    assert_eq!(r.receipt["merge_base"], r.receipt["base"]);
}

#[test]
fn one_merge_base_keeps_the_merge_base_diff() {
    let mut case = Case::new();
    case.write(
        "crates/rsi-common/src/types.rs",
        "pub fn t() { let _x = 1; }\n#[cfg(test)]\nmod tests { #[test] fn a() {} }\n",
    );
    case.commit();
    let r = case.check(&["--no-compile"]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(r.receipt["merge_bases"].as_array().unwrap().len(), 1);
    assert_eq!(r.receipt["merge_base"], r.receipt["merge_bases"][0]);
}

// ---------------------------------------------------------------- migrations

const REWIND_PATH: &str = "crates/rsid-store/src/store/tests.rs";

fn with_body(gated: &str, body: &str) -> String {
    gated.replacen("{}", &format!("{{\n    {body}\n}}"), 1)
}

/// The legacy `rewind_fixture`: helper functions, callers in two shards and a
/// cross-module caller that must retain its own shard.
fn rewind_fixture(case: &mut Case) -> String {
    let helpers = "const REWIND_TEARDOWN_COVERED_THROUGH: i32 = 156;\nfn rewind_post_v121_tail_to() {\n    old_tail();\n}\nfn assert_fixture_matches_claimed_version() {\n    old_catalog();\n}\nfn assert_post_v77_chain_replayed() {\n    old_catalog();\n}\nfn indirect_fixture() {\n    rewind_store_to_schema_version();\n}\n";
    let mut tests = String::new();
    for (name, call) in [
        ("migration_old", "rewind_store_to_schema_version"),
        ("migration_tail", "rewind_post_v121_tail_to"),
        ("migration_replay", "assert_post_v77_chain_replayed"),
        ("migration_indirect", "indirect_fixture"),
        ("schema_rewinds_correctly", "unrelated"),
        ("fixture_guard_rejects_drift", "unrelated"),
        ("ordinary_store_test", "unrelated"),
    ] {
        tests += &with_body(&gate("store-01", name), &format!("{call}();"));
    }
    let original = format!("{helpers}{tests}");
    case.write(REWIND_PATH, &original);
    case.write(
        "crates/rsid-store/src/store/other.rs",
        &with_body(
            &gate("store-02", "external_migration"),
            "rewind_post_v121_tail_to();",
        ),
    );
    case.commit();
    case.rebase_base_here();
    original
}

fn filter_set(r: &Checked) -> std::collections::BTreeSet<String> {
    strings(&r.receipt["suggested_filters"])
        .into_iter()
        .collect()
}
fn set_of(items: &[&str]) -> std::collections::BTreeSet<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

#[test]
fn rewind_helper_only_diff_selects_callers_and_fixture_guards() {
    let mut case = Case::new();
    let original = rewind_fixture(&mut case);
    case.write(
        REWIND_PATH,
        &original
            .replace("156;", "157;")
            .replace("old_tail();", "new_tail();")
            .replace("old_catalog();", "new_catalog();"),
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    let mut expected: std::collections::BTreeSet<String> = [
        "migration_old",
        "migration_tail",
        "migration_replay",
        "migration_indirect",
        "schema_rewinds_correctly",
        "fixture_guard_rejects_drift",
    ]
    .iter()
    .map(|n| format!("rsid=shard:store-01:test({n})"))
    .collect();
    expected.insert("rsid=shard:store-02:test(external_migration)".into());
    assert_eq!(filter_set(&r), expected);
}

fn additive_rewind_change(case: &mut Case, migration: &str) -> Checked {
    let original = rewind_fixture(case);
    let mut pins = String::new();
    for (shard, name) in [
        (
            "store-01",
            "every_recovered_migration_step_actually_executes",
        ),
        ("store-01", "queue_fixture_rewinds_v150"),
        ("store-02", "previous_schema_upgrades_to_the_live_indexes"),
    ] {
        pins += &with_body(&gate(shard, name), "rewind_post_v121_tail_to();");
    }
    case.write(REWIND_PATH, &format!("{original}{pins}"));
    case.commit();
    case.rebase_base_here();
    case.write("crates/rsid-store/src/store/migrations/v151.rs", migration);
    case.write(
        REWIND_PATH,
        &format!("{original}{pins}")
            .replace("156;", "157;")
            .replace("old_tail();", "new_tail();"),
    );
    case.commit();
    case.check(&[])
}

const CHAIN_PINS: [&str; 3] = [
    "rsid=shard:store-01:test(every_recovered_migration_step_actually_executes)",
    "rsid=shard:store-01:test(queue_fixture_rewinds_v150)",
    "rsid=shard:store-02:test(previous_schema_upgrades_to_the_live_indexes)",
];

#[test]
fn additive_migration_selects_only_the_chain_pins() {
    let mut case = Case::new();
    let r = additive_rewind_change(
        &mut case,
        "impl Store {\n    fn migrate_v151(&self) {\n        add_column_if_not_exists_tx(&tx, \"t\", \"c\", \"TEXT\");\n        // DROP is only a comment here\n    }\n}\n",
    );
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(filter_set(&r), set_of(&CHAIN_PINS));
}

#[test]
fn a_slice_numbered_after_a_declared_pending_sibling_is_not_a_conflict() {
    let mut case = Case::new();
    case.write(
        "crates/rsid-store/src/store/migrations/v152.rs",
        "// v152\n",
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(
        r.receipt["migrations"]["numbering_conflict"],
        json!(true),
        "{}",
        r.summary
    );
    assert!(r.summary.contains(" RENUMBER"));
    case.write(
        "tools/provisional-migrations/issue-1285-gap.json",
        &json!({"schema_version": 1, "version": 152, "files": [], "pending_predecessors": [151]})
            .to_string(),
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(
        r.receipt["migrations"]["numbering_conflict"],
        json!(false),
        "{}",
        r.summary
    );
    assert_eq!(
        r.receipt["migrations"]["pending_predecessors"],
        json!([151])
    );
    assert!(!r.summary.contains(" RENUMBER"));
}

#[test]
fn a_declared_gap_does_not_excuse_a_wrong_first_number() {
    let mut case = Case::new();
    case.write(
        "crates/rsid-store/src/store/migrations/v153.rs",
        "// v153\n",
    );
    case.write(
        "tools/provisional-migrations/issue-1285-gap.json",
        &json!({"schema_version": 1, "version": 153, "files": [], "pending_predecessors": [151]})
            .to_string(),
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(
        r.receipt["migrations"]["numbering_conflict"],
        json!(true),
        "{}",
        r.summary
    );
}

#[test]
fn a_migration_only_diff_selects_the_catalog_replay_pins() {
    let mut case = Case::new();
    rewind_fixture(&mut case);
    let replay = with_body(
        &gate("store-03", "migration_replays_from_previous_head"),
        "rewind_post_v121_tail_to();\n    let _ = \"SELECT sql FROM sqlite_master\";",
    );
    case.write("crates/rsid-store/src/store/custody/tests.rs", &replay);
    case.commit();
    case.rebase_base_here();
    case.write(
        "crates/rsid-store/src/store/migrations/v151.rs",
        "impl Store {\n    fn migrate_v151(&self) {\n        add_column_if_not_exists_tx(&tx, \"t\", \"c\", \"TEXT\");\n    }\n}\n",
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    let filters = filter_set(&r);
    assert!(
        filters.contains("rsid=shard:store-03:test(migration_replays_from_previous_head)"),
        "{filters:?}"
    );
    assert!(!filters.contains("rsid=shard:store-01:test(migration_old)"));
}

#[test]
fn data_rewriting_migration_keeps_every_rewind_caller() {
    let mut case = Case::new();
    let r = additive_rewind_change(
        &mut case,
        "impl Store {\n    fn migrate_v151(&self) {\n        add_column_if_not_exists_tx(&tx, \"t\", \"c\", \"TEXT\");\n        tx.execute(\"UPDATE t SET c = 'x'\", []);\n    }\n}\n",
    );
    assert_eq!(r.code, 0, "{}", r.summary);
    let filters = filter_set(&r);
    assert!(filters.contains("rsid=shard:store-01:test(migration_old)"));
    assert!(filters.contains("rsid=shard:store-02:test(external_migration)"));
}

#[test]
fn isolated_new_table_selects_only_chain_pins() {
    let mut case = Case::new();
    let r = additive_rewind_change(
        &mut case,
        "impl Store {\n    fn migrate_v151(&self) {\n        tx.execute_batch(\"CREATE TABLE turns(id TEXT PRIMARY KEY, state TEXT); CREATE INDEX turns_state ON turns(state); CREATE TRIGGER turns_guard BEFORE UPDATE ON turns WHEN NEW.state='bad' BEGIN SELECT RAISE(ABORT,'bad state'); END;\");\n        tx.pragma_update(None, \"user_version\", 151);\n    }\n}\n",
    );
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(filter_set(&r), set_of(&CHAIN_PINS));
}

#[test]
fn imported_journal_migration_with_foreign_keys_keeps_only_chain_pins() {
    let migration = "//! Journal\nuse super::super::Store;\nuse crate::error::Result;\nuse rusqlite::{Transaction, TransactionBehavior};\nimpl Store {\n    pub(super) fn migrate_v151(&self, version: i32) -> Result<()> {\n        if version < 151 {\n            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;\n            tx.execute_batch(\"CREATE TABLE turns(id TEXT PRIMARY KEY, session_id TEXT REFERENCES sessions(id) ON DELETE RESTRICT, link TEXT DEFAULT 'https://example.invalid/journal'); CREATE INDEX turns_session ON turns(session_id); CREATE TRIGGER turns_identity BEFORE UPDATE ON turns WHEN NEW.id IS NOT OLD.id BEGIN SELECT RAISE(ABORT,'immutable'); END;\");\n            tx.pragma_update(None, \"user_version\", 151)?;\n            tx.commit()?;\n        }\n        Ok(())\n    }\n}\n";
    let mut case = Case::new();
    let r = additive_rewind_change(&mut case, migration);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(filter_set(&r), set_of(&CHAIN_PINS));
    assert!(
        r.receipt["narrowed_filters"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("proven additive/isolated")
    );
    assert!(super::migration::additive_migration(migration));
    assert!(!super::migration::additive_migration(
        &migration.replace("use rusqlite::", "use custom::")
    ));
    assert!(!super::migration::additive_migration(&migration.replace(
        "tx.commit()?;",
        "mutate_existing(&tx);\n            tx.commit()?;"
    )));
    assert!(!super::migration::additive_migration(&migration.replace(
        "CREATE INDEX turns_session ON turns",
        "CREATE INDEX turns_session ON sessions"
    )));
}

#[test]
fn new_table_proof_rejects_existing_targets_data_writes_and_unknown_helpers() {
    let classify = super::migration::additive_migration;
    let base = |sql: &str, rust: &str| {
        format!("impl Store {{ fn migrate_v151(&self) {{ tx.execute_batch(\"{sql}\"); {rust} }} }}")
    };
    let new = "CREATE TABLE turns(id TEXT PRIMARY KEY, state TEXT);";
    assert!(classify(&base(new, "")));
    for (extra, rust) in [
        ("CREATE INDEX old_idx ON sessions(id);", ""),
        (
            "CREATE TRIGGER old_guard BEFORE UPDATE ON sessions BEGIN SELECT RAISE(ABORT,'bad'); END;",
            "",
        ),
        (
            "CREATE TRIGGER writes_old AFTER INSERT ON turns BEGIN UPDATE sessions SET state='x'; END;",
            "",
        ),
        ("INSERT INTO turns VALUES('a','live');", ""),
        ("DROP TABLE turns;", ""),
        ("CREATE TABLE copied AS SELECT * FROM turns;", ""),
        ("PRAGMA user_version=151;", ""),
        ("", "mutate_existing(&tx);"),
        ("", "(mutate_existing)(&tx);"),
        ("", "mutate_existing!(&tx);"),
        ("", "tx.execute(\"UPDATE sessions SET state=1\", []);"),
        ("", "tx.pragma_update(None, \"foreign_keys\", 0);"),
    ] {
        assert!(
            !classify(&base(&format!("{new}{extra}"), rust)),
            "{extra} {rust}"
        );
    }
    assert!(!classify(
        "fn migrate_v151() { tx.execute_batch(SQL_FROM_HELPER); }"
    ));
    assert!(!classify(&base(
        "CREATE TABLE IF NOT EXISTS turns(id TEXT);",
        ""
    )));
    assert!(!classify(&base(
        "CREATE TABLE /* existing */ IF NOT EXISTS turns(id TEXT);",
        ""
    )));
    assert!(classify(&base(
        "CREATE TABLE turns(id TEXT CHECK(rsi_uuid_is_canonical(id)), created_at TEXT CHECK(rsi_rfc3339_nanos_is_canonical(created_at)));",
        ""
    )));
}

#[test]
fn another_hunk_keeps_the_full_store_test_module() {
    let mut case = Case::new();
    let original = rewind_fixture(&mut case);
    case.write(
        REWIND_PATH,
        &original
            .replace("156;", "157;")
            .replace("unrelated();", "changed();"),
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(
        r.receipt["suggested_filters"],
        json!(["rsid=shard:store-01:test(store::tests)"])
    );
}

#[test]
fn deleting_non_fixture_code_keeps_the_full_module() {
    let mut case = Case::new();
    let original = rewind_fixture(&mut case);
    case.write(
        REWIND_PATH,
        &original
            .replace("156;", "157;")
            .replace("    unrelated();\n", ""),
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(
        r.receipt["suggested_filters"],
        json!(["rsid=shard:store-01:test(store::tests)"])
    );
}

// ------------------------------------------------- declared coverage and targets

const STORE_MOD: &str = "pub struct Store;\nimpl Store {\n    pub fn open() -> u8 {\n        1\n    }\n    pub fn other() -> u8 {\n        2\n    }\n}\nmod tests;\n";
const EXACT_TEST: &str = "h1_v83_startup_terminal_history_is_idempotent_across_file_backed_reopen";

fn repo_script(name: &str) -> String {
    std::fs::read_to_string(
        std::path::Path::new(rsi_common::manifest_dir!())
            .join("../../scripts")
            .join(name),
    )
    .unwrap()
}

/// A store module whose open lifecycle hooks the repo's real declaration covers (#1585).
fn store_open_fixture(case: &mut Case, exact: bool) {
    case.write(
        "scripts/test-coverage.json",
        &repo_script("test-coverage.json"),
    );
    case.write("crates/rsid-store/src/lib.rs", "pub mod store;\n");
    case.write("crates/rsid-store/src/store/mod.rs", STORE_MOD);
    let mut list = vec![
        ("store-01", "store_open_refuses_newer_schema"),
        ("store-02", "store_open_accepts_latest"),
        ("store-01", "init_schema_runs_the_full_chain"),
        ("store-01", "unrelated_store_test"),
        ("store-02", "another_unrelated_test"),
    ];
    if exact {
        list.push(("store-01", EXACT_TEST));
    }
    let tests: String = list.iter().map(|(s, n)| gate(s, n)).collect();
    case.write("crates/rsid-store/src/store/tests.rs", &tests);
    case.commit();
    case.rebase_base_here();
}

fn edit_store_mod(case: &mut Case, replacements: &[(&str, &str)]) {
    let mut text = STORE_MOD.to_owned();
    for (old, new) in replacements {
        assert!(text.contains(old));
        text = text.replacen(old, new, 1);
    }
    case.write("crates/rsid-store/src/store/mod.rs", &text);
    case.commit();
}

#[test]
fn a_store_open_hook_change_selects_the_declared_open_tests_not_the_store_shards() {
    let mut case = Case::new();
    store_open_fixture(&mut case, true);
    edit_store_mod(
        &mut case,
        &[(
            "        1\n",
            "        // backup before migrating\n        helper(1)\n",
        )],
    );
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(
        r.receipt["declared_coverage"][0]["path"],
        json!("crates/rsid-store/src/store/mod.rs")
    );
    let mut by_shard = BTreeMap::new();
    for f in strings(&r.receipt["suggested_filters"]) {
        by_shard.insert(f.split(':').nth(1).unwrap().to_owned(), f);
    }
    assert_eq!(
        by_shard.keys().cloned().collect::<Vec<_>>(),
        ["store-01", "store-02"]
    );
    for name in [
        "store_open_refuses_newer_schema",
        "init_schema_runs_the_full_chain",
        EXACT_TEST,
    ] {
        assert!(by_shard["store-01"].contains(name), "{name}");
    }
    assert!(by_shard["store-02"].contains("store_open_accepts_latest"));
    for flt in by_shard.values() {
        assert!(!flt.contains("unrelated"));
        assert!(
            flt.starts_with("rsid=shard:store-0") && flt.contains("test(/^store::tests::"),
            "{flt}"
        );
    }
    assert!(strings(&r.receipt["shards"]["compiled_ok"]).contains(&"store-01".to_owned()));
}

#[test]
fn a_new_pre_migration_helper_and_its_call_stay_declared() {
    let mut case = Case::new();
    store_open_fixture(&mut case, true);
    edit_store_mod(
        &mut case,
        &[
            (
                "        1\n",
                "        pre_migration_backup();\n        1\n",
            ),
            (
                "    pub fn other",
                "    /// Copy the file before any migration runs.\n    fn pre_migration_backup() {\n        // copy\n    }\n\n    pub fn other",
            ),
        ],
    );
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(r.receipt["declared_coverage"].as_array().unwrap().len(), 1);
    assert!(
        strings(&r.receipt["suggested_filters"])
            .iter()
            .all(|f| f.contains("test(/^store::tests::"))
    );
}

#[test]
fn a_change_outside_the_declared_functions_derives_the_module_filters() {
    for replacements in [
        vec![("        2\n", "        3\n")],
        vec![
            ("        1\n", "        4\n"),
            ("        2\n", "        3\n"),
        ],
        vec![("pub struct Store;", "pub struct Store { field: u8 }")],
    ] {
        let mut case = Case::new();
        store_open_fixture(&mut case, true);
        edit_store_mod(&mut case, &replacements);
        let r = case.check(&[]);
        assert_eq!(r.code, 0, "{}", r.summary);
        assert_eq!(
            r.receipt["declared_coverage"],
            json!([]),
            "{replacements:?}"
        );
        assert!(
            strings(&r.receipt["suggested_filters"])
                .contains(&"rsid=shard:store-01:test(store)".to_owned()),
            "{replacements:?} {:?}",
            r.receipt["suggested_filters"]
        );
    }
}

#[test]
fn an_unresolvable_declaration_falls_back_to_the_module_filters() {
    let mut case = Case::new();
    store_open_fixture(&mut case, false);
    edit_store_mod(&mut case, &[("        1\n", "        4\n")]);
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(r.receipt["declared_coverage"], json!([]));
    assert!(
        strings(&r.receipt["suggested_filters"])
            .contains(&"rsid=shard:store-01:test(store)".to_owned())
    );
}

#[test]
fn a_rule_with_several_test_modules_selects_each_modules_named_tests() {
    let mut case = Case::new();
    case.write(
        "scripts/test-coverage.json",
        &json!({"version": 1, "rules": [{
            "path": "crates/rsid/src/session/mod.rs", "reason": "planner",
            "functions": ["plan_fire"],
            "tests": [{"module": "session::tests_a", "prefixes": ["watch_"]},
                      {"module": "session::tests_b", "names": ["notice_stays_durable"]}]}]})
        .to_string(),
    );
    case.write("crates/rsid/src/lib.rs", "pub mod session;\n");
    let module = "mod tests_a;\nmod tests_b;\npub fn plan_fire() -> u8 {\n    1\n}\npub fn other() -> u8 {\n    2\n}\n";
    case.write("crates/rsid/src/session/mod.rs", module);
    case.write(
        "crates/rsid/src/session/tests_a.rs",
        &(gate("session-01", "watch_fires") + &gate("session-01", "unrelated_a")),
    );
    case.write(
        "crates/rsid/src/session/tests_b.rs",
        &(gate("session-02", "notice_stays_durable") + &gate("session-02", "unrelated_b")),
    );
    case.commit();
    case.rebase_base_here();
    case.write(
        "crates/rsid/src/session/mod.rs",
        &module.replacen("    1\n", "    helper(1)\n", 1),
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(r.receipt["declared_coverage"].as_array().unwrap().len(), 1);
    let filters = strings(&r.receipt["suggested_filters"]);
    let shards: std::collections::BTreeSet<_> = filters
        .iter()
        .map(|f| f.split(':').nth(1).unwrap().to_owned())
        .collect();
    assert_eq!(shards, set_of(&["session-01", "session-02"]));
    assert!(
        filters.iter().any(|f| f.contains("watch_fires")),
        "{filters:?}"
    );
    assert!(
        filters.iter().any(|f| f.contains("notice_stays_durable")),
        "{filters:?}"
    );
    assert!(
        !filters.iter().any(|f| f.contains("unrelated")),
        "{filters:?}"
    );
}

#[test]
fn maps_diff_to_shards_and_emits_one_line_and_receipt() {
    let mut case = Case::new();
    case.write(
        "crates/rsid/src/foo.rs",
        &(gate("store-02", "a") + "const EDIT: u8 = 1;\n"),
    );
    case.write("crates/rsid/src/bar.rs", "pub fn bar() { }\n");
    case.write(
        "crates/rsi-common/src/types.rs",
        "pub fn t() { }\n#[test]\nfn a() {}\n",
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert!(
        r.summary.starts_with("check-touched-shards OK"),
        "{}",
        r.summary
    );
    assert_eq!(
        r.receipt["shards"]["compiled_ok"],
        json!(["other-03", "store-02"])
    );
    assert_eq!(r.receipt["merge_clean"], json!(true));
    assert_eq!(r.receipt["ok"], json!(true));
    assert_eq!(
        r.receipt["suggested_filters"],
        json!([
            "rsi-common=types",
            "rsid=shard:other-03:test(bar)",
            "rsid=shard:store-02:test(foo)"
        ])
    );
    assert_eq!(r.receipt["audits"]["diff_check"]["verdict"], json!("ok"));
}

#[test]
fn filters_only_name_targets_that_hold_a_test() {
    let mut case = Case::new();
    case.write("crates/rsi-common/src/untested.rs", "pub fn u() { }\n");
    case.write(
        "crates/rsid/src/bin/tool.rs",
        "fn main() { }\n#[test]\nfn t() {}\n",
    );
    case.write("crates/rsid/tests/it.rs", "#[test]\nfn t() { }\n");
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(r.receipt["shards"]["compiled_ok"], json!([]));
    assert_eq!(
        r.receipt["suggested_filters"],
        json!(["rsid=bin:tool", "rsid=test:it"])
    );
}

// ------------------------------------------------------------------ e2e surfaces

const E2E_MODULES: [&str; 9] = [
    "agent_projects",
    "editing_mode",
    "fresh_install",
    "manager_decisions",
    "manager_presets",
    "manager_seats",
    "manager_surface",
    "operator_todos",
    "topology_run",
];

fn e2e_fixture(case: &mut Case, modules: &[&str]) {
    case.write(
        "scripts/e2e-tui-surfaces.json",
        &repo_script("e2e-tui-surfaces.json"),
    );
    let mods: String = modules
        .iter()
        .map(|m| format!("#[path = \"e2e_tui/{m}.rs\"]\nmod {m};\n"))
        .collect();
    case.write("crates/rsi/tests/e2e_tui.rs", &mods);
    for module in modules {
        case.write(
            &format!("crates/rsi/tests/e2e_tui/{module}.rs"),
            "#[tokio::test]\nasync fn test_e2e_x() {}\n",
        );
    }
    case.commit();
    case.rebase_base_here();
}

fn e2e_filters(r: &Checked) -> Vec<String> {
    strings(&r.receipt["suggested_filters"])
        .into_iter()
        .filter(|f| f.starts_with("rsi=test:e2e_tui"))
        .collect()
}

#[test]
fn a_settings_change_selects_the_editing_mode_e2e_module_only() {
    let mut case = Case::new();
    e2e_fixture(&mut case, &E2E_MODULES);
    case.write("crates/rsi/src/settings_staging.rs", "pub fn stage() {}\n");
    case.write("crates/rsi/src/ui/settings.rs", "pub fn draw() {}\n");
    case.commit();
    let r = case.check(&["--no-compile"]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(e2e_filters(&r), ["rsi=test:e2e_tui:test(editing_mode::)"]);
    let modules: Vec<_> = r.receipt["e2e_surfaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["module"].clone())
        .collect();
    assert_eq!(modules, [json!("editing_mode")]);
}

#[test]
fn a_global_manager_workspace_change_selects_manager_seats_and_fresh_install() {
    let mut case = Case::new();
    e2e_fixture(&mut case, &E2E_MODULES);
    case.write(
        "crates/rsi/src/overlay/global_manager_workspace.rs",
        "pub fn open() {}\n",
    );
    case.commit();
    let r = case.check(&["--no-compile"]);
    assert_eq!(r.code, 0, "{}", r.summary);
    let filters = e2e_filters(&r);
    for module in ["manager_seats", "fresh_install", "manager_surface"] {
        assert!(
            filters.contains(&format!("rsi=test:e2e_tui:test({module}::)")),
            "{module} {filters:?}"
        );
    }
    assert!(!filters.contains(&"rsi=test:e2e_tui:test(editing_mode::)".to_owned()));
    assert!(!filters.contains(&"rsi=test:e2e_tui".to_owned()));
}

#[test]
fn an_unmapped_tui_file_and_a_comment_only_edit_select_no_e2e_module() {
    let mut case = Case::new();
    e2e_fixture(&mut case, &E2E_MODULES);
    case.write("crates/rsi/src/clipboard.rs", "pub fn copy() {}\n");
    case.write("crates/rsi/src/todo_panel.rs", "// panel\npub fn t() {}\n");
    case.commit();
    case.rebase_base_here();
    case.write("crates/rsi/src/clipboard.rs", "pub fn copy() { }\n");
    case.write(
        "crates/rsi/src/todo_panel.rs",
        "// panel, reworded\npub fn t() {}\n",
    );
    case.commit();
    let r = case.check(&["--no-compile"]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert!(e2e_filters(&r).is_empty());
}

#[test]
fn a_mapped_module_the_e2e_root_does_not_mount_is_dropped_and_reported() {
    let mut case = Case::new();
    let modules: Vec<&str> = E2E_MODULES
        .iter()
        .copied()
        .filter(|m| *m != "operator_todos")
        .collect();
    e2e_fixture(&mut case, &modules);
    case.write("crates/rsi/src/todo_panel.rs", "pub fn t() {}\n");
    case.commit();
    let r = case.check(&["--no-compile"]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert!(e2e_filters(&r).is_empty());
    let dropped: Vec<_> = r.receipt["dropped_filters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["filter"].clone())
        .collect();
    assert_eq!(dropped, [json!("rsi=test:e2e_tui:test(operator_todos::)")]);
}

#[test]
fn every_mounted_e2e_tui_module_is_mapped_or_explicitly_unmapped() {
    // #2339: a new e2e_tui module must be added to scripts/e2e-tui-surfaces.json
    // (or listed in "unmapped_ok") so a change to its surface selects it.
    let surfaces: Value = serde_json::from_str(&repo_script("e2e-tui-surfaces.json")).unwrap();
    let mut mapped: std::collections::BTreeSet<String> = surfaces["surfaces"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|s| strings(&s["modules"]))
        .collect();
    mapped.extend(strings(&surfaces["unmapped_ok"]));
    let root = std::fs::read_to_string(
        std::path::Path::new(rsi_common::manifest_dir!())
            .join("../..")
            .join(surfaces["root"].as_str().unwrap()),
    )
    .unwrap();
    let mounted: std::collections::BTreeSet<String> =
        crate::re::Re::new(r"(?m)^\s*(?:pub\s+)?mod\s+(\w+)\s*;")
            .findall1(&root)
            .into_iter()
            .collect();
    assert!(!mounted.is_empty());
    let missing: Vec<_> = mounted.difference(&mapped).collect();
    assert!(
        missing.is_empty(),
        "e2e_tui modules missing from scripts/e2e-tui-surfaces.json: {missing:?}"
    );
}

#[test]
fn every_mapped_module_selected_collapses_to_the_whole_target() {
    let mut case = Case::new();
    e2e_fixture(&mut case, &E2E_MODULES);
    for path in [
        "overlay/harness_manager.rs",
        "settings.rs",
        "todo_panel.rs",
        "overlay/graph.rs",
        "app/agent_projects.rs",
        "overlay/project_form.rs",
    ] {
        case.write(&format!("crates/rsi/src/{path}"), "pub fn f() {}\n");
    }
    case.commit();
    let r = case.check(&["--no-compile"]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(e2e_filters(&r), ["rsi=test:e2e_tui"]);
}

#[test]
fn default_binary_main_change_selects_its_binary_tests() {
    let mut case = Case::new();
    case.write(
        "crates/rsi-remote/src/main.rs",
        "fn main() {}\n#[test]\nfn decision_targets_test() {}\n",
    );
    case.commit();
    case.rebase_base_here();
    case.write(
        "crates/rsi-remote/src/main.rs",
        "fn main() { handler(); }\n#[test]\nfn decision_targets_test() {}\n",
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(r.receipt["shards"]["compiled_ok"], json!([]));
    assert_eq!(
        r.receipt["suggested_filters"],
        json!(["rsi-remote=bin:rsi-remote"])
    );
}

#[test]
fn main_test_added_beside_library_module_changes_adds_the_binary_filter() {
    let mut case = Case::new();
    case.write("crates/rsi-remote/src/lib.rs", "pub mod answers;\n");
    case.write(
        "crates/rsi-remote/src/answers.rs",
        "pub fn a() {}\n#[cfg(test)]\nmod tests { #[test] fn a() {} }\n",
    );
    case.write("crates/rsi-remote/src/main.rs", "fn main() {}\n");
    case.commit();
    case.rebase_base_here();
    case.write(
        "crates/rsi-remote/src/answers.rs",
        "pub fn a() { changed(); }\n#[cfg(test)]\nmod tests { #[test] fn a() {} }\n",
    );
    case.write(
        "crates/rsi-remote/src/main.rs",
        "fn main() {}\n#[tokio::test]\nasync fn end_to_end_through_the_real_handler() {}\n",
    );
    case.commit();
    let r = case.check(&["--no-compile"]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(
        r.receipt["suggested_filters"],
        json!(["rsi-remote=answers", "rsi-remote=bin:rsi-remote"])
    );
}

#[test]
fn filters_only_derives_the_same_filters_without_compile_or_heavy_audits() {
    let mut case = Case::new();
    case.write(
        "crates/rsid/src/foo.rs",
        &(gate("store-02", "a") + "const EDIT: u8 = 1;\n"),
    );
    case.write(
        "crates/rsid/src/bin/tool.rs",
        "fn main() { x(); }\n#[test]\nfn t() {}\n",
    );
    case.commit();
    let full = case.check(&["--no-compile"]);
    let r = case.check(&["--filters-only"]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(
        r.receipt["suggested_filters"],
        full.receipt["suggested_filters"]
    );
    assert!(strings(&r.receipt["suggested_filters"]).contains(&"rsid=bin:tool".to_owned()));
    assert_eq!(r.receipt["shards"]["compile_skipped"], json!(true));
    assert_eq!(r.receipt["filters_only"], json!(true));
    assert_eq!(
        r.receipt["audits"]["shard_gates"]["verdict"],
        json!("skipped")
    );
    assert_eq!(
        r.receipt["audits"]["released_migrations"]["verdict"],
        json!("skipped")
    );
    assert_eq!(r.receipt["audits"]["diff_check"]["verdict"], json!("ok"));
    assert!(r.receipt["timings_sec"].get("filters").is_some());
}

#[test]
fn binary_source_scanner_is_selected_when_the_scanned_source_changes() {
    let mut case = Case::new();
    case.write("crates/rsid/src/lib.rs", "pub mod launch;\n");
    case.write("crates/rsid/src/launch.rs", "pub fn launch() {}\n");
    case.write(
        "crates/rsid/src/main.rs",
        "fn main() {}\n#[test]\nfn launch_scanner() { let _ = include_str!(\"launch.rs\"); }\n",
    );
    case.write(
        "crates/rsid/src/bin/other.rs",
        "fn main() {}\n#[test]\nfn t() {}\n",
    );
    case.commit();
    case.rebase_base_here();
    case.write(
        "crates/rsid/src/launch.rs",
        "pub fn launch() { changed(); }\n",
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    let filters = strings(&r.receipt["suggested_filters"]);
    assert!(filters.contains(&"rsid=bin:rsid".to_owned()), "{filters:?}");
    assert!(!filters.contains(&"rsid=bin:other".to_owned()));
}

#[test]
fn registry_and_spawn_source_changes_select_target_source_scanners() {
    let mut case = Case::new();
    case.write("crates/rsid/src/lib.rs", "pub fn marker() {}\n");
    case.write(
        "crates/rsid/src/bin/rsi-model-control-validate.rs",
        "fn fixture_root() -> String { let _ = \"crates/rsid/src\"; let _ = \"../rsid-store/src\"; std::env::var(\"CARGO_MANIFEST_DIR\").unwrap() }\n#[test]\nfn validates_registry() { let _ = fixture_root(); let _ = \"crates/rsid-store/src/model_control/registry.rs\"; }\n",
    );
    case.write(
        "crates/rsid-store/src/model_control/registry.rs",
        "pub fn registry() {}\n",
    );
    case.write("crates/rsid/src/process.rs", "pub fn spawn() {}\n");
    case.commit();
    case.rebase_base_here();
    let anchor = case.base.clone();
    case.write(
        "crates/rsid-store/src/model_control/registry.rs",
        "pub fn registry() { changed(); }\n",
    );
    case.commit();
    let r = case.check(&["--filters-only"]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert!(
        strings(&r.receipt["suggested_filters"])
            .contains(&"rsid=bin:rsi-model-control-validate".to_owned())
    );
    // reset to base, then a new process spawn
    case.write(
        "crates/rsid-store/src/model_control/registry.rs",
        "pub fn registry() {}\n",
    );
    case.head = anchor;
    case.write(
        "crates/rsid/src/process.rs",
        "pub fn spawn() { std::process::Command::new(\"git\"); }\n",
    );
    case.commit();
    let r = case.check(&["--filters-only"]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert!(
        strings(&r.receipt["suggested_filters"])
            .contains(&"rsid=bin:rsi-model-control-validate".to_owned())
    );
}

// ---------------------------------------------------- scanners and comment-only

const SCANNER: &str = "pub mod p2_07_gate_permit_spine {\n    #[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-session-02\"))]\n    #[test]\n    fn exactly_one_writer() {\n        let root = std::path::PathBuf::from(rsi_common::manifest_dir!()).join(\"../rsid-store/src\");\n        let _ = std::fs::read_dir(root);\n    }\n}\n#[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-session-02\"))]\n#[test]\nfn names_no_source_tree() { let _ = std::env::var(\"CARGO_MANIFEST_DIR\"); }\n";

fn scanner_tree(case: &mut Case) {
    case.write("crates/rsid/src/lib.rs", "pub mod session;\n");
    case.write("crates/rsid/src/session/issue21_phase2_tests.rs", SCANNER);
    case.write("crates/rsid-store/src/lib.rs", "pub mod store;\n");
    case.write(
        "crates/rsid-store/src/store/cohort_settlement.rs",
        "pub fn settle() {}\n",
    );
    case.write("crates/rsi-common/src/other.rs", "pub fn other() {}\n");
    case.commit();
    case.rebase_base_here();
}

#[test]
fn store_only_diff_selects_the_source_scanners_that_read_the_store() {
    let mut case = Case::new();
    scanner_tree(&mut case);
    case.write(
        "crates/rsid-store/src/store/cohort_settlement.rs",
        "pub fn settle() { changed(); }\n",
    );
    case.commit();
    for flags in [["--no-compile"], ["--filters-only"]] {
        let r = case.check(&flags);
        assert_eq!(r.code, 0, "{}", r.summary);
        let joined = strings(&r.receipt["suggested_filters"]).join("\n");
        assert!(
            joined.contains("session::issue21_phase2_tests::(?:\\w+::)*exactly_one_writer"),
            "{joined}"
        );
        assert!(!joined.contains("names_no_source_tree"));
        assert_eq!(
            r.receipt["source_scanners"],
            json!(["session::issue21_phase2_tests::exactly_one_writer"])
        );
    }
}

#[test]
fn diff_outside_the_scanned_crates_leaves_scanners_out() {
    let mut case = Case::new();
    scanner_tree(&mut case);
    case.write(
        "crates/rsi-common/src/other.rs",
        "pub fn other() { changed(); }\n",
    );
    case.commit();
    let r = case.check(&["--filters-only"]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(r.receipt["source_scanners"], json!([]));
    assert!(
        !strings(&r.receipt["suggested_filters"])
            .join("\n")
            .contains("exactly_one_writer")
    );
}

const TOKEN_SCANNERS: [&str; 2] = [
    "store::cold_archive::read_classes::cold_archive_reader_classification",
    "store::cold_archive::read_tests::cold_archive_reader_invariance",
];

fn token_scanner_tree(case: &mut Case, token: &str) {
    let mut registry: Value = serde_json::from_str(&repo_script("scanned-tokens.json")).unwrap();
    if token != "conversation_events" {
        let moved = registry["tokens"]
            .as_object_mut()
            .unwrap()
            .remove("conversation_events")
            .unwrap();
        registry["tokens"]
            .as_object_mut()
            .unwrap()
            .insert(token.into(), moved);
    }
    case.write("scripts/scanned-tokens.json", &registry.to_string());
    case.write("crates/rsid-store/src/lib.rs", "pub mod store;\n");
    case.write(
        "crates/rsid-store/src/store/mod.rs",
        "pub mod cold_archive;\n",
    );
    case.write(
        "crates/rsid-store/src/store/cold_archive/mod.rs",
        "#[cfg(test)]\nmod read_classes;\n#[cfg(test)]\nmod read_tests;\n",
    );
    for (module, name) in ["read_classes", "read_tests"].iter().zip(TOKEN_SCANNERS) {
        case.write(
            &format!("crates/rsid-store/src/store/cold_archive/{module}.rs"),
            &gate("store-04", name.rsplit("::").next().unwrap()),
        );
    }
    case.commit();
    case.rebase_base_here();
}

fn assert_token_scanners(r: &Checked) {
    assert_eq!(
        r.receipt["source_scanners"],
        json!(TOKEN_SCANNERS),
        "{}",
        r.summary
    );
    let joined = strings(&r.receipt["suggested_filters"]).join("\n");
    for name in TOKEN_SCANNERS {
        assert!(
            joined.contains(name.rsplit("::").next().unwrap()),
            "{name} {joined}"
        );
    }
}

#[test]
fn new_token_reader_in_each_crate_selects_registered_scanners() {
    let mut case = Case::new();
    token_scanner_tree(&mut case, "conversation_events");
    for package in ["rsid-store", "rsid", "rsid-remote-read"] {
        case.write(
            &format!("crates/{package}/src/new_reader.rs"),
            "pub fn new_reader() { query(\"SELECT id FROM conversation_events\"); }\n",
        );
        case.commit();
        let r = case.check(&["--filters-only"]);
        assert_eq!(r.code, 0, "{package} {}", r.summary);
        assert_token_scanners(&r);
        case.rebase_base_here();
    }
}

#[test]
fn existing_token_reader_predicate_edit_and_removal_select_scanners() {
    let mut case = Case::new();
    token_scanner_tree(&mut case, "conversation_events");
    let path = "crates/rsid-store/src/store/existing.rs";
    case.write(path, "pub fn existing() {\n    query(\"SELECT id FROM conversation_events\");\n    gate(1);\n}\n");
    case.commit();
    case.rebase_base_here();
    for text in [
        "pub fn existing() {\n    query(\"SELECT id FROM conversation_events\");\n    gate(2);\n}\n",
        "pub fn existing() { query(\"SELECT id FROM another_table\"); }\n",
    ] {
        case.write(path, text);
        case.commit();
        let r = case.check(&["--filters-only"]);
        assert_eq!(r.code, 0, "{}", r.summary);
        assert_token_scanners(&r);
    }
}

#[test]
fn unrelated_and_test_only_token_edits_keep_focused_selection() {
    let mut case = Case::new();
    token_scanner_tree(&mut case, "conversation_events");
    case.write(
        "crates/rsid/src/unrelated.rs",
        "pub fn unrelated() { changed(); }\n",
    );
    case.write(
        "crates/rsid-remote-read/src/tests.rs",
        "#[test]\nfn fixture() { query(\"conversation_events\"); }\n",
    );
    case.commit();
    let r = case.check(&["--filters-only"]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(r.receipt["source_scanners"], json!([]));
}

#[test]
fn future_scanner_token_needs_only_a_registry_entry() {
    let mut case = Case::new();
    token_scanner_tree(&mut case, "future_scanned_table");
    case.write(
        "crates/rsid/src/future_reader.rs",
        "pub fn future_reader() { query(\"future_scanned_table\"); }\n",
    );
    case.commit();
    let r = case.check(&["--filters-only"]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_token_scanners(&r);
}

#[test]
fn unprovable_token_leaf_keeps_scoped_pins_and_unfiltered_landing() {
    let mut case = Case::new();
    token_scanner_tree(&mut case, "conversation_events");
    let path = "crates/rsid-remote-read/src/new_reader.rs";
    case.write(path, "pub const SQL: &str = \"conversation_events\";\n");
    case.commit();
    let r = case.check(&["--filters-only"]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(r.receipt["suggested_filters"], json!([]));
    assert_eq!(r.receipt["unfiltered_leaf_paths"], json!([path]));
    assert_eq!(r.receipt["source_scanners"], json!(TOKEN_SCANNERS));
    let joined = strings(&r.receipt["scanned_token_filters"]).join("\n");
    for name in TOKEN_SCANNERS {
        assert!(joined.contains(name.rsplit("::").next().unwrap()));
    }
}

#[test]
fn unresolved_registered_scanner_fails_derivation() {
    let mut case = Case::new();
    token_scanner_tree(&mut case, "conversation_events");
    case.write(
        "crates/rsid/src/new_reader.rs",
        "pub fn read() { query(\"conversation_events\"); }\n",
    );
    case.write(
        "crates/rsid-store/src/store/cold_archive/read_classes.rs",
        "// removed\n",
    );
    case.commit();
    let r = case.check(&["--filters-only"]);
    assert_ne!(r.code, 0);
    assert!(
        r.summary.contains("scanned-token test does not resolve"),
        "{}",
        r.summary
    );
}

#[test]
fn integration_test_reading_a_changed_command_file_is_selected() {
    let mut case = Case::new();
    case.write("crates/rsi-common/src/lib.rs", "pub fn marker() {}\n");
    case.write(".claude/commands/research.md", "old\n");
    case.write(
        "crates/rsi-common/tests/reads_command.rs",
        "#[test]\nfn reads() { let _ = root.join(\".claude/commands/research.md\"); }\n",
    );
    case.write("crates/rsid/src/lib.rs", "pub fn marker() {}\n");
    case.write(
        "crates/rsid/tests/joins_command.rs",
        "#[test]\nfn joins() { let _ = cur.join(\".claude\").join(\"commands\"); }\n",
    );
    case.write(
        "crates/rsid/tests/reads_skill.rs",
        "#[test]\nfn skill() { let _ = root.join(\".agents/skills/x/SKILL.md\"); }\n",
    );
    case.write(
        "crates/rsid/tests/unrelated.rs",
        "#[test]\nfn other() { let _ = \"settings\"; }\n",
    );
    case.commit();
    case.rebase_base_here();
    case.write(".claude/commands/research.md", "new\n");
    case.commit();
    let r = case.check(&["--filters-only"]);
    assert_eq!(r.code, 0, "{}", r.summary);
    let filters = strings(&r.receipt["suggested_filters"]);
    assert!(
        filters.contains(&"rsi-common=test:reads_command".to_owned()),
        "{filters:?}"
    );
    assert!(filters.contains(&"rsid=test:joins_command".to_owned()));
    assert!(!filters.contains(&"rsid=test:reads_skill".to_owned()));
    assert!(!filters.contains(&"rsid=test:unrelated".to_owned()));
}

fn gated_tests(shard: &str, names: &[String], calls: &[(&str, &str)]) -> String {
    names
        .iter()
        .map(|name| match calls.iter().find(|(n, _)| n == name) {
            Some((_, call)) => with_body(&gate(shard, name), &format!("{call}();")),
            None => gate(shard, name),
        })
        .collect()
}
fn numbered(prefix: &str, count: usize) -> Vec<String> {
    (0..count).map(|n| format!("{prefix}{n}")).collect()
}

#[test]
fn comment_and_doc_only_rust_hunks_select_only_source_readers() {
    let mut case = Case::new();
    let path = "crates/rsid/src/session/launch.rs";
    let original = "/// Old launch guidance.\npub fn launch() {\n    work(); // old note\n}\n/* old block /* nested */ done */\n";
    case.write("crates/rsid/src/lib.rs", "pub mod session;\n");
    case.write("crates/rsid/src/session/mod.rs", "mod launch;\n");
    let runtime = gated_tests("session-01", &numbered("runtime_", 70), &[]);
    case.write(path, &format!("{original}{runtime}"));
    case.write(
        "crates/rsid/src/session/scanners.rs",
        &format!(
            "fn read_launch() {{\n    let source = include_str!(\"launch.rs\");\n}}\n{}",
            gated_tests(
                "other-03",
                &["launch_guidance_contract".to_owned()],
                &[("launch_guidance_contract", "read_launch")]
            )
        ),
    );
    case.write("crates/rsid/src/main.rs", "fn main() {}\n#[test]\nfn source_contract() { let _ = include_str!(\"session/launch.rs\"); }\n");
    case.commit();
    case.rebase_base_here();
    case.write(
        path,
        &format!("{original}{runtime}")
            .replace("Old launch guidance.", "New launch guidance.")
            .replace("old note", "new note AGENT_VERBS")
            .replace(
                "old block /* nested */",
                "new block /* nested /* deeper */ */",
            ),
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(r.receipt["broad"], json!(false));
    assert_eq!(r.receipt["shards"]["compiled_ok"], json!(["session-01"]));
    assert_eq!(
        filter_set(&r),
        set_of(&[
            "rsid=bin:rsid",
            "rsid=shard:other-03:test(/^session::scanners::(?:\\w+::)*launch_guidance_contract$/)"
        ])
    );
    assert!(
        r.receipt["narrowed_filters"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("only Rust comments/docs")
    );
}

#[test]
fn comment_deletion_without_source_readers_keeps_only_compile_check() {
    let mut case = Case::new();
    case.write(
        "crates/rsid/src/foo.rs",
        &format!("/// Guidance\n{}", gate("store-02", "a")),
    );
    case.commit();
    case.rebase_base_here();
    case.write("crates/rsid/src/foo.rs", &gate("store-02", "a"));
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(r.receipt["suggested_filters"], json!([]));
    assert_eq!(r.receipt["shards"]["compiled_ok"], json!(["store-02"]));
}

#[test]
fn rust_comment_classification_preserves_literals_attributes_and_mixed_code() {
    let classify = super::tree::rust_comment_only_change;
    for original in [
        "fn f() { let s = \"https://old/*value*/\"; } // guide",
        "fn f() { let s = r##\"/*old*/ // value\"##; } // guide",
        "fn f() { let s = b\"//old\"; } // guide",
        "fn old() { let c = '/'; } // guide",
        "#[doc = \"old\"]\nfn f() {} // guide",
        "fn f() { old(); } // guide",
    ] {
        assert!(
            classify(original, &original.replace("guide", "new guide")),
            "{original}"
        );
        assert!(
            !classify(
                original,
                &original.replace("old", "new").replace("guide", "new guide")
            ),
            "{original}"
        );
    }
    assert!(!classify("fn f() {} // old", "fn f() {} /* unterminated"));
    assert!(!classify(
        "fn f() { let c = '/'; } // guide",
        "fn f() { let c = '*'; } // new guide"
    ));
    assert!(!classify("fn f() {}", "fn f() { changed(); } // guide"));
    assert!(classify("/** old */\nfn f() {}", "/** new */\nfn f() {}"));
}

fn registration_fixture(case: &mut Case, package: &str, module: &str) -> String {
    let root = format!("crates/{package}/src/{module}/mod.rs");
    case.write(
        &format!("crates/{package}/src/lib.rs"),
        &format!("pub mod {module};\n"),
    );
    case.write(&root, "mod old;\nmod new;\npub fn marker() {}\n");
    case.write(
        &format!("crates/{package}/src/{module}/old.rs"),
        &gate("store-02", "old"),
    );
    case.write(
        &format!("crates/{package}/src/{module}/new.rs"),
        &gate("other-03", "new"),
    );
    case.commit();
    case.rebase_base_here();
    root
}

#[test]
fn registration_additions_select_only_the_registered_modules() {
    let mut case = Case::new();
    let session = registration_fixture(&mut case, "rsid", "session");
    let store = registration_fixture(&mut case, "rsid-store", "store");
    case.write(
        &session,
        "mod old;\nmod new;\nmod added; // registration\npub fn marker() {}\n",
    );
    case.write(
        "crates/rsid/src/session/added.rs",
        &gate("other-03", "added"),
    );
    case.write(
        &store,
        "mod old;\nmod new;\npub mod added;\npub use new::Item;\npub fn marker() {}\n",
    );
    case.write(
        "crates/rsid-store/src/store/added.rs",
        &gate("store-02", "added"),
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(
        filter_set(&r),
        set_of(&[
            "rsid=shard:other-03:test(session::added)",
            "rsid=shard:other-03:test(store::new)",
            "rsid=shard:store-02:test(store::added)"
        ])
    );
}

#[test]
fn path_registration_in_regular_rust_file_selects_only_the_new_module() {
    let mut case = Case::new();
    let path = "crates/rsid-store/src/store/pending_questions.rs";
    let original = gated_tests("store-03", &numbered("existing_", 10), &[]);
    case.write("crates/rsid-store/src/lib.rs", "pub mod store;\n");
    case.write(
        "crates/rsid-store/src/store/mod.rs",
        "pub mod pending_questions;\n",
    );
    case.write(path, &original);
    case.commit();
    case.rebase_base_here();
    case.write(
        path,
        &format!("#[path = \"remote_answers.rs\"]\npub mod remote_answers;\n{original}"),
    );
    case.write(
        "crates/rsid-store/src/store/remote_answers.rs",
        "#[cfg(test)]\n#[path = \"remote_answers/tests.rs\"]\nmod tests;\n",
    );
    case.write(
        "crates/rsid-store/src/store/remote_answers/tests.rs",
        &gated_tests("store-04", &["new_answer_journal".to_owned()], &[]),
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(
        r.receipt["suggested_filters"],
        json!(["rsid=shard:store-04:test(store::pending_questions::remote_answers)"])
    );
}

#[test]
fn plain_mod_tests_under_a_path_mount_does_not_claim_the_stem_directory_file_1697() {
    let mut case = Case::new();
    let path = "crates/rsid-store/src/store/pending_questions.rs";
    let original = gated_tests("store-03", &numbered("existing_", 10), &[]);
    case.write("crates/rsid-store/src/lib.rs", "pub mod store;\n");
    case.write(
        "crates/rsid-store/src/store/mod.rs",
        "pub mod pending_questions;\n",
    );
    case.write(path, &original);
    case.write(
        "crates/rsid-store/src/store/remote_answers.rs",
        "pub fn f() {}\n",
    );
    case.write(
        "crates/rsid-store/src/store/remote_answers/tests.rs",
        &gated_tests("store-02", &["never_compiled".to_owned()], &[]),
    );
    case.commit();
    case.rebase_base_here();
    case.write(
        path,
        &format!("#[path = \"remote_answers.rs\"]\npub mod remote_answers;\n{original}"),
    );
    case.write(
        "crates/rsid-store/src/store/remote_answers.rs",
        "pub fn f() {}\n#[cfg(test)]\nmod tests;\n",
    );
    case.write(
        "crates/rsid-store/src/store/tests.rs",
        &gated_tests("store-04", &["compiled_beside".to_owned()], &[]),
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert!(r.receipt["shards"]["map"].get("store-02").is_none());
    assert_eq!(
        r.receipt["shards"]["map"]["store-04"],
        json!([
            "crates/rsid-store/src/store/remote_answers.rs",
            "crates/rsid-store/src/store/tests.rs"
        ])
    );
    assert_eq!(
        r.receipt["suggested_filters"],
        json!(["rsid=shard:store-04:test(store::pending_questions::remote_answers)"])
    );
}

#[test]
fn registration_removal_selects_the_removed_module() {
    let mut case = Case::new();
    let root = registration_fixture(&mut case, "rsid", "session");
    case.write(&root, "mod new;\npub fn marker() {}\n");
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(
        r.receipt["suggested_filters"],
        json!(["rsid=shard:store-02:test(session::old)"])
    );
    assert_eq!(r.receipt["shards"]["compiled_ok"], json!(["store-02"]));
}

#[test]
fn pub_use_removal_selects_the_reexported_module() {
    let mut case = Case::new();
    let root = registration_fixture(&mut case, "rsid", "session");
    case.write(
        &root,
        "mod old;\nmod new;\npub use self::new::{Item, Other};\npub fn marker() {}\n",
    );
    case.commit();
    case.rebase_base_here();
    case.write(&root, "mod old;\nmod new;\npub fn marker() {}\n");
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(
        r.receipt["suggested_filters"],
        json!(["rsid=shard:other-03:test(session::new)"])
    );
}

#[test]
fn pub_use_paths_and_aliases_select_the_reexported_module() {
    let mut case = Case::new();
    let root = registration_fixture(&mut case, "rsid", "session");
    for usage in [
        "new",
        "new as renamed",
        "crate::session::new",
        "super::session::new::Item",
        "self::new::*",
    ] {
        case.write(
            &root,
            &format!("mod old;\nmod new;\npub(crate) use {usage};\npub fn marker() {{}}\n"),
        );
        case.commit();
        let r = case.check(&[]);
        assert_eq!(r.code, 0, "{usage} {}", r.summary);
        assert_eq!(
            r.receipt["suggested_filters"],
            json!(["rsid=shard:other-03:test(session::new)"]),
            "{usage}"
        );
    }
}

#[test]
fn real_mod_rs_change_retains_the_root_filter() {
    let mut case = Case::new();
    let root = registration_fixture(&mut case, "rsid", "session");
    case.write(
        &root,
        "mod new;\npub fn marker() { println!(\"changed\"); }\n",
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(
        r.receipt["suggested_filters"],
        json!(["rsid=shard:other-03:test(session)"])
    );
}

#[test]
fn removing_code_from_mod_rs_retains_the_root_filter() {
    let mut case = Case::new();
    let root = registration_fixture(&mut case, "rsid", "session");
    case.write(&root, "mod old;\nmod new;\n");
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(
        r.receipt["suggested_filters"],
        json!(["rsid=shard:other-03:test(session)"])
    );
}

#[test]
fn binary_submodule_selects_its_binary_even_when_tests_are_elsewhere() {
    let mut case = Case::new();
    case.write("crates/rsid/src/bin/tool/helper.rs", "pub fn helper() {}\n");
    case.write("crates/rsi-common/src/bin/nested/main.rs", "fn main() {}\n");
    case.write(
        "crates/rsi-common/src/bin/nested/tests.rs",
        "#[test]\nfn t() {}\n",
    );
    case.write(
        "crates/rsi-common/src/bin/nested/deep/helper.rs",
        "pub fn helper() {}\n",
    );
    case.commit();
    case.rebase_base_here();
    case.write(
        "crates/rsid/src/bin/tool/helper.rs",
        "pub fn helper() { }\n",
    );
    case.write(
        "crates/rsi-common/src/bin/nested/deep/helper.rs",
        "pub fn helper() { }\n",
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(
        r.receipt["suggested_filters"],
        json!(["rsi-common=bin:nested", "rsid=bin:tool"])
    );
    assert_eq!(r.receipt["shards"]["compiled_ok"], json!([]));
}

/// The suggested filters of a diff that rewrites `changed` on top of `base`.
fn filters_after(base: &[(&str, &str)], changed: &[(&str, &str)]) -> Vec<String> {
    let mut case = Case::new();
    for (path, text) in base {
        case.write(path, text);
    }
    case.commit();
    case.rebase_base_here();
    for (path, text) in changed {
        case.write(path, text);
    }
    case.commit();
    let r = case.check(&["--filters-only"]);
    assert_eq!(r.code, 0, "{}", r.summary);
    strings(&r.receipt["suggested_filters"])
}

#[test]
fn tests_helper_module_selects_only_the_integration_targets_that_mount_it() {
    let filters = filters_after(
        &[
            (
                "crates/rsi-common/tests/a.rs",
                "mod common;\n#[test]\nfn t() {}\n",
            ),
            ("crates/rsi-common/tests/b.rs", "#[test]\nfn t() {}\n"),
            ("crates/rsi-common/tests/common/mod.rs", "pub fn h() {}\n"),
        ],
        &[(
            "crates/rsi-common/tests/common/mod.rs",
            "pub fn h() { work(); }\n",
        )],
    );
    assert_eq!(filters, ["rsi-common=test:a"]);
}

#[test]
fn integration_directory_target_and_mounted_suite_modules_select_their_target() {
    let filters = filters_after(
        &[
            (
                "crates/rsi-common/tests/dir/main.rs",
                "mod sub;\n#[test]\nfn t() {}\n",
            ),
            ("crates/rsi-common/tests/dir/sub.rs", "pub fn s() {}\n"),
            (
                "crates/rsi-common/tests/suite.rs",
                "mod cases;\n#[test]\nfn t() {}\n",
            ),
            ("crates/rsi-common/tests/suite/cases.rs", "pub fn c() {}\n"),
        ],
        &[
            (
                "crates/rsi-common/tests/dir/sub.rs",
                "pub fn s() { work(); }\n",
            ),
            (
                "crates/rsi-common/tests/suite/cases.rs",
                "pub fn c() { work(); }\n",
            ),
        ],
    );
    assert_eq!(filters, ["rsi-common=test:dir", "rsi-common=test:suite"]);
}

#[test]
fn module_mounted_only_by_the_binary_selects_the_binary_not_a_lib_filter() {
    let gateway = "pub fn g() {}\n#[cfg(test)]\nmod tests { #[test] fn a() {} }\n";
    for lib in [false, true] {
        let mut base = vec![
            (
                "crates/rsi-remote/src/main.rs",
                "mod gateway;\nfn main() {}\n",
            ),
            ("crates/rsi-remote/src/gateway.rs", gateway),
        ];
        if lib {
            base.push(("crates/rsi-remote/src/lib.rs", "pub fn l() {}\n"));
        }
        let changed = gateway.replace("g() {}", "g() { work(); }");
        let filters = filters_after(
            &base,
            &[("crates/rsi-remote/src/gateway.rs", changed.as_str())],
        );
        assert_eq!(filters, ["rsi-remote=bin:rsi-remote"], "lib={lib}");
    }
}

#[test]
fn module_shared_by_the_library_and_a_binary_selects_both() {
    let filters = filters_after(
        &[
            ("crates/rsi-common/src/lib.rs", "pub mod foo;\n"),
            (
                "crates/rsi-common/src/bin/n.rs",
                "#[path = \"../foo.rs\"]\nmod foo;\nfn main() {}\n",
            ),
            (
                "crates/rsi-common/src/foo.rs",
                "pub fn f() {}\n#[test]\nfn t() {}\n",
            ),
        ],
        &[(
            "crates/rsi-common/src/foo.rs",
            "pub fn f() { work(); }\n#[test]\nfn t() {}\n",
        )],
    );
    assert_eq!(filters, ["rsi-common=bin:n", "rsi-common=foo"]);
}

#[test]
fn sources_that_include_a_changed_data_file_run_their_own_tests() {
    let read = |path: &str| {
        format!(
            "#[cfg(test)]\nmod tests {{ #[test] fn reads() {{ let _ = include_str!(\"{path}\"); }} }}\n"
        )
    };
    let schema = format!("pub fn s() {{}}\n{}", read("../../../docs/x.md"));
    let other = format!("pub fn o() {{}}\n{}", read("../../../docs/y.md"));
    let mut filters = filters_after(
        &[
            ("docs/x.md", "old\n"),
            ("docs/y.md", "old\n"),
            (
                "crates/rsi-common/src/lib.rs",
                "pub mod schema;\npub mod other;\n",
            ),
            ("crates/rsi-common/src/schema.rs", &schema),
            ("crates/rsi-common/src/other.rs", &other),
            (
                "crates/rsi-common/src/bin/tool.rs",
                "fn main() {}\n#[test]\nfn t() { let _ = include_bytes!(\"../../../../docs/x.md\"); }\n",
            ),
            (
                "crates/rsi/tests/it.rs",
                "#[test]\nfn t() { let _ = include_str!(\"../../../docs/x.md\"); }\n",
            ),
        ],
        &[("docs/x.md", "new\n")],
    );
    filters.sort();
    assert_eq!(
        filters,
        ["rsi-common=bin:tool", "rsi-common=schema", "rsi=test:it"]
    );
}

#[test]
fn binary_test_reading_a_changed_skill_file_is_selected() {
    let filters = filters_after(
        &[
            (".claude/skills/x/SKILL.md", "old\n"),
            (
                "crates/rsi-common/src/bin/tool.rs",
                "fn main() {}\n#[test]\nfn t() { let _ = root.join(\".claude/skills/x/SKILL.md\"); }\n",
            ),
            (
                "crates/rsi-common/src/bin/other.rs",
                "fn main() {}\n#[test]\nfn t() {}\n",
            ),
        ],
        &[(".claude/skills/x/SKILL.md", "new\n")],
    );
    assert_eq!(filters, ["rsi-common=bin:tool"]);
}

#[test]
fn package_fixture_read_by_a_path_literal_selects_only_its_readers() {
    let filters = filters_after(
        &[
            ("crates/rsi-common/tests/fixtures/m/a.json", "{}"),
            (
                "crates/rsi-common/tests/corpus.rs",
                "#[test]\nfn t() { let _ = dir.join(\"fixtures\").join(\"m\"); }\n",
            ),
            (
                "crates/rsi-common/tests/other.rs",
                "#[test]\nfn t() { let _ = dir.join(\"snapshots\"); }\n",
            ),
            (
                "crates/rsi/tests/bare.rs",
                "#[test]\nfn t() { let _ = dir.join(\"a.json\"); }\n",
            ),
        ],
        &[("crates/rsi-common/tests/fixtures/m/a.json", "{\"x\": 1}")],
    );
    assert_eq!(filters, ["rsi-common=test:corpus"]);
}

fn synthetic_path_filters(reader: &str, extra: &[(&str, &str)]) -> Vec<String> {
    let tool = format!("fn main() {{}}\n{reader}");
    let mut base: Vec<(&str, &str)> = vec![
        ("docs/x.md", "old\n"),
        ("crates/rsid/src/bin/tool.rs", tool.as_str()),
        (
            "crates/rsid/src/bin/other.rs",
            "fn main() {}\n#[test]\nfn t() {}\n",
        ),
    ];
    for (path, text) in extra {
        base.retain(|(p, _)| p != path);
        base.push((path, text));
    }
    filters_after(&base, &[("docs/x.md", "new\n")])
}

#[test]
fn a_fabricated_path_list_fed_to_in_memory_code_selects_no_bin() {
    let filters = synthetic_path_filters(
        "#[test]\nfn t() {\n    assert!(classify(&[\"docs/x.md\", \"scripts/y.sh\"]));\n}\nfn classify(paths: &[&str]) -> bool {\n    paths.iter().any(|p| p.starts_with(\"docs/\"))\n}\n",
        &[],
    );
    assert!(filters.is_empty(), "{filters:?}");
}

#[test]
fn a_fabricated_path_loop_checked_in_memory_selects_no_bin() {
    let filters = synthetic_path_filters(
        "#[test]\nfn t() {\n    for path in [\n        \"docs/x.md\",\n        \"crates/a/Cargo.toml\",\n    ] {\n        assert_eq!(classify(&[path]), None);\n    }\n}\nfn classify(paths: &[&str]) -> Option<u8> {\n    paths.first().map(|_| 1)\n}\n",
        &[],
    );
    assert!(filters.is_empty(), "{filters:?}");
}

#[test]
fn a_path_list_a_test_reads_from_disk_selects_its_bin() {
    for (name, reader) in [
        (
            "fs read of each path",
            "#[test]\nfn t() {\n    for p in [\"docs/x.md\"] {\n        let _ = std::fs::read_to_string(root.join(p));\n    }\n}\n",
        ),
        (
            "same-file helper reads the list",
            "#[test]\nfn t() {\n    load(&[\"docs/x.md\"]);\n}\nfn load(paths: &[&str]) {\n    for p in paths { let _ = std::fs::read(p); }\n}\n",
        ),
        (
            "path method observes the list",
            "#[test]\nfn t() {\n    for p in [\"docs/x.md\"] { assert!(root.join(p).exists()); }\n}\n",
        ),
        (
            "alias hides the reader",
            "use std::fs::read as slurp;\n#[test]\nfn t() {\n    for p in [\"docs/x.md\"] { let _ = slurp(p); }\n}\n",
        ),
        (
            "glob import hides the reader",
            "use helpers::*;\n#[test]\nfn t() {\n    load_all(&[\"docs/x.md\"]);\n}\n",
        ),
        (
            "unknown macro",
            "#[test]\nfn t() {\n    read_docs!(&[\"docs/x.md\"]);\n}\n",
        ),
        (
            "include macro",
            "#[test]\nfn t() {\n    let _ = (&[\"docs/x.md\"], include_str!(\"../../../../docs/x.md\"));\n}\n",
        ),
        (
            "const list outside any function",
            "const DOCS: &[&str] = &[\"docs/x.md\"];\n#[test]\nfn t() {\n    for p in DOCS { check(p); }\n}\n",
        ),
        (
            "single path literal join",
            "#[test]\nfn t() {\n    let _ = root.join(\"docs/x.md\");\n}\n",
        ),
        (
            "function parameter may be a reader",
            "#[test]\nfn t() {\n    scan(&[\"docs/x.md\"], std::fs::read_to_string);\n}\nfn scan(paths: &[&str], read: fn(&str) -> String) {\n    for p in paths { read(p); }\n}\n",
        ),
    ] {
        assert_eq!(
            synthetic_path_filters(reader, &[]),
            ["rsid=bin:tool"],
            "{name}"
        );
    }
}

#[test]
fn a_path_list_whose_consumer_cannot_be_proved_in_memory_selects_its_bin() {
    for (name, reader) in [
        (
            "unknown receiver method on a bound list",
            "fn fixture(reader: &ExternalReader) {\n    let paths = [\"docs/x.md\"];\n    reader.load_paths(&paths);\n}\n",
        ),
        (
            "std-named method on an unknown receiver",
            "fn fixture(reader: &ExternalReader) {\n    let paths = [\"docs/x.md\"];\n    reader.get(&paths);\n}\n",
        ),
        (
            "std-named method taking the list directly",
            "fn fixture(reader: &ExternalReader) {\n    reader.get(&[\"docs/x.md\"]);\n}\n",
        ),
        (
            "callback aliased by let",
            "fn fixture(read: impl Fn(&str)) {\n    let reader = read;\n    for path in [\"docs/x.md\"] {\n        reader(path);\n    }\n}\n",
        ),
        (
            "external associated function",
            "fn fixture() {\n    external::load(&[\"docs/x.md\"]);\n}\n",
        ),
        (
            "external constructor then method",
            "fn fixture() {\n    let reader = ExternalReader::new();\n    reader.get(&[\"docs/x.md\"]);\n}\n",
        ),
        (
            "list used as a method receiver with a project callback",
            "fn fixture() {\n    [\"docs/x.md\"].iter().for_each(load_file);\n}\nfn load_file(p: &&str) {\n    let _ = std::fs::read(p);\n}\n",
        ),
        (
            "callback parameter of a project function",
            "fn fixture() {\n    run(&[\"docs/x.md\"], load_file);\n}\nfn run(paths: &[&str], f: fn(&str)) {\n    for p in paths { f(p); }\n}\nfn load_file(p: &str) {\n    let _ = std::fs::read(p);\n}\n",
        ),
        (
            "generic parameter of a project function",
            "fn fixture() {\n    run(&[\"docs/x.md\"]);\n}\nfn run<T: Loader>(paths: &[&str]) {\n    T::load(paths);\n}\n",
        ),
        (
            "project method with a std name that reads",
            "struct Loader;\nimpl Loader {\n    fn get(&self, paths: &[&str]) {\n        for p in paths { let _ = std::fs::read(p); }\n    }\n}\nfn fixture() {\n    check(&[\"docs/x.md\"], &Loader);\n}\nfn check(paths: &[&str], loader: &Loader) {\n    loader.get(paths);\n}\n",
        ),
        (
            "foreign field inside a project wrapper",
            "struct Local {\n    backend: ExternalReader,\n}\nfn fixture(reader: &Local) {\n    check(&[\"docs/x.md\"], reader);\n}\nfn check(paths: &[&str], reader: &Local) {\n    for path in paths {\n        reader.backend.get(path);\n    }\n}\n",
        ),
        (
            "foreign closure parameter",
            "fn fixture() {\n    let work = |reader: ExternalReader| {\n        for path in [\"docs/x.md\"] {\n            reader.get(path);\n        }\n    };\n}\n",
        ),
        (
            "foreign field built in the walk",
            "struct Local {\n    backend: ExternalReader,\n}\nfn fixture() {\n    let local = Local { backend: ExternalReader::new() };\n    check(&[\"docs/x.md\"], &local);\n}\nfn check(paths: &[&str], local: &Local) {\n    for path in paths {\n        local.backend.get(path);\n    }\n}\n",
        ),
        (
            "callback field fed by a function item",
            "struct Hook<'a> {\n    run: &'a dyn Fn(&str),\n}\nfn fixture() {\n    check(&[\"docs/x.md\"], &Hook { run: &load_file });\n}\nfn check(paths: &[&str], hook: &Hook) {\n    for path in paths { (hook.run)(path); }\n}\nfn load_file(p: &str) {\n    let _ = std::fs::read(p);\n}\n",
        ),
        (
            "callback field received from the caller",
            "struct Hook<'a> {\n    run: &'a dyn Fn(&str),\n}\nfn fixture(hook: &Hook) {\n    check(&[\"docs/x.md\"], hook);\n}\nfn check(paths: &[&str], hook: &Hook) {\n    for path in paths { (hook.run)(path); }\n}\n",
        ),
        (
            "constant holding a callback",
            "static READER: fn(&str) = load_file;\nfn fixture() {\n    for path in [\"docs/x.md\"] {\n        READER(path);\n    }\n}\nfn load_file(p: &str) {\n    let _ = std::fs::read(p);\n}\n",
        ),
        (
            "implicit Drop of a project type",
            "struct Local {\n    path: &'static str,\n}\nimpl Drop for Local {\n    fn drop(&mut self) {\n        let _ = std::fs::read(self.path);\n    }\n}\nfn fixture() {\n    for path in [\"docs/x.md\"] {\n        let local = Local { path };\n    }\n}\n",
        ),
        (
            "implicit Deref of a project type",
            "struct Local {\n    path: String,\n}\nimpl std::ops::Deref for Local {\n    type Target = str;\n    fn deref(&self) -> &str {\n        let _ = std::fs::read(&self.path);\n        &self.path\n    }\n}\nfn fixture() {\n    for path in [\"docs/x.md\"] {\n        let local = Local { path: path.to_string() };\n        assert!(local.len() > 0);\n    }\n}\n",
        ),
        (
            "implicit Index of a project type",
            "struct Local;\nimpl std::ops::Index<&str> for Local {\n    type Output = u8;\n    fn index(&self, path: &str) -> &u8 {\n        let _ = std::fs::read(path);\n        &0\n    }\n}\nfn fixture() {\n    let local = Local;\n    for path in [\"docs/x.md\"] {\n        let _ = local[path];\n    }\n}\n",
        ),
        (
            "implicit IntoIterator of a project type",
            "struct Local {\n    path: &'static str,\n}\nimpl IntoIterator for Local {\n    type Item = u8;\n    type IntoIter = std::vec::IntoIter<u8>;\n    fn into_iter(self) -> Self::IntoIter {\n        let _ = std::fs::read(self.path);\n        Vec::new().into_iter()\n    }\n}\nfn fixture() {\n    for path in [\"docs/x.md\"] {\n        for _ in (Local { path }) {}\n    }\n}\n",
        ),
        (
            "method of a project type the file does not define",
            "struct Local;\nfn fixture() {\n    for path in [\"docs/x.md\"] {\n        let local = Local;\n        assert!(local.ready(path));\n    }\n}\n",
        ),
        (
            "function value named but not called",
            "fn fixture() {\n    let paths = vec![\"docs/x.md\"];\n    let load = load_file;\n    check(&paths, &load);\n}\nfn check(paths: &[&str], load: &dyn Fn(&str)) {}\nfn load_file(p: &str) {\n    let _ = std::fs::read(p);\n}\n",
        ),
    ] {
        assert_eq!(
            synthetic_path_filters(&format!("{reader}#[test]\nfn runs() {{}}\n"), &[]),
            ["rsid=bin:tool"],
            "{name}"
        );
    }
}

#[test]
fn a_list_fed_to_a_struct_with_a_closure_built_in_the_same_fixture_selects_no_bin() {
    let filters = synthetic_path_filters(
        "struct Hook<'a> {\n    seen: &'a dyn Fn(&str) -> bool,\n}\n#[test]\nfn fixture() {\n    let seen = |path: &str| path.ends_with(\".md\");\n    assert!(check(&[\"docs/x.md\"], &Hook { seen: &seen }));\n}\nfn check(paths: &[&str], hook: &Hook) -> bool {\n    paths.iter().all(|path| (hook.seen)(path))\n}\n",
        &[],
    );
    assert!(filters.is_empty(), "{filters:?}");
}

#[test]
fn a_trait_a_sibling_file_implements_for_a_std_type_keeps_the_reader() {
    let extension = "use std::path::PathBuf;\npub trait Probe {\n    fn get(&self);\n}\nimpl Probe for PathBuf {\n    fn get(&self) {\n        let _ = std::fs::read(self);\n    }\n}\n";
    let user = |method: &str| {
        format!(
            "use std::path::PathBuf;\nmod extension;\nuse extension::Probe;\n#[test]\nfn fixture() {{\n    for path in [\"docs/x.md\"] {{\n        let p = PathBuf::from(path);\n        p.{method}();\n    }}\n}}\n"
        )
    };
    let files = [("crates/rsid/src/bin/tool/extension.rs", extension)];
    assert_eq!(
        synthetic_path_filters(&user("get"), &files),
        ["rsid=bin:tool"]
    );
    assert!(synthetic_path_filters(&user("clone"), &files).is_empty());
}

#[test]
fn a_re_exported_alias_of_an_extension_trait_keeps_the_reader() {
    let extension = "use std::path::PathBuf;\npub trait Probe {\n    fn get(&self);\n}\nimpl Probe for PathBuf {\n    fn get(&self) {\n        let _ = std::fs::read(self);\n    }\n}\npub use self::Probe as Access;\n";
    let user = "use std::path::PathBuf;\nmod extension;\nuse extension::Access;\n#[test]\nfn fixture() {\n    for path in [\"docs/x.md\"] {\n        let p = PathBuf::from(path);\n        p.get();\n    }\n}\n";
    assert_eq!(
        synthetic_path_filters(
            user,
            &[("crates/rsid/src/bin/tool/extension.rs", extension)]
        ),
        ["rsid=bin:tool"]
    );
}

#[test]
fn a_default_method_of_a_trait_with_a_blanket_impl_keeps_the_reader() {
    let extension = "pub trait Probe {\n    fn get(&self) {\n        let _ = std::fs::read(\"docs/x.md\");\n    }\n}\nimpl<T> Probe for T {}\npub use self::Probe as Access;\n";
    let user = "mod extension;\nuse extension::Access;\n#[test]\nfn fixture() {\n    for path in [\"docs/x.md\"] {\n        let p = String::from(path);\n        p.get();\n    }\n}\n";
    assert_eq!(
        synthetic_path_filters(
            user,
            &[("crates/rsid/src/bin/tool/extension.rs", extension)]
        ),
        ["rsid=bin:tool"]
    );
}

#[test]
fn a_std_name_beside_an_import_from_a_foreign_crate_keeps_the_reader() {
    let reader = "use itertools::Itertools;\n#[test]\nfn fixture() {\n    for path in [\"docs/x.md\"] {\n        let names = vec![path];\n        assert!(!names.iter().join(\",\").is_empty());\n    }\n}\n";
    assert_eq!(synthetic_path_filters(reader, &[]), ["rsid=bin:tool"]);
}

#[test]
fn a_trait_implemented_for_a_std_type_in_the_prelude_keeps_the_reader() {
    let extension = "use std::path::PathBuf;\npub struct Marker(pub &'static str);\nimpl From<Marker> for PathBuf {\n    fn from(marker: Marker) -> PathBuf {\n        let _ = std::fs::read(marker.0);\n        PathBuf::new()\n    }\n}\n";
    let user = "use std::path::PathBuf;\nmod extension;\n#[test]\nfn fixture() {\n    for path in [\"docs/x.md\"] {\n        let _ = PathBuf::from(extension::Marker(path));\n    }\n}\n";
    assert_eq!(
        synthetic_path_filters(
            user,
            &[("crates/rsid/src/bin/tool/extension.rs", extension)]
        ),
        ["rsid=bin:tool"]
    );
}

#[test]
fn a_path_list_handed_to_a_reader_in_another_file_selects_its_bin() {
    let filters = synthetic_path_filters(
        "mod loader;\n#[test]\nfn t() {\n    loader::load_docs(&[\"docs/x.md\"]);\n}\n",
        &[(
            "crates/rsid/src/bin/tool/loader.rs",
            "pub fn load_docs(paths: &[&str]) {\n    for p in paths { let _ = std::fs::read_to_string(p); }\n}\n",
        )],
    );
    assert_eq!(filters, ["rsid=bin:tool"]);
}

#[test]
fn a_path_list_handed_to_an_in_memory_helper_in_another_file_selects_no_bin() {
    let filters = synthetic_path_filters(
        "mod classify;\n#[test]\nfn t() {\n    assert!(classify::docs_only(&[\"docs/x.md\"]));\n}\n",
        &[(
            "crates/rsid/src/bin/tool/classify.rs",
            "pub fn docs_only(paths: &[&str]) -> bool {\n    paths.iter().all(|p| p.starts_with(\"docs/\"))\n}\n",
        )],
    );
    assert!(filters.is_empty(), "{filters:?}");
}

#[test]
fn a_synthetic_list_beside_a_real_read_of_the_same_file_selects_its_bin() {
    let filters = synthetic_path_filters(
        "#[test]\nfn fabricated() {\n    assert!(classify(&[\"docs/x.md\"]));\n}\nfn classify(paths: &[&str]) -> bool { !paths.is_empty() }\n#[test]\nfn real() {\n    let _ = std::fs::read_to_string(root.join(\"docs/x.md\"));\n}\n",
        &[],
    );
    assert_eq!(filters, ["rsid=bin:tool"]);
}

#[test]
fn reader_bin_in_another_crate_keeps_its_own_package() {
    let filters = filters_after(
        &[
            ("crates/rsi-common/AGENTS.md", "old\n"),
            (
                "crates/rsid/src/bin/tool.rs",
                "fn main() {}\n#[test]\nfn t() { let _ = include_str!(\"../../../rsi-common/AGENTS.md\"); }\n",
            ),
        ],
        &[("crates/rsi-common/AGENTS.md", "new\n")],
    );
    assert_eq!(filters, ["rsid=bin:tool"]);
}

#[test]
fn failed_shard_compile_fails_the_receipt() {
    let mut case = Case::new();
    case.write(
        "crates/rsid/src/foo.rs",
        &(gate("store-02", "a") + "const EDIT: u8 = 1;\n"),
    );
    case.commit();
    let options = Options {
        compile: true,
        env: Some(vec![("CHECK_TOUCHED_SHARDS_CARGO".into(), "false".into())]),
        ..Options::default()
    };
    let loaded = case.load(&case.base, &case.head).unwrap();
    let derived = derive::derive(&loaded, &options).unwrap();
    assert!(derived.failed);
    assert_eq!(
        derived.receipt["shards"]["compiled_failed"],
        json!(["store-02"])
    );
    assert!(derived.summary.contains("FAIL"));
}

#[test]
fn new_migration_numbering_and_dynamic_sql_are_reported() {
    let mut case = Case::new();
    case.write("crates/rsid/src/store/migrations/v152.rs", "// skips 151\n");
    case.write(
        "crates/rsid/src/foo.rs",
        &(gate("store-02", "a") + "fn q(t: &str) -> String { format!(\"SELECT * FROM {t}\") }\n"),
    );
    case.commit();
    let r = case.check(&["--no-compile"]);
    assert_eq!(
        r.receipt["migrations"],
        json!({"new": [152], "base_head": 150, "numbering_conflict": true, "pending_predecessors": [], "stray_pre_split": []})
    );
    assert_eq!(r.receipt["audits"]["dynamic_sql"]["verdict"], json!("flag"));
    assert_eq!(r.code, 1);
}

#[test]
fn conflicting_candidate_is_not_merge_clean_and_pins_follow_touched_files() {
    let mut case = Case::new();
    let base = case.base.clone();
    case.write("crates/rsi/src/key_tables.rs", "// base\n");
    let rolling = case.commit_on(&[&base]);
    case.write("crates/rsi/src/key_tables.rs", "// feature\n");
    case.commit_on(&[&base]);
    case.base = rolling;
    let r = case.check(&["--no-compile"]);
    assert_eq!(r.receipt["merge_clean"], json!(false));
    assert!(strings(&r.receipt["suggested_filters"]).contains(&"rsi=manual::".to_owned()));
    assert_eq!(r.code, 1);
}

#[test]
fn a_path_mounted_test_module_gets_its_mounted_module_path() {
    let mut case = Case::new();
    case.write("crates/rsid/src/lib.rs", "pub mod session;\n");
    case.write("crates/rsid/src/session/mod.rs", "mod tests;\n");
    case.write(
        "crates/rsid/src/session/tests/mod.rs",
        "mod manager_actions;\n",
    );
    case.write(
        "crates/rsid/src/session/tests/manager_actions.rs",
        &(gate("other-03", "own") + "#[path = \"manager_issue_worker.rs\"]\nmod issue_worker;\n"),
    );
    case.write(
        "crates/rsid/src/session/tests/manager_issue_worker.rs",
        &gate("other-03", "w"),
    );
    case.commit();
    case.rebase_base_here();
    case.write(
        "crates/rsid/src/session/tests/manager_issue_worker.rs",
        &(gate("other-03", "w") + "const EDIT: u8 = 1;\n"),
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(
        r.receipt["suggested_filters"],
        json!(["rsid=shard:other-03:test(session::tests::manager_actions::issue_worker)"])
    );
    assert_eq!(r.receipt["dropped_filters"], json!([]));
}

#[test]
fn a_module_with_tests_mounted_by_path_from_its_parent_is_found() {
    let mut case = Case::new();
    case.write("crates/rsid/src/lib.rs", "pub mod session;\n");
    case.write("crates/rsid/src/session/mod.rs", "mod gm;\n");
    case.write(
        "crates/rsid/src/session/gm.rs",
        "pub fn f() {}\n#[cfg(test)]\n#[path = \"gm_tests.rs\"]\nmod tests;\n",
    );
    case.write(
        "crates/rsid/src/session/gm_tests.rs",
        &gate("store-02", "g"),
    );
    case.commit();
    case.rebase_base_here();
    case.write(
        "crates/rsid/src/session/gm.rs",
        "pub fn f() { }\n#[cfg(test)]\n#[path = \"gm_tests.rs\"]\nmod tests;\n",
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(r.receipt["shards"]["compiled_ok"], json!(["store-02"]));
    assert_eq!(
        r.receipt["suggested_filters"],
        json!(["rsid=shard:store-02:test(session::gm)"])
    );
}

#[test]
fn a_filter_that_matches_no_test_is_dropped_and_reported() {
    let mut case = Case::new();
    let helper = "#[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-store-02\"))]\npub fn helper() {}\n";
    case.write("crates/rsid/src/foo.rs", &(gate("store-01", "a") + helper));
    case.commit();
    case.rebase_base_here();
    case.write(
        "crates/rsid/src/foo.rs",
        &(gate("store-01", "a") + helper + "const EDIT: u8 = 1;\n"),
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(
        r.receipt["suggested_filters"],
        json!(["rsid=shard:store-01:test(foo)"])
    );
    let dropped: Vec<_> = r.receipt["dropped_filters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["filter"].clone())
        .collect();
    assert_eq!(dropped, [json!("rsid=shard:store-02:test(foo)")]);
    assert!(
        r.receipt["dropped_filters"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("store-02")
    );
}

#[test]
fn a_module_is_filtered_under_the_package_that_owns_it() {
    let mut case = Case::new();
    let tested = "pub fn f() {}\n#[cfg(test)]\nmod tests { #[test] fn a() {} }\n";
    case.write("crates/rsid-core/src/lib.rs", "pub mod process_control;\n");
    case.write("crates/rsid-core/src/process_control.rs", tested);
    case.write("crates/rsi/src/lib.rs", "pub mod view;\n");
    case.write("crates/rsi/src/view.rs", &tested.replace("f()", "v()"));
    case.commit();
    case.rebase_base_here();
    case.write(
        "crates/rsid-core/src/process_control.rs",
        &tested.replace("f() {}", "f() { }"),
    );
    case.write(
        "crates/rsi/src/view.rs",
        &tested.replace("f() {}", "v() { }"),
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(
        r.receipt["suggested_filters"],
        json!(["rsi=view", "rsid-core=process_control"])
    );
}

// ------------------------------------------------ module sizes, traces and budgets

#[test]
fn filters_name_only_sources_that_hold_a_runnable_test() {
    let mut case = Case::new();
    let mods = [
        "docs_only",
        "string_only",
        "ignored_only",
        "compiled_out",
        "mixed",
        "real",
    ];
    case.write(
        "crates/codegraph/src/lib.rs",
        &mods
            .iter()
            .map(|m| format!("pub mod {m};\n"))
            .collect::<String>(),
    );
    let sources = [
        (
            "docs_only",
            "/// Attribute tails that mark a test (`#[test]`,\n/// `#[tokio::test]`).\npub fn f() {}\n",
        ),
        (
            "string_only",
            "pub const SRC: &str = \"#[test]\\nfn x() {}\";\npub fn f() {}\n",
        ),
        (
            "ignored_only",
            "pub fn f() {}\n#[cfg(test)]\nmod tests {\n    #[test]\n    #[ignore = \"slow\"]\n    fn slow() {}\n}\n",
        ),
        (
            "compiled_out",
            "pub fn f() {}\n#[cfg(any())]\nmod tests {\n    #[test]\n    fn t() {}\n}\n",
        ),
        (
            "mixed",
            "pub fn f() {}\n#[cfg(test)]\nmod tests {\n    #[test]\n    #[ignore = \"slow\"]\n    fn slow() {}\n\n    #[test]\n    fn fast() {}\n}\n",
        ),
        (
            "real",
            "pub fn f() {}\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn a() {}\n}\n",
        ),
    ];
    let targets = [
        (
            "corpus",
            "#[test]\n#[ignore = \"full corpus\"]\nfn s1() {}\n",
        ),
        (
            "docs",
            "//! Run with `cargo test` (a `#[test]` per case).\nfn helper() {}\n",
        ),
        ("live", "#[tokio::test]\nasync fn live() {}\n"),
    ];
    for (module, text) in sources {
        case.write(&format!("crates/codegraph/src/{module}.rs"), text);
    }
    for (target, text) in targets {
        case.write(&format!("crates/codegraph/tests/{target}.rs"), text);
    }
    case.commit();
    case.rebase_base_here();
    for (module, text) in sources {
        case.write(
            &format!("crates/codegraph/src/{module}.rs"),
            &text.replace("pub fn f() {}", "pub fn f() { }"),
        );
    }
    for (target, text) in targets {
        case.write(
            &format!("crates/codegraph/tests/{target}.rs"),
            &format!("{text}const TOUCHED: u8 = 1;\n"),
        );
    }
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    let mut filters = strings(&r.receipt["suggested_filters"]);
    filters.sort();
    assert_eq!(
        filters,
        ["codegraph=mixed", "codegraph=real", "codegraph=test:live"]
    );
}

#[test]
fn public_item_selection_names_only_compiled_tests_in_the_dependent_file() {
    let mut case = Case::new();
    case.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/rsi-common\", \"crates/rsid\"]\n",
    );
    case.write(
        "crates/rsi-common/Cargo.toml",
        "[package]\nname = \"rsi-common\"\nversion = \"0.1.0\"\n",
    );
    case.write("crates/rsid/Cargo.toml", "[package]\nname = \"rsid\"\nversion = \"0.1.0\"\n[dependencies]\nrsi-common = { path = \"../rsi-common\" }\n");
    case.write(
        "crates/rsi-common/src/lib.rs",
        "pub fn selected_item() -> u8 { 1 }\n",
    );
    case.write("crates/rsid/src/lib.rs", "mod foo;\n");
    let enabled = gate("store-02", "enabled");
    let disabled = gate("store-02", "t");
    let indent = |text: &str, pad: &str| {
        text.lines()
            .map(|row| format!("{pad}{row}\n"))
            .collect::<String>()
    };
    case.write(
        "crates/rsid/src/foo.rs",
        &format!(
            "use rsi_common::selected_item;\n{enabled}#[cfg(any())]\nmod disabled {{\n{}}}\nmod outer {{\n    #[cfg(any( ))]\n    mod nested {{\n{}    }}\n}}\n",
            indent(&disabled, "    "),
            indent(&disabled, "        ")
        ),
    );
    case.commit();
    case.rebase_base_here();
    case.write(
        "crates/rsi-common/src/lib.rs",
        "pub fn selected_item() -> u16 { 1 }\n",
    );
    case.commit();
    let r = case.check(&["--filters-only"]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(
        r.receipt["suggested_filters"],
        json!(["rsid=shard:store-02:test(=foo::enabled)"])
    );
}

#[test]
fn a_crate_root_diff_selects_the_roots_own_inline_test_module() {
    let mut case = Case::new();
    let root = "pub mod child;\npub fn f() {}\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn a() { assert_eq!(1, 1); }\n}\n";
    case.write("crates/codegraph/src/lib.rs", root);
    case.write(
        "crates/codegraph/src/child.rs",
        "pub fn c() {}\n#[cfg(test)]\nmod tests { #[test] fn b() {} }\n",
    );
    case.write("crates/plain/src/lib.rs", "pub fn p() {}\n");
    case.commit();
    case.rebase_base_here();
    case.write(
        "crates/codegraph/src/lib.rs",
        &root.replace("assert_eq!(1, 1)", "assert_eq!(2, 2)"),
    );
    case.write("crates/plain/src/lib.rs", "pub fn p() { }\n");
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(r.receipt["suggested_filters"], json!(["codegraph=tests::"]));
}

#[test]
fn a_catalog_change_with_no_catalog_test_pins_nothing() {
    let mut case = Case::new();
    case.write("crates/rsid/src/rpc/agent_x.rs", &gate("other-03", "p"));
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(
        r.receipt["suggested_filters"],
        json!(["rsid=shard:other-03:test(rpc::agent_x)"])
    );
    assert_eq!(r.receipt["dropped_filters"], json!([]));
    assert_eq!(r.receipt["broad"], json!(false));
}

fn session_fixture(case: &mut Case, count: usize) {
    case.write("crates/rsid/src/lib.rs", "pub mod session;\n");
    case.write(
        "crates/rsid/src/session/mod.rs",
        "mod big;\npub const LIMIT: u32 = 1;\npub fn used() {}\npub fn unused() {}\n",
    );
    case.write(
        "crates/rsid/src/session/big.rs",
        &gated_tests(
            "other-03",
            &numbered("t", count),
            &[("t3", "used"), ("t9", "used")],
        ),
    );
    case.commit();
    case.rebase_base_here();
}

#[test]
fn one_function_edit_of_a_big_module_selects_only_the_tests_that_mention_it() {
    let mut case = Case::new();
    session_fixture(&mut case, 70);
    case.write(
        "crates/rsid/src/session/mod.rs",
        "mod big;\npub const LIMIT: u32 = 1;\npub fn used() { }\npub fn unused() {}\n",
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(
        r.receipt["suggested_filters"],
        json!(["rsid=shard:other-03:test(/^session::big::(?:\\w+::)*(?:t3|t9)$/)"])
    );
    assert_eq!(r.receipt["broad"], json!(false));
    assert_eq!(r.receipt["narrowed_filters"][0]["tests"], json!(2));
}

#[test]
fn code_observer_trace_ignores_separate_block_doc_hunks() {
    let mut case = Case::new();
    session_fixture(&mut case, 70);
    let path = "crates/rsid/src/session/mod.rs";
    let original = format!("{}/** old guide\n * old details\n */\n", case.read(path));
    case.write(path, &original);
    case.commit();
    case.rebase_base_here();
    case.write(
        path,
        &original
            .replace("old", "new")
            .replace("fn used() {}", "fn used() { changed(); }"),
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(r.receipt["broad"], json!(false));
    assert_eq!(
        r.receipt["suggested_filters"],
        json!(["rsid=shard:other-03:test(/^session::big::(?:\\w+::)*(?:t3|t9)$/)"])
    );
}

const REGISTRY_BEFORE: &str = "use std::collections::HashMap;\nuse uuid::Uuid;\nmod tests;\npub(crate) struct AgentTokenRegistry {\n    by_session: HashMap<Uuid, String>,\n}\nimpl AgentTokenRegistry {\n    pub(crate) fn insert(&mut self, token: String, id: Uuid) {\n        self.by_session.insert(id, token);\n    }\n}\nfn remint_agent_token(registry: &mut AgentTokenRegistry) {\n    registry.insert(token, id);\n}\n";
const REGISTRY_AFTER: &str = "use std::collections::HashMap;\nuse uuid::Uuid;\nmod tests;\nmod agent_token_registry {\n    use std::collections::HashMap;\n    use uuid::Uuid;\n    #[derive(Debug, Clone, Copy)]\n    pub(crate) struct AgentTransportWitness {\n        incarnation_id: Uuid,\n    }\n    #[derive(Debug)]\n    struct AgentTokenBinding {\n        token: String,\n        witness: AgentTransportWitness,\n    }\n    pub(crate) struct AgentTokenRegistry {\n        by_session: HashMap<Uuid, AgentTokenBinding>,\n    }\n    impl AgentTokenRegistry {\n        #[cfg(test)]\n        pub(crate) fn insert(&mut self, token: String, id: Uuid) {\n            self.insert_binding(token, id);\n        }\n        fn insert_binding(&mut self, token: String, id: Uuid) {\n            let binding = AgentTokenBinding { token, witness };\n            self.by_session.insert(id, binding);\n        }\n        pub(super) fn remint(&mut self, token: String, id: Uuid) {\n            self.insert_binding(token, id);\n        }\n        pub(crate) fn binding_for_session(&self, id: Uuid) -> AgentTransportWitness {\n            self.by_session.get(id).witness\n        }\n    }\n}\npub(crate) use agent_token_registry::AgentTokenRegistry;\nfn remint_agent_token(registry: &mut AgentTokenRegistry) {\n    registry.remint(token, id);\n}\n";

fn private_registry_fixture(extra: &str, helper: bool, observer: bool) -> Checked {
    let mut case = Case::new();
    let path = "crates/rsid/src/session/mod.rs";
    case.write("crates/rsid/src/lib.rs", "pub mod session;\n");
    case.write(path, REGISTRY_BEFORE);
    if observer {
        case.write(
            "crates/rsid/src/session/observer.rs",
            &format!(
                "struct RegistryObserver {{\n    registry: AgentTokenRegistry,\n}}\nimpl RegistryObserver {{\n    fn reconcile(&self) {{\n        self.registry.binding_for_session(id);\n    }}\n}}\n{}",
                with_body(&gate("session-04", "registry_observer"), "RegistryObserver::new().reconcile();")
            ),
        );
        case.write(
            "crates/rsid/src/session/unrelated.rs",
            &format!(
                "struct OtherObserver {{\n    value: String,\n}}\nimpl OtherObserver {{\n    fn reconcile(&self) {{\n        other_work();\n    }}\n}}\n{}",
                with_body(&gate("session-04", "other_observer"), "OtherObserver::new().reconcile();")
            ),
        );
    }
    let mut tests: String = [
        (
            "registry_bijective",
            "AgentTokenRegistry::default().insert(token, id);",
        ),
        (
            "registry_remint",
            "AgentTokenRegistry::default().insert(token, id);",
        ),
        ("registry_witness", "registry.binding_for_session(id);"),
        ("registry_cleanup", "registry.binding_for_session(id);"),
        (
            "continue_registers_working_agent_token",
            "remint_agent_token(registry);",
        ),
        (
            "remint_revokes_prior_token",
            "remint_agent_token(registry);",
        ),
    ]
    .iter()
    .map(|(name, body)| with_body(&gate("session-03", name), body))
    .collect();
    let unrelated = numbered("unrelated_", 70);
    let calls: Vec<(&str, &str)> = unrelated
        .iter()
        .map(|n| (n.as_str(), "unrelated_insert"))
        .collect();
    tests += &gated_tests("session-03", &unrelated, &calls);
    tests += "fn unrelated_insert() {\n    map.insert(token, witness);\n}\n";
    if helper {
        tests = tests.replacen(
            "AgentTokenRegistry::default().insert(token, id);",
            "seed_registry();",
            1,
        );
        tests += "fn seed_registry() {\n    AgentTokenRegistry::default().insert(token, id);\n}\n";
    }
    case.write("crates/rsid/src/session/tests.rs", &tests);
    case.commit();
    case.rebase_base_here();
    case.write(path, &format!("{REGISTRY_AFTER}{extra}"));
    case.commit();
    case.check(&["--filters-only"])
}

#[test]
fn private_registry_move_selects_its_six_real_users() {
    let r = private_registry_fixture("", false, false);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(
        r.receipt["broad"],
        json!(false),
        "{}",
        r.receipt["broad_filters"]
    );
    assert_eq!(
        r.receipt["suggested_filters"],
        json!([
            "rsid=shard:session-03:test(/^session::tests::(?:\\w+::)*(?:continue_registers_working_agent_token|registry_bijective|registry_cleanup|registry_remint|registry_witness|remint_revokes_prior_token)$/)"
        ])
    );
}

#[test]
fn private_test_method_users_trace_through_a_fixture_helper() {
    let r = private_registry_fixture("", true, false);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(r.receipt["broad"], json!(false));
    assert!(
        strings(&r.receipt["suggested_filters"])
            .join(" ")
            .contains("registry_bijective")
    );
}

#[test]
fn private_member_user_traces_its_receiver_and_cross_module_test() {
    let r = private_registry_fixture("", false, true);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(r.receipt["broad"], json!(false));
    let filters = strings(&r.receipt["suggested_filters"]);
    assert!(
        filters.contains(
            &"rsid=shard:session-04:test(/^session::observer::(?:\\w+::)*registry_observer$/)"
                .to_owned()
        ),
        "{filters:?}"
    );
    assert_eq!(filters.len(), 2);
}

#[test]
fn private_move_with_an_unrelated_const_keeps_the_broad_fallback() {
    let r = private_registry_fixture("pub const LIMIT: u32 = 2;\n", false, false);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(r.receipt["broad"], json!(true));
    assert_eq!(
        r.receipt["suggested_filters"],
        json!(["rsid=shard:session-03:test(session)"])
    );
}

#[test]
fn a_function_no_test_mentions_selects_no_test_and_says_so() {
    let mut case = Case::new();
    session_fixture(&mut case, 70);
    case.write(
        "crates/rsid/src/session/mod.rs",
        "mod big;\npub const LIMIT: u32 = 1;\npub fn used() {}\npub fn unused() { }\n",
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(r.receipt["suggested_filters"], json!([]));
    assert_eq!(r.receipt["narrowed_filters"][0]["tests"], json!(0));
    assert!(
        r.receipt["narrowed_filters"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("compile check")
    );
    assert_eq!(r.receipt["shards"]["compiled_ok"], json!(["other-03"]));
}

#[test]
fn an_untraceable_edit_of_a_big_module_is_flagged_broad() {
    let mut case = Case::new();
    session_fixture(&mut case, 70);
    case.write(
        "crates/rsid/src/session/mod.rs",
        "mod big;\npub const LIMIT: u32 = 2;\npub fn used() {}\npub fn unused() {}\n",
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(
        r.receipt["suggested_filters"],
        json!(["rsid=shard:other-03:test(session)"])
    );
    assert_eq!(r.receipt["broad"], json!(true));
    assert_eq!(
        r.receipt["broad_filters"][0]["filter"],
        json!("rsid=shard:other-03:test(session)")
    );
    assert_eq!(r.receipt["broad_filters"][0]["tests"], json!(70));
    assert!(format!("{} ", r.summary).contains(" BROAD "));
}

fn wide_module_fixture(per_shard: usize) -> Case {
    let mut case = Case::new();
    case.write("crates/rsid/src/lib.rs", "pub mod session;\n");
    case.write(
        "crates/rsid/src/session/mod.rs",
        "mod big;\nmod wide;\npub const LIMIT: u32 = 1;\n",
    );
    case.write(
        "crates/rsid/src/session/big.rs",
        &gated_tests("other-03", &numbered("a", per_shard), &[]),
    );
    case.write(
        "crates/rsid/src/session/wide.rs",
        &gated_tests("session-01", &numbered("b", per_shard), &[]),
    );
    case.commit();
    case.rebase_base_here();
    case.write(
        "crates/rsid/src/session/mod.rs",
        "mod big;\nmod wide;\npub const LIMIT: u32 = 2;\n",
    );
    case.commit();
    case
}

#[test]
fn a_module_filter_is_sized_by_every_shard_the_real_build_runs() {
    let case = wide_module_fixture(40);
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(
        r.receipt["suggested_filters"],
        json!(["rsid=shard:other-03:test(session)"])
    );
    assert_eq!(r.receipt["broad"], json!(true));
    let broad: Vec<_> = r.receipt["broad_filters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| (b["filter"].clone(), b["tests"].clone()))
        .collect();
    assert_eq!(
        broad,
        [(json!("rsid=shard:other-03:test(session)"), json!(80))]
    );
}

#[test]
fn a_module_whose_shards_together_stay_small_is_not_flagged() {
    let case = wide_module_fixture(20);
    let r = case.check(&[]);
    assert_eq!(
        r.receipt["suggested_filters"],
        json!(["rsid=shard:other-03:test(session)"])
    );
    assert_eq!(r.receipt["broad"], json!(false));
}

fn anchored_module_fixture(case: &mut Case) {
    case.write(
        "crates/rsid/src/lib.rs",
        "pub mod deploy;\npub mod bridge;\n",
    );
    case.write(
        "crates/rsid/src/deploy.rs",
        &format!(
            "pub const EDIT: u8 = 1;\n{}",
            gated_tests("other-02", &["a".into(), "b".into()], &[])
        ),
    );
    case.write(
        "crates/rsid/src/bridge.rs",
        &gated_tests("store-01", &["v95_deployed_bridge_rolls_back".into()], &[]),
    );
    case.commit();
    case.rebase_base_here();
    case.write(
        "crates/rsid/src/deploy.rs",
        &format!(
            "pub const EDIT: u8 = 2;\n{}",
            gated_tests("other-02", &["a".into(), "b".into()], &[])
        ),
    );
    case.commit();
}

#[test]
fn a_module_filter_that_also_matches_unrelated_test_names_is_anchored() {
    let mut case = Case::new();
    anchored_module_fixture(&mut case);
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(
        r.receipt["suggested_filters"],
        json!(["rsid=shard:other-02:test(/^deploy::/)"])
    );
    assert_eq!(r.receipt["dropped_filters"], json!([]));
    assert_eq!(r.receipt["broad"], json!(false));
    let anchored = &r.receipt["narrowed_filters"][0];
    assert_eq!(
        (&anchored["module"], &anchored["filter"]),
        (&json!("deploy"), &json!("test(/^deploy::/)"))
    );
    assert!(
        anchored["reason"]
            .as_str()
            .unwrap()
            .contains("bridge::v95_deployed_bridge_rolls_back")
    );
}

#[test]
fn an_anchored_module_filter_is_sized_by_its_own_tree() {
    let mut case = Case::new();
    case.write(
        "crates/rsid/src/lib.rs",
        "pub mod deploy;\npub mod bridge;\n",
    );
    case.write(
        "crates/rsid/src/deploy.rs",
        &format!(
            "pub const EDIT: u8 = 1;\n{}",
            gated_tests("other-02", &numbered("d", 40), &[])
        ),
    );
    case.write(
        "crates/rsid/src/bridge.rs",
        &gated_tests("store-01", &numbered("v95_deployed_", 40), &[]),
    );
    case.commit();
    case.rebase_base_here();
    case.write(
        "crates/rsid/src/deploy.rs",
        &format!(
            "pub const EDIT: u8 = 2;\n{}",
            gated_tests("other-02", &numbered("d", 40), &[])
        ),
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(
        r.receipt["suggested_filters"],
        json!(["rsid=shard:other-02:test(/^deploy::/)"])
    );
    assert_eq!(
        r.receipt["broad"],
        json!(false),
        "{}",
        r.receipt["broad_filters"]
    );
}

#[test]
fn topology_verify_filter_selects_its_full_module_path_and_descendants() {
    let mut case = Case::new();
    case.write(
        "crates/rsid/src/topology/verify.rs",
        &format!(
            "pub const EDIT: u8 = 1;\n{}",
            gated_tests("other-02", &["judge_test".into()], &[])
        ),
    );
    case.write(
        "crates/rsid/src/topology/verify/tests.rs",
        &gated_tests("other-02", &["nested_test".into()], &[]),
    );
    for module in [
        "topology/verify_extra",
        "session/topology/verify",
        "store/cold_archive/tests",
    ] {
        case.write(
            &format!("crates/rsid/src/{module}.rs"),
            &gated_tests("other-02", &["verify".into()], &[]),
        );
    }
    case.commit();
    case.rebase_base_here();
    case.write(
        "crates/rsid/src/topology/verify.rs",
        &format!(
            "pub const EDIT: u8 = 2;\n{}",
            gated_tests("other-02", &["judge_test".into()], &[])
        ),
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(
        r.receipt["suggested_filters"],
        json!(["rsid=shard:other-02:test(/^topology::verify::/)"])
    );
    assert_eq!(r.receipt["narrowed_filters"][0]["tests"], json!(2));
}

#[test]
fn anchored_module_filters_cover_their_own_subtree_filters() {
    let anchored = "rsid=shard:other-02:test(/^deploy::/)".to_owned();
    let inner = "rsid=shard:other-02:test(deploy::tests)".to_owned();
    let named = "rsid=shard:other-02:test(/^deploy::(?:\\w+::)*retry$/)".to_owned();
    let elsewhere = "rsid=shard:other-02:test(satellite::deploy)".to_owned();
    let covered = super::filters::covered_filters(
        &[inner, named, anchored.clone(), elsewhere.clone()],
        &mut Vec::new(),
    );
    assert_eq!(covered, [anchored, elsewhere]);
}

#[test]
fn a_small_module_keeps_its_plain_module_filter() {
    let mut case = Case::new();
    session_fixture(&mut case, 5);
    case.write(
        "crates/rsid/src/session/mod.rs",
        "mod big;\npub const LIMIT: u32 = 2;\npub fn used() {}\npub fn unused() {}\n",
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(
        r.receipt["suggested_filters"],
        json!(["rsid=shard:other-03:test(session)"])
    );
    assert_eq!(r.receipt["broad"], json!(false));
    assert_eq!(r.receipt["narrowed_filters"], json!([]));
}

const LEDGER_ROOT: &str = "crates/rsid-store/src/store/manager_ledger.rs";
const LEDGER_ORIGINAL: &str = "mod review_source;\nmod tests;\npub struct LedgerObservation {\n    pub source_commit: Option<String>,\n}\n";

/// #1548: a .rs parent with 90 ledger tests and cross-module consumers.
fn ledger_observation_fixture(case: &mut Case) {
    case.write("crates/rsid-store/src/lib.rs", "pub mod store;\n");
    case.write(
        "crates/rsid-store/src/store/mod.rs",
        "pub mod manager_ledger;\n",
    );
    case.write(LEDGER_ROOT, LEDGER_ORIGINAL);
    case.write(
        "crates/rsid-store/src/store/manager_ledger/tests.rs",
        &gated_tests("store-04", &numbered("ledger_", 90), &[]),
    );
    case.write(
        "crates/rsid-store/src/store/manager_ledger/review_source.rs",
        "pub struct ReclaimedReviewSource {}\npub fn manager_review_reclaimed_source() -> Option<ReclaimedReviewSource> {\n    None\n}\n",
    );
    case.write(
        "crates/rsid/src/session/review_source.rs",
        &format!(
            "fn review_proof() {{\n    manager_review_reclaimed_source();\n}}\n{}",
            gated_tests(
                "session-05",
                &["source_custody".into(), "retry".into(), "unrelated".into()],
                &[
                    ("source_custody", "review_proof"),
                    ("retry", "review_proof")
                ]
            )
        ),
    );
    case.commit();
    case.rebase_base_here();
}

#[test]
fn ledger_field_and_helper_export_trace_source_custody_and_retry() {
    let mut case = Case::new();
    ledger_observation_fixture(&mut case);
    case.write(
        LEDGER_ROOT,
        &LEDGER_ORIGINAL
            .replace("mod tests;", "pub use review_source::ReclaimedReviewSource;\nmod tests;")
            .replace("    pub source_commit", "    pub reclaimed_review_source: Option<ReclaimedReviewSource>,\n    pub source_commit"),
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(
        r.receipt["suggested_filters"],
        json!([
            "rsid=shard:session-05:test(/^session::review_source::(?:\\w+::)*(?:retry|source_custody)$/)"
        ])
    );
    assert_eq!(r.receipt["broad"], json!(false));
    assert_eq!(r.receipt["narrowed_filters"][0]["tests"], json!(2));
}

#[test]
fn field_replacement_traces_both_old_and_new_consumers() {
    let mut case = Case::new();
    ledger_observation_fixture(&mut case);
    case.write(
        LEDGER_ROOT,
        &LEDGER_ORIGINAL.replace("source_commit", "old_proof"),
    );
    case.write(
        "crates/rsid/src/session/review_source.rs",
        &gated_tests(
            "session-05",
            &["old_reader".into(), "new_reader".into()],
            &[("old_reader", "old_proof"), ("new_reader", "new_proof")],
        ),
    );
    case.commit();
    case.rebase_base_here();
    case.write(
        LEDGER_ROOT,
        &LEDGER_ORIGINAL.replace("source_commit", "new_proof"),
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(
        r.receipt["suggested_filters"],
        json!([
            "rsid=shard:session-05:test(/^session::review_source::(?:\\w+::)*(?:new_reader|old_reader)$/)"
        ])
    );
}

#[test]
fn common_field_name_retains_the_broad_fallback() {
    let mut case = Case::new();
    ledger_observation_fixture(&mut case);
    let readers = numbered("reader_", 11);
    let calls: Vec<(&str, &str)> = readers
        .iter()
        .map(|n| (n.as_str(), "common_field"))
        .collect();
    case.write(
        "crates/rsid/src/session/readers.rs",
        &gated_tests("session-05", &readers, &calls),
    );
    case.commit();
    case.rebase_base_here();
    case.write(
        LEDGER_ROOT,
        &LEDGER_ORIGINAL.replace(
            "    pub source_commit",
            "    pub common_field: String,\n    pub source_commit",
        ),
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(
        r.receipt["suggested_filters"],
        json!(["rsid=shard:store-04:test(store::manager_ledger)"])
    );
    assert_eq!(r.receipt["broad"], json!(true));
}

#[test]
fn duplicate_expression_keeps_different_library_package_coverage() {
    let first = "rsid=shard:session-03:test(rotation)".to_owned();
    let same = first.replace("session-03", "session-04");
    let other = first.replace("session-03", "store-01");
    let packages: BTreeMap<String, Vec<String>> = BTreeMap::from([
        ("session-03".into(), vec!["rsid".into()]),
        ("session-04".into(), vec!["rsid".into()]),
        ("store-01".into(), vec!["rsid-store".into()]),
    ]);
    let kept = super::filters::unsharded_filters(
        &[first.clone(), same, other.clone()],
        &packages,
        &mut Vec::new(),
    );
    assert_eq!(kept, [first, other]);
}

#[test]
fn aggregate_gate_budget_traces_small_modules_and_merges_coverage() {
    let mut case = Case::new();
    ledger_observation_fixture(&mut case);
    for i in 0..13 {
        case.write(
            &format!("crates/rsid/src/small_{i:02}.rs"),
            &format!(
                "pub fn unobserved_{i}() {{}}\n{}",
                gate("other-03", &format!("t{i}"))
            ),
        );
    }
    case.commit();
    case.rebase_base_here();
    for i in 0..13 {
        case.write(
            &format!("crates/rsid/src/small_{i:02}.rs"),
            &format!(
                "pub fn unobserved_{i}() {{ }}\n{}",
                gate("other-03", &format!("t{i}"))
            ),
        );
    }
    case.write(
        LEDGER_ROOT,
        &LEDGER_ORIGINAL.replace(
            "mod tests;",
            "pub use review_source::ReclaimedReviewSource;\nmod tests;",
        ),
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(r.receipt["suggested_filters"].as_array().unwrap().len(), 12);
    let last = r.receipt["narrowed_filters"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(last["tests"], json!(0));
    assert!(
        last["reason"]
            .as_str()
            .unwrap()
            .contains("aggregate gate budget")
    );
}

#[test]
fn uncertain_aggregate_coverage_can_exceed_the_budget() {
    let mut case = Case::new();
    for i in 0..13 {
        case.write(
            &format!("crates/rsid/src/small_{i:02}.rs"),
            &format!(
                "pub const LIMIT: u32 = 1;\n{}",
                gate("other-03", &format!("t{i}"))
            ),
        );
    }
    case.commit();
    case.rebase_base_here();
    for i in 0..13 {
        case.write(
            &format!("crates/rsid/src/small_{i:02}.rs"),
            &format!(
                "pub const LIMIT: u32 = 2;\n{}",
                gate("other-03", &format!("t{i}"))
            ),
        );
    }
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    let expected: Vec<_> = (0..13)
        .map(|i| format!("rsid=shard:other-03:test(small_{i:02})"))
        .collect();
    assert_eq!(strings(&r.receipt["suggested_filters"]), expected);
}

#[test]
fn filter_merging_preserves_shards_and_regex_coverage() {
    let broad = "rsid=shard:session-05:test(session::review)".to_owned();
    let child = "rsid=shard:session-05:test(session::review::source)".to_owned();
    let named = "rsid=shard:session-05:test(/^session::review::(?:\\w+::)*retry$/)".to_owned();
    let other_shard = named.replace("session-05", "session-01");
    let other_module = named.replace("session::review", "session::custody");
    let merged = super::filters::covered_filters(
        &[
            child,
            named,
            other_shard.clone(),
            broad.clone(),
            broad.clone(),
            other_module.clone(),
        ],
        &mut Vec::new(),
    );
    assert_eq!(merged, [other_shard, broad, other_module]);
}

#[test]
fn named_filter_chunks_preserve_all_observers_within_256_characters() {
    let names: Vec<String> = (0..3)
        .map(|i| format!("source_custody_retry_{i}_{}", "x".repeat(65)))
        .collect();
    let picked: Vec<_> = names
        .iter()
        .map(|n| super::filters::Observer {
            module: "session::review".into(),
            name: n.clone(),
            shard: "session-05".into(),
        })
        .collect();
    let filters = super::filters::traced_filters(&picked);
    assert_eq!(filters.len(), 2);
    assert!(filters.iter().all(|f| f.len() <= 256));
    assert!(
        names
            .iter()
            .all(|n| filters.iter().any(|f| f.contains(n.as_str())))
    );
}

fn manager_actions_fixture(case: &mut Case, own_tests: &[String]) {
    case.write("crates/rsid/src/lib.rs", "pub mod session;\n");
    case.write("crates/rsid/src/session/mod.rs", "mod tests;\n");
    case.write(
        "crates/rsid/src/session/tests/mod.rs",
        "mod manager_actions;\n",
    );
    case.write(
        "crates/rsid/src/session/tests/manager_actions.rs",
        &format!(
            "{}#[path = \"manager_issue_worker.rs\"]\nmod issue_worker;\n",
            gated_tests("other-03", own_tests, &[])
        ),
    );
    case.write(
        "crates/rsid/src/session/tests/manager_issue_worker.rs",
        &gated_tests("other-03", &["w1".into(), "w2".into()], &[]),
    );
    case.commit();
    case.rebase_base_here();
}

#[test]
fn a_test_module_with_submodules_selects_only_its_own_tests() {
    let mut case = Case::new();
    manager_actions_fixture(&mut case, &["a1".into(), "a2".into()]);
    case.write(
        "crates/rsid/src/session/tests/manager_actions.rs",
        &format!(
            "{}#[path = \"manager_issue_worker.rs\"]\nmod issue_worker;\nconst EDIT: u8 = 1;\n",
            gated_tests("other-03", &["a1".into(), "a2".into()], &[])
        ),
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(
        r.receipt["suggested_filters"],
        json!(["rsid=shard:other-03:test(/^session::tests::manager_actions::[A-Za-z0-9_]+$/)"])
    );
    assert_eq!(
        r.receipt["narrowed_filters"][0]["excludes"],
        json!(["session::tests::manager_actions::issue_worker"])
    );
    assert_eq!(r.receipt["dropped_filters"], json!([]));
}

#[test]
fn a_test_module_whose_tests_all_live_in_submodules_keeps_the_prefix() {
    let mut case = Case::new();
    manager_actions_fixture(&mut case, &[]);
    case.write(
        "crates/rsid/src/session/tests/manager_actions.rs",
        "#[path = \"manager_issue_worker.rs\"]\nmod issue_worker;\nconst EDIT: u8 = 1;\n",
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(
        r.receipt["suggested_filters"],
        json!(["rsid=shard:other-03:test(session::tests::manager_actions)"])
    );
}

fn rpc_dispatch_text(arms: &str) -> String {
    format!(
        "mod tests;\nimpl RpcServer {{\n    fn dispatch(&self) {{\n        match request.method.as_str() {{\n{arms}        }}\n    }}\n}}\n"
    )
}

#[test]
fn a_rpc_change_pins_the_catalog_tests_by_name_not_the_rpc_tests_module() {
    let mut case = Case::new();
    let arms: String = (0..70)
        .map(|n| format!("            \"Verb{n}\" => self.handle_{n}(),\n"))
        .collect();
    let rpc = rpc_dispatch_text(&arms);
    case.write("crates/rsid/src/lib.rs", "pub mod rpc;\n");
    case.write("crates/rsid/src/rpc.rs", &rpc);
    let mut names = numbered("t", 70);
    names.extend([
        "login_is_operator_only".into(),
        "verbs_are_cataloged".into(),
    ]);
    case.write(
        "crates/rsid/src/rpc/tests.rs",
        &(gated_tests("other-03", &names, &[])
            + &gate("other-03", "export_thing_is_refused")
                .replace("{}", "{ let _ = \"ExportThing\"; }")),
    );
    case.commit();
    case.rebase_base_here();
    case.write(
        "crates/rsid/src/rpc.rs",
        &rpc.replace("        }\n    }\n}\n", "            \"ExportThing\" => self.handle_export(),\n            // AGENT_VERBS stays unchanged\n        }\n    }\n}\n"),
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(
        filter_set(&r),
        set_of(&[
            "rsid=shard:other-03:test(/^rpc::tests::(?:\\w+::)*export_thing_is_refused$/)",
            "rsid=shard:other-03:test(/^rpc::tests::(?:\\w+::)*(?:login_is_operator_only|verbs_are_cataloged)$/)"
        ])
    );
    assert_eq!(r.receipt["broad"], json!(false));
}

#[test]
fn catalog_pins_are_split_so_every_suggested_filter_fits_the_job_wire_cap() {
    let mut case = Case::new();
    let rpc = rpc_dispatch_text("");
    case.write("crates/rsid/src/lib.rs", "pub mod rpc;\n");
    case.write("crates/rsid/src/rpc.rs", &rpc);
    let names: Vec<String> = (0..56)
        .map(|n| format!("agent_verb_catalog_pin_{n:02}_{}", "x".repeat(40)))
        .collect();
    case.write(
        "crates/rsid/src/rpc/tests.rs",
        &gated_tests("other-01", &names, &[]),
    );
    case.commit();
    case.rebase_base_here();
    case.write(
        "crates/rsid/src/rpc.rs",
        &rpc.replace("        }\n    }\n}\n", "            \"ExportThing\" => self.handle_export(),\n            // AGENT_VERBS stays unchanged\n        }\n    }\n}\n"),
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    let filters = strings(&r.receipt["suggested_filters"]);
    let pins: Vec<_> = filters
        .iter()
        .filter(|f| f.contains("agent_verb_catalog_pin"))
        .collect();
    assert!(pins.len() > 1);
    assert!(
        filters.iter().all(|f| f.len() <= 256),
        "{:?}",
        filters.iter().map(String::len).collect::<Vec<_>>()
    );
    assert!(
        pins.iter()
            .all(|f| f.starts_with("rsid=shard:other-01:test(/^rpc::tests::(?:\\w+::)*")),
        "{pins:?}"
    );
    for name in &names {
        assert_eq!(
            pins.iter().filter(|f| f.contains(name.as_str())).count(),
            1,
            "{name}"
        );
    }
}

#[test]
fn a_pin_that_cannot_be_split_below_the_wire_cap_is_named() {
    let mut case = Case::new();
    let rpc = rpc_dispatch_text("");
    case.write("crates/rsid/src/lib.rs", "pub mod rpc;\n");
    case.write("crates/rsid/src/rpc.rs", &rpc);
    let name = format!("agent_verb_catalog_{}", "y".repeat(260));
    case.write(
        "crates/rsid/src/rpc/tests.rs",
        &gated_tests("other-01", &[name.clone()], &[]),
    );
    case.commit();
    case.rebase_base_here();
    case.write(
        "crates/rsid/src/rpc.rs",
        &rpc.replace("        }\n    }\n}\n", "            \"ExportThing\" => self.handle_export(),\n            // AGENT_VERBS stays unchanged\n        }\n    }\n}\n"),
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(r.receipt["oversize_filters"].as_array().unwrap().len(), 1);
    assert!(
        r.receipt["oversize_filters"][0]
            .as_str()
            .unwrap()
            .contains(&name)
    );
}

// ------------------------------------------------------ declared guards and pins

const RSID_MANIFEST: &str = "[features]\ndefault = []\ntest-shard-mode = []\ntest-shard-store-02 = [\"test-shard-mode\"]\ntest-shard-other-03 = [\"test-shard-mode\"]\n";
const STORE_MANIFEST: &str = "[features]\ndefault = []\ntest-shard-mode = []\ntest-shard-store-01 = [\"test-shard-mode\"]\ntest-shard-store-02 = [\"test-shard-mode\"]\n";
const GUARD_TABLE: &str = "tools/test-guards.json";

fn repo_tool(name: &str) -> String {
    std::fs::read_to_string(
        std::path::Path::new(rsi_common::manifest_dir!())
            .join("../../tools")
            .join(name),
    )
    .unwrap()
}

/// #3076: both changed inputs and the external observer are in the split store crate.
fn manager_spend_observer_fixture(with_test: bool) -> (Case, &'static str, String) {
    let mut case = Case::new();
    case.manifests = true;
    let src = "crates/rsid-store/src/";
    case.write("crates/rsid/Cargo.toml", RSID_MANIFEST);
    case.write("crates/rsid-store/Cargo.toml", STORE_MANIFEST);
    case.write(
        &format!("{src}lib.rs"),
        "pub mod model_control;\npub mod store;\n",
    );
    case.write(&format!("{src}model_control/mod.rs"), "mod cost;\n");
    case.write(
        &format!("{src}store/mod.rs"),
        "mod manager_resources;\nmod manager_intent;\nmod unrelated;\nmod portfolio_nodes;\n",
    );
    case.write(
        &format!("{src}store/portfolio_nodes.rs"),
        "#[cfg(test)]\n#[path = \"portfolio_delegation_tests.rs\"]\nmod delegation_tests;\n",
    );
    let name = "a_manager_pass_costs_a_bounded_number_of_instructions_per_member_2494";
    case.write(
        &format!("{src}store/portfolio_delegation_tests.rs"),
        &gated_tests(
            "store-01",
            &[(if with_test {
                name
            } else {
                "other_delegation_test"
            })
            .to_owned()],
            &[],
        ),
    );
    for path in [
        "model_control/cost.rs",
        "store/manager_resources.rs",
        "store/manager_intent.rs",
        "store/unrelated.rs",
    ] {
        case.write(
            &format!("{src}{path}"),
            &format!(
                "pub fn spend() -> u8 {{ 1 }}\n{}",
                gated_tests("store-01", &["own_spend_test".into()], &[])
            ),
        );
    }
    let intent = format!("{src}store/manager_intent.rs");
    let text = format!("{}mod detail;\n", case.read(&intent));
    case.write(&intent, &text);
    case.write(
        &format!("{src}store/manager_intent/detail.rs"),
        "pub fn detail() { 1; }\n",
    );
    case.write(GUARD_TABLE, &repo_tool("test-guards.json"));
    case.commit();
    case.rebase_base_here();
    (case, name, src.to_owned())
}

#[test]
fn each_manager_spend_input_selects_the_external_delegation_cost_bound() {
    for paths in [
        vec!["model_control/cost.rs"],
        vec!["store/manager_resources.rs"],
        vec!["store/manager_intent.rs"],
        vec!["model_control/cost.rs", "store/manager_resources.rs"],
    ] {
        let (mut case, name, src) = manager_spend_observer_fixture(true);
        for path in &paths {
            case.write(
                &format!("{src}{path}"),
                &format!(
                    "pub fn spend() -> u8 {{ 2 }}\n{}",
                    gated_tests("store-01", &["own_spend_test".into()], &[])
                ),
            );
        }
        case.commit();
        let r = case.check(&["--filters-only"]);
        assert_eq!(r.code, 0, "{}", r.summary);
        let full = format!("store::portfolio_nodes::delegation_tests::{name}");
        let pin = format!("rsid=shard:store-01:test(={full})");
        let filters = strings(&r.receipt["suggested_filters"]);
        assert_eq!(
            filters.iter().filter(|f| **f == pin).count(),
            1,
            "{filters:?}"
        );
        let mut touched: Vec<_> = paths.iter().map(|p| format!("{src}{p}")).collect();
        touched.sort();
        assert_eq!(
            r.receipt["declared_guards"],
            json!([{"package": "rsid-store", "test": full, "paths": touched, "filters": [pin]}])
        );
        for path in &paths {
            let module = path.trim_end_matches(".rs").replace('/', "::");
            assert!(
                filters.contains(&format!("rsid=shard:store-01:test({module})")),
                "{module} {filters:?}"
            );
        }
    }
}

#[test]
fn an_unrelated_module_keeps_its_own_coverage_without_a_declared_guard() {
    let (mut case, _, src) = manager_spend_observer_fixture(true);
    case.write(
        &format!("{src}store/unrelated.rs"),
        &format!(
            "pub fn spend() -> u8 {{ 2 }}\n{}",
            gated_tests("store-01", &["own_spend_test".into()], &[])
        ),
    );
    case.commit();
    let r = case.check(&["--filters-only"]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(r.receipt["declared_guards"], json!([]));
    assert_eq!(
        r.receipt["suggested_filters"],
        json!(["rsid=shard:store-01:test(store::unrelated)"])
    );
}

#[test]
fn guard_declarations_fail_loudly_instead_of_publishing_partial_filters() {
    let (case, _, _) = manager_spend_observer_fixture(true);
    let original: Value = serde_json::from_str(&case.read(GUARD_TABLE)).unwrap();
    let mut cases: Vec<(String, &str)> = vec![
        ("{".into(), "EOF while parsing"),
        (
            json!({"version": 2, "guards": []}).to_string(),
            "expected version: 1",
        ),
        (
            json!({"version": 1, "guards": [null]}).to_string(),
            "expected package, test and modules",
        ),
    ];
    for (key, value, error) in [
        ("package", json!("missing-crate"), "package must be"),
        (
            "modules",
            json!(["store::missing"]),
            "unknown file-backed module",
        ),
        ("modules", json!([]), "nonempty modules array"),
        ("modules", json!([123]), "module names"),
        (
            "test",
            json!("store::portfolio_nodes::delegation_tests::missing"),
            "unknown or non-runnable",
        ),
        ("test", json!("bad test!"), "full test name"),
        ("typo", json!("value"), "expected package, test and modules"),
    ] {
        let mut config = original.clone();
        config["guards"][0][key] = value;
        cases.push((config.to_string(), error));
    }
    for (table, error) in cases {
        let (mut case, _, _) = manager_spend_observer_fixture(true);
        case.write(GUARD_TABLE, &table);
        case.commit();
        let r = case.check(&["--filters-only", "--json"]);
        assert_eq!(r.code, 2, "{error}: {}", r.summary);
        assert!(
            r.summary.contains(&format!("{GUARD_TABLE}:")),
            "{}",
            r.summary
        );
        assert!(r.summary.contains(error), "{error}: {}", r.summary);
        assert_eq!(r.receipt, Value::Null);
    }
}

#[test]
fn guard_validation_checks_the_test_even_on_an_unrelated_diff() {
    let (mut case, name, src) = manager_spend_observer_fixture(false);
    case.write(
        &format!("{src}store/unrelated.rs"),
        "pub fn spend() -> u8 { 2 }\n",
    );
    case.commit();
    let r = case.check(&["--filters-only", "--json"]);
    assert_eq!(r.code, 2, "{}", r.summary);
    assert!(
        r.summary.contains(&format!("unknown or non-runnable sharded test rsid-store::store::portfolio_nodes::delegation_tests::{name}")),
        "{}",
        r.summary
    );
}

#[test]
fn guard_observers_are_selected_for_comments_descendants_and_table_edits() {
    for change in ["comment", "descendant", "table"] {
        let (mut case, name, src) = manager_spend_observer_fixture(true);
        match change {
            "comment" => {
                let path = format!("{src}store/manager_intent.rs");
                let text = format!("// updated explanation\n{}", case.read(&path));
                case.write(&path, &text);
            }
            "descendant" => case.write(
                &format!("{src}store/manager_intent/detail.rs"),
                "pub fn detail() { 2; }\n",
            ),
            _ => {
                let table: Value = serde_json::from_str(&case.read(GUARD_TABLE)).unwrap();
                let mut buffer = Vec::new();
                let formatter = serde_json::ser::PrettyFormatter::with_indent(b"    ");
                serde::Serialize::serialize(
                    &table,
                    &mut serde_json::Serializer::with_formatter(&mut buffer, formatter),
                )
                .unwrap();
                case.write(
                    GUARD_TABLE,
                    &format!("{}\n", String::from_utf8(buffer).unwrap()),
                );
            }
        }
        case.commit();
        let r = case.check(&["--filters-only"]);
        assert_eq!(r.code, 0, "{change}: {}", r.summary);
        assert!(
            strings(&r.receipt["suggested_filters"]).contains(&format!(
                "rsid=shard:store-01:test(=store::portfolio_nodes::delegation_tests::{name})"
            )),
            "{change}"
        );
        assert_eq!(
            r.receipt["declared_guards"].as_array().unwrap().len(),
            1,
            "{change}"
        );
    }
}

#[test]
fn an_arbitrary_guard_resolves_an_inline_test_module_in_the_daemon() {
    let (mut case, _, _) = manager_spend_observer_fixture(true);
    case.write("crates/rsid/src/lib.rs", "mod input;\nmod observer;\n");
    case.write("crates/rsid/src/input.rs", "pub fn input() -> u8 { 1 }\n");
    let invariant: String = gate("other-03", "invariant")
        .lines()
        .map(|l| format!("    {l}"))
        .collect::<Vec<_>>()
        .join("\n");
    case.write(
        "crates/rsid/src/observer.rs",
        &format!("#[cfg(test)]\nmod tests {{\n{invariant}\n}}\n"),
    );
    case.write(
        GUARD_TABLE,
        &format!("{}\n", json!({"version": 1, "guards": [{"package": "rsid", "test": "observer::tests::invariant", "modules": ["input"]}]})),
    );
    case.commit();
    case.rebase_base_here();
    case.write("crates/rsid/src/input.rs", "pub fn input() -> u8 { 2 }\n");
    case.commit();
    let r = case.check(&["--filters-only"]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert!(
        strings(&r.receipt["suggested_filters"])
            .contains(&"rsid=shard:other-03:test(=observer::tests::invariant)".to_owned())
    );
    assert_eq!(
        r.receipt["declared_guards"][0]["test"],
        json!("observer::tests::invariant")
    );
}

fn manifest_dir_guard_fixture() -> (Case, &'static str, String) {
    let mut case = Case::new();
    let name = "workspace_code_never_bakes_the_manifest_path_at_compile_time";
    case.write(
        "crates/rsi-common/tests/manifest_dir_guard.rs",
        &format!("#[test]\nfn {name}() {{}}\n"),
    );
    case.write("crates/rsid/tests/e2e_seed.rs", "#[test]\nfn seed() {}\n");
    case.commit();
    case.rebase_base_here();
    (
        case,
        name,
        format!("rsi-common=test:manifest_dir_guard:test(={name})"),
    )
}

#[test]
fn scanned_rust_changes_select_one_exact_workspace_manifest_guard() {
    for (path, text) in [
        (
            "crates/rsid/tests/e2e_seed.rs",
            "#[test]\nfn seed() { let _x = 1; }\n",
        ),
        (
            "crates/rsid/tests/e2e_seed.rs",
            "// a comment is scanned too\n#[test]\nfn seed() {}\n",
        ),
        (
            "crates/rsi-common/src/untested.rs",
            "pub fn u() { let _x = 1; }\n",
        ),
        ("crates/new-crate/examples/example.rs", "fn main() {}\n"),
    ] {
        let (mut case, _, pin) = manifest_dir_guard_fixture();
        case.write(path, text);
        case.commit();
        let r = case.check(&["--filters-only"]);
        assert_eq!(r.code, 0, "{path}: {}", r.summary);
        assert_eq!(
            strings(&r.receipt["suggested_filters"])
                .iter()
                .filter(|f| **f == pin)
                .count(),
            1,
            "{path}"
        );
        assert_eq!(
            r.receipt["workspace_source_observers"],
            json!([pin]),
            "{path}"
        );
    }
}

#[test]
fn manifest_guard_observer_respects_the_scanners_path_exemptions() {
    for path in [
        "crates/rsid/build.rs",
        "crates/rsi-common/src/manifest_dir.rs",
        "crates/rsid/target/generated.rs",
        "tools/example.rs",
        "crates/rsid/notes.md",
    ] {
        let (mut case, _, _) = manifest_dir_guard_fixture();
        case.write(path, "// outside the guard's scan\n");
        case.commit();
        let r = case.check(&["--filters-only"]);
        assert_eq!(r.code, 0, "{path}: {}", r.summary);
        assert_eq!(r.receipt["workspace_source_observers"], json!([]), "{path}");
    }
}

fn agent_schema_observer_fixture(case: &mut Case, with_test: bool) {
    case.write(
        "crates/rsi-common/src/lib.rs",
        "pub mod agent_control_schema;\n",
    );
    case.write(
        "crates/rsi-common/src/agent_control_schema.rs",
        "pub fn descriptor() -> &'static str { \"old\" }\n",
    );
    case.write("crates/rsid/src/lib.rs", "pub mod rpc;\n");
    case.write("crates/rsid/src/rpc.rs", "mod tests;\n");
    let mut names = vec![
        (if with_test {
            "operator_daemon_config_is_absent_from_every_agent_surface"
        } else {
            "unrelated"
        })
        .to_owned(),
    ];
    names.push("other_fixture".into());
    case.write(
        "crates/rsid/src/rpc/tests.rs",
        &gated_tests("other-01", &names, &[]),
    );
    case.commit();
    case.rebase_base_here();
}

#[test]
fn an_agent_control_schema_edit_selects_the_operator_only_catalog_contract_test() {
    let mut case = Case::new();
    agent_schema_observer_fixture(&mut case, true);
    case.write(
        "crates/rsi-common/src/agent_control_schema.rs",
        "pub fn descriptor() -> &'static str { \"new\" }\n",
    );
    case.commit();
    let r = case.check(&["--no-compile"]);
    assert_eq!(r.code, 0, "{}", r.summary);
    let filters = strings(&r.receipt["suggested_filters"]);
    assert!(filters.contains(&"rsid=shard:other-01:test(/^rpc::tests::(?:\\w+::)*operator_daemon_config_is_absent_from_every_agent_surface$/)".to_owned()), "{filters:?}");
    assert!(!filters.join("").contains("other_fixture"));
    assert_eq!(
        r.receipt["source_observer_pins"],
        json!(["rpc::tests::operator_daemon_config_is_absent_from_every_agent_surface"])
    );
    assert_eq!(r.receipt["source_observer_missing"], json!([]));
}

#[test]
fn an_unrelated_rsi_common_edit_does_not_select_the_catalog_contract_test() {
    let mut case = Case::new();
    agent_schema_observer_fixture(&mut case, true);
    case.write(
        "crates/rsi-common/src/types.rs",
        "pub fn t() { let _x = 1; }\n#[cfg(test)]\nmod tests { #[test] fn a() {} }\n",
    );
    case.commit();
    let r = case.check(&["--no-compile"]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(r.receipt["source_observer_pins"], json!([]));
    assert!(
        !strings(&r.receipt["suggested_filters"])
            .join("")
            .contains("operator_daemon_config")
    );
}

#[test]
fn a_missing_source_observer_test_is_named_in_the_receipt() {
    let mut case = Case::new();
    agent_schema_observer_fixture(&mut case, false);
    case.write(
        "crates/rsi-common/src/agent_control_schema.rs",
        "pub fn descriptor() -> &'static str { \"new\" }\n",
    );
    case.commit();
    let r = case.check(&["--no-compile"]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(
        r.receipt["source_observer_missing"],
        json!(["rpc::tests::operator_daemon_config_is_absent_from_every_agent_surface"])
    );
}

fn rpc_dispatch_fixture(case: &mut Case, count: usize) -> (String, String) {
    let path = "crates/rsid/src/rpc.rs".to_owned();
    let arms: String = (0..count)
        .map(|n| format!("            \"Verb{n}\" => self.handle_{n}(request).await,\n"))
        .collect();
    let rpc = format!(
        "mod tests;\nimpl RpcServer {{\n    fn dispatch(&self) {{\n        let result = match request.method.as_str() {{\n{arms}            _ => unknown(),\n        }};\n    }}\n}}\n"
    );
    case.write("crates/rsid/src/lib.rs", "pub mod rpc;\n");
    case.write(&path, &rpc);
    let unrelated = numbered("unrelated_", count);
    let calls: Vec<(&str, &str)> = unrelated.iter().map(|n| (n.as_str(), "dispatch")).collect();
    let mut tests = gated_tests("other-03", &unrelated, &calls);
    tests += "const RPC_SOURCE: &str = include_str!(\"../rpc.rs\");\nconst EXAMPLE: &str = r#\"include_str!(\"../rpc.rs\")\"#;\nfn dispatch_methods() {\n    let source = RPC_SOURCE;\n}\n";
    tests += &gated_tests(
        "other-03",
        &[
            "dispatch_table_complete".into(),
            "export_works".into(),
            "export_refused".into(),
            "rpc_catalog".into(),
        ],
        &[
            ("dispatch_table_complete", "dispatch_methods"),
            ("rpc_catalog", "dispatch_methods"),
            ("export_works", "handle_export"),
            ("export_refused", "ExportThing"),
        ],
    );
    case.write("crates/rsid/src/rpc/tests.rs", &tests);
    case.commit();
    case.rebase_base_here();
    (path, rpc)
}

#[test]
fn multiline_dispatch_addition_selects_item_and_table_tests() {
    for count in [5, 70] {
        let mut case = Case::new();
        let (path, rpc) = rpc_dispatch_fixture(&mut case, count);
        case.write(&path, &rpc.replace("            _ =>", "            \"ExportThing\" => {\n                self.handle_export(request)\n                    .await\n            }\n            _ =>"));
        case.commit();
        let r = case.check(&[]);
        assert_eq!(r.code, 0, "{count}: {}", r.summary);
        assert_eq!(r.receipt["broad"], json!(false), "{count}");
        assert_eq!(
            r.receipt["suggested_filters"],
            json!([
                "rsid=shard:other-03:test(/^rpc::tests::(?:\\w+::)*(?:dispatch_table_complete|export_refused|export_works|rpc_catalog)$/)"
            ]),
            "{count}"
        );
    }
}

#[test]
fn dispatch_prelude_and_unknown_arm_changes_keep_explicit_broad_coverage() {
    for (before, after) in [
        (
            "        let result =",
            "        shared_gate();\n        let result =",
        ),
        ("_ => unknown()", "_ => new_unknown()"),
    ] {
        let mut case = Case::new();
        let (path, rpc) = rpc_dispatch_fixture(&mut case, 70);
        case.write(&path, &rpc.replace(before, after));
        case.commit();
        let r = case.check(&[]);
        assert_eq!(r.code, 0, "{before}: {}", r.summary);
        assert_eq!(r.receipt["broad"], json!(true), "{before}");
        assert!(
            r.receipt["broad_filters"][0]["reason"]
                .as_str()
                .unwrap()
                .contains("extends beyond simple"),
            "{}",
            r.receipt["broad_filters"]
        );
        assert_eq!(
            (
                &r.receipt["broad_filters"][0]["stop"],
                &r.receipt["broad_filters"][0]["worker_bounded"]
            ),
            (&Value::Null, &json!(false))
        );
        assert_eq!(
            r.receipt["suggested_filters"],
            json!(["rsid=shard:other-03:test(rpc)"]),
            "{before}"
        );
    }
}

#[test]
fn family_registration_traces_qualified_predicate_and_private_request_helpers() {
    let mut case = Case::new();
    let (path, rpc) = rpc_dispatch_fixture(&mut case, 70);
    let family = "pub(super) fn is_operator_method(method: &str) -> bool {\n    method == \"AnswerPendingDecisionV1\"\n}\nfn request() {\n    let method = \"AnswerPendingDecisionV1\";\n}\n";
    case.write(
        "crates/rsid/src/rpc/sessions.rs",
        &format!(
            "{family}{}",
            gated_tests(
                "other-03",
                &["pending_answer_admission".into()],
                &[("pending_answer_admission", "request")]
            )
        ),
    );
    let peers = numbered("peer_", 70);
    let peer_calls: Vec<(&str, &str)> = peers.iter().map(|n| (n.as_str(), "fixture")).collect();
    case.write(
        "crates/rsid/src/rpc/unrelated.rs",
        &format!("fn is_operator_method() {{\n    other_methods();\n}}\nfn request() {{\n    unrelated();\n}}\nfn fixture() {{\n    request();\n}}\n{}", gated_tests("other-03", &peers, &peer_calls)),
    );
    case.commit();
    case.rebase_base_here();
    case.write(
        &path,
        &rpc.replace("            _ =>", "            method if sessions::is_operator_method(method) => {\n                self.handle_answer_pending_decision(request).await\n            }\n            _ =>"),
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(r.receipt["broad"], json!(false));
    assert_eq!(
        filter_set(&r),
        set_of(&[
            "rsid=shard:other-03:test(/^rpc::sessions::(?:\\w+::)*pending_answer_admission$/)",
            "rsid=shard:other-03:test(/^rpc::tests::(?:\\w+::)*(?:dispatch_table_complete|rpc_catalog)$/)"
        ])
    );
    let exported = format!(
        "{family}{}",
        gated_tests(
            "other-03",
            &["pending_answer_admission".into()],
            &[("pending_answer_admission", "request")]
        )
    )
    .replace("fn request()", "pub(crate) fn request()");
    case.write("crates/rsid/src/rpc/sessions.rs", &exported);
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(r.receipt["broad"], json!(false));
    assert_eq!(
        filter_set(&r),
        set_of(&[
            "rsid=shard:other-03:test(rpc::sessions)",
            "rsid=shard:other-03:test(/^rpc::tests::(?:\\w+::)*(?:dispatch_table_complete|rpc_catalog)$/)"
        ])
    );
    case.write(
        "crates/rsid/src/rpc/loader.rs",
        "use super::sessions::request as session_request;\npub(crate) fn load() {\n    session_request();\n}\n",
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(
        r.receipt["broad"],
        json!(false),
        "{}",
        r.receipt["broad_filters"]
    );
    assert_eq!(
        filter_set(&r),
        set_of(&[
            "rsid=shard:other-03:test(rpc::sessions)",
            "rsid=shard:other-03:test(/^rpc::tests::(?:\\w+::)*(?:dispatch_table_complete|rpc_catalog)$/)"
        ])
    );
}

#[test]
fn path_mounted_test_files_resolve_through_sibling_directories_and_chains() {
    let mut case = Case::new();
    case.write("crates/rsid/src/lib.rs", "pub mod session;\n");
    case.write("crates/rsid/src/session/mod.rs", "mod tests;\n");
    case.write(
        "crates/rsid/src/session/tests/mod.rs",
        "#[cfg(test)]\n#[path = \"../shared_fixtures/portfolio.rs\"]\nmod portfolio;\n",
    );
    case.write(
        "crates/rsid/src/session/shared_fixtures/portfolio.rs",
        "#[path = \"portfolio_levels.rs\"]\nmod levels;\n",
    );
    case.write(
        "crates/rsid/src/session/shared_fixtures/portfolio_levels.rs",
        &gated_tests("other-03", &["l1".into()], &[]),
    );
    case.commit();
    case.rebase_base_here();
    case.write(
        "crates/rsid/src/session/shared_fixtures/portfolio_levels.rs",
        &format!(
            "{}const EDIT: u8 = 1;\n",
            gated_tests("other-03", &["l1".into()], &[])
        ),
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(
        r.receipt["suggested_filters"],
        json!(["rsid=shard:other-03:test(session::tests::portfolio::levels)"])
    );
    assert_eq!(r.receipt["dropped_filters"], json!([]));
}

fn include_fixture(binding: &str, render_tests: &[String], extra: &[(&str, String)]) -> Case {
    let mut case = Case::new();
    case.write("crates/rsid/src/lib.rs", "pub mod session;\n");
    case.write(
        "crates/rsid/src/session/mod.rs",
        "mod preamble;\nmod other;\npub fn central() { preamble::render(); }\n",
    );
    case.write("crates/rsid/src/session/guidance/w.md", "old\n");
    let calls: Vec<(&str, &str)> = render_tests
        .iter()
        .map(|n| (n.as_str(), "render"))
        .collect();
    let mut names = render_tests.to_vec();
    names.push("t_unrelated".into());
    case.write(
        "crates/rsid/src/session/preamble.rs",
        &format!(
            "{binding}\npub fn render() -> usize {{ TEXT.len() }}\n{}",
            gated_tests("other-03", &names, &calls)
        ),
    );
    case.write(
        "crates/rsid/src/session/other.rs",
        &gated_tests(
            "other-03",
            &["t_central".into(), "t_far".into(), "t_other".into()],
            &[("t_central", "central"), ("t_far", "t_central")],
        ),
    );
    for (path, text) in extra {
        case.write(path, text);
    }
    case.commit();
    case.rebase_base_here();
    case.write("crates/rsid/src/session/guidance/w.md", "new\n");
    case.commit();
    case
}

const INCLUDE_BINDING: &str = "const TEXT: &str = include_str!(\"guidance/w.md\");";

#[test]
fn included_data_file_selects_the_tests_of_its_const_instead_of_the_module_closure() {
    let case = include_fixture(INCLUDE_BINDING, &["t_render".into()], &[]);
    let r = case.check(&["--filters-only"]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(
        r.receipt["suggested_filters"],
        json!([
            "rsid=shard:other-03:test(/^session::other::(?:\\w+::)*t_central$/)",
            "rsid=shard:other-03:test(/^session::preamble::(?:\\w+::)*t_render$/)"
        ])
    );
    assert_eq!(r.receipt["broad"], json!(false));
}

#[test]
fn included_data_file_with_an_unresolvable_reader_keeps_the_lander_selection() {
    let case = include_fixture(
        "const TEXT: &str = concat!(include_str!(\"guidance/w.md\"), \"x\");",
        &["t_render".into()],
        &[],
    );
    let r = case.check(&["--filters-only"]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(r.receipt["suggested_filters"], json!([]));
}

#[test]
fn included_data_file_whose_tests_exceed_the_bound_keeps_the_lander_selection() {
    let case = include_fixture(INCLUDE_BINDING, &numbered("t", 70), &[]);
    let r = case.check(&["--filters-only"]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(r.receipt["suggested_filters"], json!([]));
}

#[test]
fn included_data_file_also_read_by_a_path_literal_keeps_the_lander_selection() {
    let other = gated_tests("other-03", &["t_read".into()], &[]).replace(
        "fn t_read() {}",
        "fn t_read() { let _ = \"guidance/w.md\"; }",
    );
    let case = include_fixture(
        INCLUDE_BINDING,
        &["t_render".into()],
        &[("crates/rsid/src/session/other.rs", other)],
    );
    let r = case.check(&["--filters-only"]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(r.receipt["suggested_filters"], json!([]));
}

// -------------------------------------------- exported helper and hub identity

const HELPER: &str = "pub fn changed_helper() -> u32 {\n    1\n}\n";

fn core_unrelated() -> String {
    gated_tests("other-01", &numbered("core_filler_", 70), &[])
}
fn gated_call(shard: &str, name: &str, call: &str) -> String {
    with_body(&gate(shard, name), &format!("{call};"))
}
fn gated_call_fn(name: &str, call: &str) -> String {
    with_body(&gate("other-01", name), &format!("{call}();"))
}

/// The #2247 fixture: `dialectic::tools::execute_tool` and a private
/// `openai::execute_tool` share a name.
struct Identity {
    case: Case,
}
impl Identity {
    fn new() -> Self {
        let mut case = Case::empty();
        case.write(
            "crates/rsid/src/lib.rs",
            "pub mod core;\npub mod dialectic;\npub mod local;\npub mod openai;\npub mod unique;\n",
        );
        case.write(
            "crates/rsid/src/core.rs",
            &format!("{HELPER}{}", core_unrelated()),
        );
        case.write("crates/rsid/src/unique.rs", "pub struct Unique;\n");
        case.write(
            "crates/rsid/src/dialectic/mod.rs",
            &format!(
                "pub mod tools;\n\npub fn run_dialectic() {{\n    tools::execute_tool();\n}}\n{}",
                gated_call_fn("dialectic_observes_the_helper", "run_dialectic")
            ),
        );
        case.write(
            "crates/rsid/src/dialectic/tools.rs",
            "pub fn execute_tool() {\n    changed_helper();\n}\n",
        );
        case.write(
            "crates/rsid/src/openai.rs",
            &format!("fn execute_tool() {{\n    other();\n}}\n\nfn openai_turn() {{\n    execute_tool();\n}}\n{}", gated_call_fn("openai_turn_is_unrelated", "openai_turn")),
        );
        case.write(
            "crates/rsid/src/local.rs",
            &format!("fn variable_user() {{\n    let execute_tool = 1;\n    let _ = execute_tool + 1;\n}}\n{}", gated_call_fn("a_local_variable_is_unrelated", "variable_user")),
        );
        case.commit();
        case.rebase_base_here();
        Self { case }
    }
    fn commit_base(&mut self) {
        self.case.commit();
        self.case.rebase_base_here();
    }
    /// Derive filters for the one-line edit of `changed_helper` (no compile).
    fn receipt(&mut self) -> Value {
        self.case.write(
            "crates/rsid/src/core.rs",
            &format!(
                "{}{}",
                HELPER.replace("    1\n", "    2\n"),
                core_unrelated()
            ),
        );
        self.case.commit();
        let r = self.case.check(&["--filters-only"]);
        assert_eq!(r.code, 0, "{}", r.summary);
        r.receipt
    }
    fn mods(&mut self, extra: &[&str]) {
        let mut names: std::collections::BTreeSet<&str> =
            ["core", "dialectic", "local", "openai", "unique"]
                .into_iter()
                .collect();
        names.extend(extra.iter().copied());
        self.case.write(
            "crates/rsid/src/lib.rs",
            &names
                .iter()
                .map(|n| format!("pub mod {n};\n"))
                .collect::<String>(),
        );
    }
    fn consumer(&mut self, module: &str, path: &str, test: &str) {
        self.case.write(
            &format!("crates/rsid/src/{module}.rs"),
            &format!(
                "pub fn via_{module}() {{\n    {path}();\n}}\n{}",
                gated_call_fn(test, &format!("via_{module}"))
            ),
        );
    }
    fn unrelated_same_name(&mut self) {
        self.case.write(
            "crates/rsid/src/stranger.rs",
            "pub fn execute_tool() {\n    other();\n}\n",
        );
    }
}

fn observed(receipt: &Value) -> String {
    strings(&receipt["suggested_filters"]).join(" ")
}
fn resolved(receipt: &Value, observers: &[&str]) {
    assert_eq!(
        receipt["broad"],
        json!(false),
        "{}",
        receipt["broad_filters"]
    );
    let seen = observed(receipt);
    for name in observers {
        assert!(seen.contains(name), "{name} {seen}");
    }
    assert!(!seen.contains("openai_turn_is_unrelated"));
    assert!(!seen.contains("stranger_is_unrelated"));
    assert!(
        !strings(&receipt["suggested_filters"])
            .iter()
            .any(|f| f.contains("core_filler"))
    );
}
fn broad(receipt: &Value, reasons: &[&str]) {
    assert_eq!(receipt["broad"], json!(true), "{receipt}");
    let reason = receipt["broad_filters"][0]["reason"].as_str().unwrap();
    assert!(
        reason.contains("ambiguous exported helper: dialectic::tools::execute_tool"),
        "{reason}"
    );
    for part in reasons {
        assert!(reason.contains(part), "{part} in {reason}");
    }
    assert!(
        strings(&receipt["suggested_filters"])
            .iter()
            .any(|f| f.contains("core"))
    );
}

#[test]
fn a_qualified_call_reaches_its_helper_and_a_same_name_private_one_stays_out() {
    let mut id = Identity::new();
    let receipt = id.receipt();
    assert_eq!(
        receipt["broad"],
        json!(false),
        "{}",
        receipt["broad_filters"]
    );
    let seen = observed(&receipt);
    assert!(seen.contains("dialectic_observes_the_helper"));
    assert!(!seen.contains("openai_turn_is_unrelated"));
    assert!(!seen.contains("a_local_variable_is_unrelated"));
}

#[test]
fn an_explicit_import_and_a_glob_of_the_owner_both_reach_the_helper() {
    let mut id = Identity::new();
    id.case.write(
        "crates/rsid/src/imports.rs",
        &format!("use crate::dialectic::tools::execute_tool;\n\npub fn via_import() {{\n    execute_tool();\n}}\n{}", gated_call_fn("import_observes_the_helper", "via_import")),
    );
    id.case.write(
        "crates/rsid/src/dialectic/tools/tests.rs",
        &format!(
            "use super::*;\n\nfn drives() {{\n    execute_tool();\n}}\n{}",
            gated_call_fn("glob_observes_the_helper", "drives")
        ),
    );
    id.case.write("crates/rsid/src/lib.rs", "pub mod core;\npub mod dialectic;\npub mod imports;\npub mod local;\npub mod openai;\npub mod unique;\n");
    id.commit_base();
    let receipt = id.receipt();
    assert_eq!(
        receipt["broad"],
        json!(false),
        "{}",
        receipt["broad_filters"]
    );
    let seen = observed(&receipt);
    for name in [
        "dialectic_observes_the_helper",
        "import_observes_the_helper",
        "glob_observes_the_helper",
    ] {
        assert!(seen.contains(name), "{name}");
    }
    assert!(!seen.contains("openai_turn_is_unrelated"));
}

#[test]
fn an_inline_module_helper_is_reached_through_its_qualified_path() {
    let mut id = Identity::new();
    id.case.write(
        "crates/rsid/src/dialectic/mod.rs",
        &format!("pub mod tools;\n\nfn archive() {{\n    seam::execute_tool();\n}}\n\npub(crate) mod seam {{\n    pub(super) fn execute_tool() {{\n        changed_helper();\n    }}\n}}\n{}", gated_call_fn("seam_observes_the_helper", "archive")),
    );
    id.case.write(
        "crates/rsid/src/dialectic/tools.rs",
        "pub fn execute_tool() {\n    other();\n}\n",
    );
    id.commit_base();
    let receipt = id.receipt();
    assert_eq!(
        receipt["broad"],
        json!(false),
        "{}",
        receipt["broad_filters"]
    );
    let seen = observed(&receipt);
    assert!(seen.contains("seam_observes_the_helper"));
    assert!(!seen.contains("openai_turn_is_unrelated"));
}

#[test]
fn a_pub_use_reexport_chain_reaches_the_helper_and_a_same_name_item_stays_out() {
    let mut id = Identity::new();
    id.case.write(
        "crates/rsid/src/dialectic/mod.rs",
        &format!("pub mod tools;\npub use tools::execute_tool;\n\npub fn run_dialectic() {{\n    tools::execute_tool();\n}}\n{}", gated_call_fn("dialectic_observes_the_helper", "run_dialectic")),
    );
    id.case.write(
        "crates/rsid/src/facade.rs",
        "pub use crate::dialectic::execute_tool;\n",
    );
    id.consumer(
        "plugins",
        "crate::dialectic::execute_tool",
        "plugins_observe_the_helper",
    );
    id.consumer(
        "deep",
        "crate::facade::execute_tool",
        "a_two_hop_reexport_observes_the_helper",
    );
    id.unrelated_same_name();
    id.consumer(
        "stray",
        "crate::stranger::execute_tool",
        "stranger_is_unrelated",
    );
    id.mods(&["facade", "plugins", "deep", "stranger", "stray"]);
    id.commit_base();
    let receipt = id.receipt();
    resolved(
        &receipt,
        &[
            "dialectic_observes_the_helper",
            "plugins_observe_the_helper",
            "a_two_hop_reexport_observes_the_helper",
        ],
    );
}

#[test]
fn a_reexported_module_and_a_glob_chain_reach_the_helper() {
    let mut id = Identity::new();
    id.case.write(
        "crates/rsid/src/api.rs",
        "pub use crate::dialectic::tools;\n",
    );
    id.case.write(
        "crates/rsid/src/facade.rs",
        "pub use crate::dialectic::tools::*;\n",
    );
    id.case
        .write("crates/rsid/src/prelude.rs", "pub use crate::facade::*;\n");
    id.consumer(
        "through_module",
        "crate::api::tools::execute_tool",
        "a_reexported_module_observes_the_helper",
    );
    id.case.write(
        "crates/rsid/src/through_glob.rs",
        &format!(
            "use crate::prelude::*;\n\npub fn via_through_glob() {{\n    execute_tool();\n}}\n{}",
            gated_call_fn("a_glob_chain_observes_the_helper", "via_through_glob")
        ),
    );
    id.unrelated_same_name();
    id.case.write(
        "crates/rsid/src/stray.rs",
        &format!(
            "use crate::stranger::*;\n\npub fn via_stray() {{\n    execute_tool();\n}}\n{}",
            gated_call_fn("stranger_is_unrelated", "via_stray")
        ),
    );
    id.mods(&[
        "api",
        "facade",
        "prelude",
        "through_module",
        "through_glob",
        "stranger",
        "stray",
    ]);
    id.commit_base();
    let receipt = id.receipt();
    resolved(
        &receipt,
        &[
            "a_reexported_module_observes_the_helper",
            "a_glob_chain_observes_the_helper",
        ],
    );
}

#[test]
fn an_explicit_alias_of_the_helper_is_a_caller_and_an_alias_of_a_same_name_item_is_not() {
    let mut id = Identity::new();
    id.case.write(
        "crates/rsid/src/plugins.rs",
        &format!("use crate::dialectic::tools::execute_tool as run_tool;\n\npub fn via_plugins() {{\n    run_tool();\n}}\n{}", gated_call_fn("an_alias_observes_the_helper", "via_plugins")),
    );
    id.case.write(
        "crates/rsid/src/aliased_facade.rs",
        "pub use crate::dialectic::tools::execute_tool as launch;\n",
    );
    id.consumer(
        "second_hop",
        "crate::aliased_facade::launch",
        "a_reexported_alias_observes_the_helper",
    );
    id.unrelated_same_name();
    id.case.write(
        "crates/rsid/src/stray.rs",
        &format!("use crate::stranger::execute_tool as run_tool;\n\npub fn via_stray() {{\n    run_tool();\n}}\n{}", gated_call_fn("stranger_is_unrelated", "via_stray")),
    );
    id.mods(&[
        "plugins",
        "aliased_facade",
        "second_hop",
        "stranger",
        "stray",
    ]);
    id.commit_base();
    let receipt = id.receipt();
    resolved(
        &receipt,
        &[
            "an_alias_observes_the_helper",
            "a_reexported_alias_observes_the_helper",
        ],
    );
}

#[test]
fn a_module_named_like_the_helper_is_a_path_segment_not_a_reference() {
    let mut id = Identity::new();
    id.case.write(
        "crates/rsid/src/exec/mod.rs",
        &format!("pub mod execute_tool;\n\npub fn summarize() {{\n    crate::exec::execute_tool::summary();\n}}\n{}", gated_call_fn("a_module_segment_is_unrelated", "summarize")),
    );
    id.case.write(
        "crates/rsid/src/exec/execute_tool.rs",
        "pub fn summary() {}\n",
    );
    id.mods(&["exec"]);
    id.commit_base();
    let receipt = id.receipt();
    resolved(&receipt, &[]);
    assert!(!observed(&receipt).contains("a_module_segment_is_unrelated"));
}

#[test]
fn imported_topology_verify_module_preserves_observers_without_claiming_cold_archive_verify() {
    let mut id = Identity::new();
    for (module, child) in [
        ("store", "cold_archive"),
        ("store/cold_archive", "tests"),
        ("topology", "verify"),
        ("session", "manager_ledger"),
        ("session/manager_ledger", "job_evidence"),
    ] {
        id.case.write(
            &format!("crates/rsid/src/{module}/mod.rs"),
            &format!("pub mod {child};\n"),
        );
    }
    id.case.write(
        "crates/rsid/src/store/cold_archive/tests.rs",
        &format!(
            "pub struct Fixture;\n\nimpl Fixture {{\n    pub fn verify(&self) {{\n        changed_helper();\n    }}\n}}\n{}",
            with_body(&gate("other-01", "cold_archive_observes_verify"), "Fixture::verify(&Fixture);")
        ),
    );
    id.case.write(
        "crates/rsid/src/topology/verify.rs",
        "pub fn judge() {\n    changed_helper();\n}\n",
    );
    id.case
        .write("crates/rsid/src/unique.rs", "pub fn verify() {}\n");
    id.case.write(
        "crates/rsid/src/session/manager_ledger/job_evidence.rs",
        &format!("use crate::topology::verify::{{self, Reason}};\n\npub fn judged() {{\n    verify::judge();\n}}\n{}", gated_call_fn("job_evidence_observes_topology_verifier", "judged")),
    );
    id.mods(&["store", "topology", "session"]);
    id.commit_base();
    let receipt = id.receipt();
    resolved(
        &receipt,
        &[
            "cold_archive_observes_verify",
            "job_evidence_observes_topology_verifier",
        ],
    );
    let loaded = id.case.load(&id.case.base, &id.case.head).unwrap();
    let mut references = super::identity::ItemReferences::new(&loaded.sources).unwrap();
    let verify = references.callers("store::cold_archive::tests", "verify");
    assert_eq!(
        verify.functions,
        std::collections::BTreeSet::from([(
            "store::cold_archive::tests".to_owned(),
            "cold_archive_observes_verify".to_owned()
        )])
    );
    let judge = references.callers("topology::verify", "judge");
    assert_eq!(
        judge.functions,
        std::collections::BTreeSet::from([(
            "session::manager_ledger::job_evidence".to_owned(),
            "judged".to_owned()
        )])
    );
}

#[test]
fn a_reexport_cycle_keeps_the_broad_fallback_and_names_it() {
    let mut id = Identity::new();
    id.case.write(
        "crates/rsid/src/ping.rs",
        "pub use crate::pong::execute_tool;\n",
    );
    id.case.write(
        "crates/rsid/src/pong.rs",
        "pub use crate::ping::execute_tool;\n",
    );
    id.consumer(
        "plugins",
        "crate::ping::execute_tool",
        "plugins_observe_the_helper",
    );
    id.mods(&["ping", "pong", "plugins"]);
    id.commit_base();
    let receipt = id.receipt();
    broad(&receipt, &["plugins::via_plugins", "a re-export cycle"]);
}

#[test]
fn a_chain_two_items_can_supply_keeps_the_broad_fallback_and_names_it() {
    let mut id = Identity::new();
    id.unrelated_same_name();
    id.case.write(
        "crates/rsid/src/facade.rs",
        "pub use crate::dialectic::tools::*;\npub use crate::stranger::*;\n",
    );
    id.consumer(
        "plugins",
        "crate::facade::execute_tool",
        "plugins_observe_the_helper",
    );
    id.mods(&["facade", "plugins", "stranger"]);
    id.commit_base();
    let receipt = id.receipt();
    broad(
        &receipt,
        &["plugins::via_plugins", "may name 2 different items"],
    );
}

#[test]
fn a_reexport_chain_past_the_depth_bound_keeps_the_broad_fallback_and_names_it() {
    let mut id = Identity::new();
    let hops: Vec<String> = (0..8).map(|n| format!("hop{n}")).collect();
    for (index, here) in hops.iter().enumerate() {
        let there = hops.get(index + 1).map_or("dialectic", String::as_str);
        id.case.write(
            &format!("crates/rsid/src/{here}.rs"),
            &format!("pub use crate::{there}::execute_tool;\n"),
        );
    }
    id.case.write(
        "crates/rsid/src/dialectic/mod.rs",
        "pub mod tools;\npub use tools::execute_tool;\n",
    );
    id.consumer(
        "plugins",
        "crate::hop0::execute_tool",
        "plugins_observe_the_helper",
    );
    let mut extra: Vec<&str> = hops.iter().map(String::as_str).collect();
    extra.push("plugins");
    id.mods(&extra);
    id.commit_base();
    let receipt = id.receipt();
    broad(&receipt, &["plugins::via_plugins", "deeper than"]);
}

#[test]
fn a_reexport_of_no_known_module_keeps_the_broad_fallback_and_names_it() {
    let mut id = Identity::new();
    id.case.write(
        "crates/rsid/src/facade.rs",
        "pub use crate::missing::execute_tool;\n",
    );
    id.consumer(
        "plugins",
        "crate::facade::execute_tool",
        "plugins_observe_the_helper",
    );
    id.mods(&["facade", "plugins"]);
    id.commit_base();
    let receipt = id.receipt();
    broad(
        &receipt,
        &["plugins::via_plugins", "that names no known module"],
    );
}

#[test]
fn an_alias_of_a_module_this_scan_cannot_place_keeps_the_broad_fallback() {
    let mut id = Identity::new();
    id.case.write(
        "crates/rsid/src/plugins.rs",
        "use crate::missing::execute_tool as run_tool;\n\npub fn load() {\n    run_tool();\n}\n",
    );
    id.mods(&["plugins"]);
    id.commit_base();
    let receipt = id.receipt();
    broad(
        &receipt,
        &["renames it in a `use` of a module this scan cannot place"],
    );
}

#[test]
fn a_type_declaration_quoted_in_a_string_does_not_make_the_type_ambiguous() {
    let mut id = Identity::new();
    id.case.write(
        "crates/rsid/src/builder.rs",
        &format!("pub struct Builder<T> {{\n    value: T,\n}}\n\nimpl<T> Builder<T>\nwhere\n    T: Copy,\n{{\n    pub fn new(value: T) -> Self {{\n        changed_helper();\n        Self {{ value }}\n    }}\n}}\n\npub fn make() {{\n    Builder::new(1);\n}}\n{}", gated_call_fn("builder_observes_the_helper", "make")),
    );
    id.case.write(
        "crates/rsid/src/other_kind.rs",
        &format!("pub struct Other;\n\nimpl Other {{\n    pub fn new() -> Self {{\n        Self\n    }}\n}}\n\npub fn make_other() {{\n    Other::new();\n}}\n{}", gated_call_fn("other_kind_is_unrelated", "make_other")),
    );
    id.case.write(
        "crates/rsid/src/scan.rs",
        "const QUOTED: &str = \"pub struct Builder<T> {\";\n",
    );
    id.case.write(
        "crates/rsid/src/lib.rs",
        "pub mod builder;\npub mod core;\npub mod dialectic;\npub mod local;\npub mod openai;\npub mod other_kind;\npub mod scan;\npub mod unique;\n",
    );
    id.commit_base();
    let receipt = id.receipt();
    assert_eq!(
        receipt["broad"],
        json!(false),
        "{}",
        receipt["broad_filters"]
    );
    let seen = observed(&receipt);
    assert!(seen.contains("builder_observes_the_helper"));
    assert!(!seen.contains("other_kind_is_unrelated"));
}

// ----------------------------------------------------------------- hub bound

struct Hub {
    case: Case,
    changed: String,
}
const HUB_CHANGED: &str = "pub mod chain;\npub mod direct;\npub mod far;\npub mod hub;\npub mod other;\n\npub fn changed_helper() -> u32 {\n    1\n}\n";
impl Hub {
    fn new() -> Self {
        let mut case = Case::empty();
        case.write(
            "crates/rsid/src/lib.rs",
            "pub mod openai;\npub mod session;\n",
        );
        case.write(
            "crates/rsid/src/session/mod.rs",
            &format!("{HUB_CHANGED}{}", core_unrelated()),
        );
        case.write(
            "crates/rsid/src/openai.rs",
            "fn execute_tool() {\n    unrelated();\n}\n",
        );
        case.write("crates/rsid/src/session/hub.rs", "pub struct Hub;\n\nimpl Hub {\n    pub fn step(&self) {\n        changed_helper();\n    }\n}\n");
        case.write("crates/rsid/src/session/other.rs", "pub fn step() {}\n");
        let far: String = (0..160)
            .map(|n| format!("pub fn far_user_{n:03}(_hub: &Hub) {{}}\n\n"))
            .collect();
        case.write(
            "crates/rsid/src/session/far.rs",
            &format!(
                "{far}{}{}",
                gated_call("other-01", "far_observer_a", "far_user_000(&Hub)"),
                gated_call("other-01", "far_observer_b", "far_user_001(&Hub)")
            ),
        );
        case.write(
            "crates/rsid/src/session/direct.rs",
            &format!(
                "pub fn wrapper() {{\n    Hub.step();\n}}\n\nfn cyc_a() {{\n    Hub.step();\n    cyc_b();\n}}\n\nfn cyc_b() {{\n    cyc_a();\n}}\n{}{}{}{}{}",
                gated_call("other-01", "calls_the_changed_function", "changed_helper()"),
                gated_call("other-01", "calls_the_hub_method", "Hub.step()"),
                gated_call("other-01", "calls_the_unrelated_step", "other::step()"),
                gated_call("other-01", "calls_the_wrapper", "wrapper()"),
                gated_call("other-01", "calls_the_cycle", "cyc_b()"),
            ),
        );
        case.write("crates/rsid/src/session/chain.rs", "");
        case.commit();
        case.rebase_base_here();
        Self {
            case,
            changed: HUB_CHANGED.to_owned(),
        }
    }
    fn commit_to_base(&mut self) {
        self.case.write(
            "crates/rsid/src/session/mod.rs",
            &format!("{}{}", self.changed, core_unrelated()),
        );
        self.case.commit();
        self.case.rebase_base_here();
    }
    fn receipt(&mut self) -> Value {
        self.case.write(
            "crates/rsid/src/session/mod.rs",
            &format!("{}{}", self.changed, core_unrelated()).replace("    1\n", "    2\n"),
        );
        self.case.commit();
        let r = self.case.check(&["--filters-only"]);
        assert_eq!(r.code, 0, "{}", r.summary);
        r.receipt
    }
}

fn distances(receipt: &Value, names: &[&str]) -> BTreeMap<String, u64> {
    let mut found = BTreeMap::new();
    for item in receipt["direct_observer_filters"].as_array().unwrap() {
        let filter = item["filter"].as_str().unwrap();
        for name in names {
            let pattern = crate::re::Re::new(&format!(r"(?<![A-Za-z0-9_]){name}(?![A-Za-z0-9_])"));
            if pattern.is_match(filter) {
                let distance = item["distance"].as_u64().unwrap();
                let slot = found.entry((*name).to_owned()).or_insert(distance);
                *slot = (*slot).min(distance);
            }
        }
    }
    found
}
const NEAR: [&str; 7] = [
    "calls_the_changed_function",
    "calls_the_hub_method",
    "calls_the_unrelated_step",
    "calls_the_wrapper",
    "calls_the_cycle",
    "far_observer_a",
    "far_observer_b",
];

#[test]
fn every_direct_observer_stays_named_nearest_first_and_the_far_ones_stay_in_the_broad_filter() {
    let mut hub = Hub::new();
    let receipt = hub.receipt();
    assert_eq!(receipt["broad"], json!(true));
    let broad = &receipt["broad_filters"][0];
    assert!(
        broad["reason"]
            .as_str()
            .unwrap()
            .contains("observer trace stops at high-fan-in Hub"),
        "{broad}"
    );
    assert_eq!(
        broad["hubs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["token"].clone())
            .collect::<Vec<_>>(),
        [json!("Hub")]
    );
    assert!(broad["hubs"][0]["functions"].as_u64().unwrap() > 150);
    assert_eq!(
        distances(&receipt, &NEAR),
        BTreeMap::from([
            ("calls_the_changed_function".to_owned(), 1),
            ("calls_the_hub_method".to_owned(), 2),
            ("calls_the_unrelated_step".to_owned(), 2),
            ("calls_the_wrapper".to_owned(), 3),
            ("calls_the_cycle".to_owned(), 4),
        ])
    );
    assert_eq!(
        receipt["direct_observers"],
        json!({"tests": 5, "by_distance": {"1": 1, "2": 2, "3": 1, "4": 1}})
    );
    assert_eq!(broad["direct_tests"], json!(5));
    assert_eq!(
        (&broad["stop"], &broad["worker_bounded"]),
        (&json!("hub"), &json!(true))
    );
    assert!(
        receipt["suggested_filters"]
            .as_array()
            .unwrap()
            .contains(&broad["filter"])
    );
    assert_eq!(broad["tests"], json!(70 + 2 + 5));
}

#[test]
fn an_identity_the_scan_cannot_place_stays_broad_and_names_the_settled_observers() {
    let mut hub = Hub::new();
    hub.case.write(
        "crates/rsid/src/session/tools.rs",
        "pub fn execute_tool() {\n    changed_helper();\n}\n",
    );
    hub.case.write(
        "crates/rsid/src/session/plugins.rs",
        "pub fn load() {\n    crate::session::execute_tool();\n}\n",
    );
    hub.case.write(
        "crates/rsid/src/session/hub.rs",
        "pub use tools::execute_tool;\n\npub struct Hub;\n\nimpl Hub {\n    pub fn step(&self) {\n        changed_helper();\n    }\n}\n",
    );
    hub.changed = hub.changed.replace(
        "pub mod hub;",
        "pub mod hub;\npub mod plugins;\npub mod tools;",
    );
    hub.commit_to_base();
    let receipt = hub.receipt();
    let broad = &receipt["broad_filters"][0];
    assert!(
        broad["reason"]
            .as_str()
            .unwrap()
            .contains("ambiguous exported helper"),
        "{broad}"
    );
    assert_eq!(broad["trace_stopped"], json!("unresolved helper identity"));
    assert_eq!(
        (&broad["stop"], &broad["worker_bounded"]),
        (&json!("unresolved helper identity"), &json!(false))
    );
    assert_eq!(
        distances(&receipt, &NEAR),
        BTreeMap::from([("calls_the_changed_function".to_owned(), 1)])
    );
    assert!(
        receipt["suggested_filters"]
            .as_array()
            .unwrap()
            .contains(&broad["filter"])
    );
}

#[test]
fn a_trace_that_outgrows_the_function_bound_keeps_the_layers_it_settled() {
    let mut hub = Hub::new();
    let mut links = String::from("pub fn link_000() {\n    changed_helper();\n}\n\n");
    for n in 1..160 {
        links += &format!("pub fn link_{n:03}() {{\n    link_{:03}();\n}}\n\n", n - 1);
    }
    hub.case.write(
        "crates/rsid/src/session/chain.rs",
        &format!(
            "{links}{}{}",
            gated_call("other-01", "calls_the_chain_start", "link_000()"),
            gated_call("other-01", "calls_the_chain_end", "link_159()")
        ),
    );
    hub.case.write("crates/rsid/src/session/hub.rs", "");
    hub.case.write("crates/rsid/src/session/far.rs", "");
    hub.commit_to_base();
    let receipt = hub.receipt();
    let broad = &receipt["broad_filters"][0];
    assert!(
        broad["reason"]
            .as_str()
            .unwrap()
            .contains("observer trace exceeds 150 functions"),
        "{broad}"
    );
    assert_eq!(broad["trace_stopped"], json!("function bound"));
    assert_eq!(
        (&broad["stop"], &broad["worker_bounded"]),
        (&json!("function bound"), &json!(true))
    );
    assert_eq!(
        distances(
            &receipt,
            &[
                "calls_the_changed_function",
                "calls_the_chain_start",
                "calls_the_chain_end"
            ]
        ),
        BTreeMap::from([
            ("calls_the_changed_function".to_owned(), 1),
            ("calls_the_chain_start".to_owned(), 2)
        ])
    );
    assert!(
        receipt["suggested_filters"]
            .as_array()
            .unwrap()
            .contains(&broad["filter"])
    );
}

// -------------------------------------------------- private fields, modules, split store

const STORE_FIELD: &str = "\n    cache: RefCell<idle::IdleCache>,";
const STORE_INIT: &str = "\n            cache: RefCell::new(idle::IdleCache::default()),";

fn field_store_text(field: &str, init: &str) -> String {
    format!(
        "pub mod idle;\nmod open_tests;\nmod users;\n\npub struct Store {{\n    conn: u32,{field}\n}}\n\nimpl Store {{\n    pub fn open() -> Store {{\n        Store {{\n            conn: 1,{init}\n        }}\n    }}\n\n    pub fn open_for_test() -> Self {{\n        Self {{\n            conn: 2,{init}\n        }}\n    }}\n}}\n"
    )
}

/// #213: a new private field beside mechanical constructor initializers.
fn private_field_case() -> Case {
    let mut case = Case::empty();
    case.write("crates/rsid-store/src/lib.rs", "pub mod store;\n");
    case.write(
        "crates/rsid-store/src/store/mod.rs",
        &field_store_text("", ""),
    );
    case.write(
        "crates/rsid-store/src/store/idle.rs",
        &format!(
            "#[derive(Default)]\npub struct IdleCache {{\n    pub hits: u32,\n}}\nimpl Store {{\n    pub fn peek(&self) -> u32 {{\n        self.cache.borrow().hits\n    }}\n}}\n{}",
            gated_tests("store-01", &["idle_peek".into()], &[("idle_peek", "Store::open().peek")])
        ),
    );
    case.write(
        "crates/rsid-store/src/store/users.rs",
        &(0..160)
            .map(|n| format!("fn user_{n}(_store: &Store) {{}}\n"))
            .collect::<String>(),
    );
    let opens = numbered("opens_", 70);
    let calls: Vec<(&str, &str)> = opens.iter().map(|n| (n.as_str(), "Store::open")).collect();
    case.write(
        "crates/rsid-store/src/store/open_tests.rs",
        &gated_tests("store-01", &opens, &calls),
    );
    case.commit();
    case.rebase_base_here();
    case
}

fn field_candidate(case: &mut Case, text: &str) -> Value {
    case.write("crates/rsid-store/src/store/mod.rs", text);
    case.commit();
    let r = case.check(&["--filters-only"]);
    assert_eq!(r.code, 0, "{}", r.summary);
    r.receipt
}

#[test]
fn unique_private_field_and_mechanical_constructors_name_only_the_readers() {
    let mut case = private_field_case();
    let receipt = field_candidate(&mut case, &field_store_text(STORE_FIELD, STORE_INIT));
    assert_eq!(
        receipt["broad"],
        json!(false),
        "{}",
        receipt["broad_filters"]
    );
    assert_eq!(receipt["broad_filters"], json!([]));
    let filters = strings(&receipt["suggested_filters"]);
    assert_eq!(filters.len(), 1, "{filters:?}");
    assert!(filters[0].contains("idle_peek"));
    assert!(!receipt["suggested_filters"].to_string().contains("opens_"));
}

#[test]
fn a_non_mechanical_initializer_keeps_the_broad_module_filter() {
    let mut case = private_field_case();
    let receipt = field_candidate(
        &mut case,
        &field_store_text(
            STORE_FIELD,
            "\n            cache: RefCell::new(warm_up(1)),",
        ),
    );
    assert_eq!(receipt["broad"], json!(true));
}

#[test]
fn a_constructor_that_also_reads_the_field_keeps_its_callers() {
    let mut case = private_field_case();
    let text = field_store_text(STORE_FIELD, STORE_INIT)
        .replacen(
            "        Store {\n            conn: 1,",
            "        let warm = Self::peek;\n        Store {\n            conn: 1,",
            1,
        )
        .replacen(
            "pub fn open() -> Store {",
            "pub fn open() -> Store {\n        let seen = Self::cache_len(&Self::open_for_test());",
            1,
        );
    case.write("crates/rsid-store/src/store/seen.rs", "impl Store {\n    fn cache_len(&self) -> u32 {\n        self.cache.borrow().hits\n    }\n}\n");
    let receipt = field_candidate(&mut case, &text);
    assert_eq!(receipt["broad"], json!(true));
}

#[test]
fn the_same_field_name_on_another_type_is_not_traced_by_name() {
    let mut case = private_field_case();
    case.write(
        "crates/rsid-store/src/store/other.rs",
        "pub struct Other {\n    cache: u8,\n}\n",
    );
    case.commit();
    case.rebase_base_here();
    let receipt = field_candidate(&mut case, &field_store_text(STORE_FIELD, STORE_INIT));
    assert_eq!(receipt["broad"], json!(true));
}

#[test]
fn a_pub_field_keeps_the_broad_module_filter() {
    let mut case = private_field_case();
    let receipt = field_candidate(
        &mut case,
        &field_store_text(
            &STORE_FIELD.replace("    cache", "    pub cache"),
            STORE_INIT,
        ),
    );
    assert_eq!(receipt["broad"], json!(true));
}

#[test]
fn a_macro_that_can_spell_the_field_keeps_the_broad_module_filter() {
    let mut case = private_field_case();
    case.write(
        "crates/rsid-store/src/store/macros.rs",
        "macro_rules! field {\n    ($name:ident) => {\n        $name: 0,\n    };\n}\n",
    );
    case.commit();
    case.rebase_base_here();
    case.write(
        "crates/rsid-store/src/store/macros.rs",
        "macro_rules! field {\n    ($name:ident) => {\n        cache: 0,\n    };\n}\n",
    );
    case.commit();
    case.rebase_base_here();
    let receipt = field_candidate(&mut case, &field_store_text(STORE_FIELD, STORE_INIT));
    assert_eq!(receipt["broad"], json!(true));
}

#[test]
fn a_destructuring_pattern_row_is_a_read_not_an_initializer() {
    let mut case = private_field_case();
    case.write("crates/rsid-store/src/store/pattern.rs", "fn take(store: Store) {\n    let Store {\n        cache: seen,\n        ..\n    } = store;\n}\n");
    case.commit();
    case.rebase_base_here();
    let receipt = field_candidate(&mut case, &field_store_text(STORE_FIELD, STORE_INIT));
    assert_eq!(receipt["broad"], json!(true));
}

#[test]
fn the_tree_proof_accepts_one_private_declaration_only() {
    let proof = |files: &[(&str, &str)], types: &[&str]| {
        let texts: BTreeMap<String, String> = files
            .iter()
            .map(|(p, t)| ((*p).into(), (*t).into()))
            .collect();
        let types: BTreeSet<String> = types.iter().map(|t| (*t).into()).collect();
        super::seeds::unique_private_field_owner(&texts, "cache", &types).unwrap()
    };
    let one = [
        ("a.rs", "pub struct Store {\n    cache: u8,\n}\n"),
        (
            "b.rs",
            "fn open() -> Store {\n    Store {\n        cache: Cell::new(0),\n    }\n}\n",
        ),
    ];
    assert_eq!(proof(&one, &["Store"]).as_deref(), Some("Store"));
    assert_eq!(proof(&one, &[]), None);
    let mut other = one.to_vec();
    other.push(("c.rs", "pub struct Other {\n    cache: u8,\n}\n"));
    assert_eq!(proof(&other, &["Store", "Other"]), None);
    let pubbed = [
        ("a.rs", "pub struct Store {\n    pub cache: u8,\n}\n"),
        one[1],
    ];
    assert_eq!(proof(&pubbed, &["Store"]), None);
    let call = [one[0], one[1], ("c.rs", "fn f() {\n    g(cache: 1);\n}\n")];
    assert_eq!(proof(&call, &["Store"]), None);
    assert_eq!(
        proof(
            &[(
                "a.rs",
                "fn f() {\n    Store {\n        cache: Cell::new(0),\n    }\n}\n"
            )],
            &["Store"]
        ),
        None
    );
}

fn changed_modules_case() -> Case {
    let mut case = Case::empty();
    case.write("crates/rsid/src/lib.rs", "pub mod session;\n");
    case.write(
        "crates/rsid/src/session/mod.rs",
        "mod rotation;\nmod tools;\npub fn central() {}\n",
    );
    case.write(
        "crates/rsid/src/session/rotation.rs",
        "mod recovery;\nmod fence_tests;\nmod abandon;\npub fn rot() {}\n",
    );
    case.write(
        "crates/rsid/src/session/rotation/recovery.rs",
        &format!(
            "pub fn recover() {{}}\n{}",
            gate("session-03", "recovery_keeps_the_controller")
        ),
    );
    case.write(
        "crates/rsid/src/session/rotation/fence_tests.rs",
        &gate("session-03", "fence_holds"),
    );
    case.write(
        "crates/rsid/src/session/rotation/abandon.rs",
        &gate("session-03", "abandon_is_a_separate_source"),
    );
    case.write(
        "crates/rsid/src/session/tools.rs",
        &gate("session-01", "tools_are_unrelated"),
    );
    case.write("crates/rsid/tests/it.rs", "#[test]\nfn t() {}\n");
    case.commit();
    case.rebase_base_here();
    case
}

#[test]
fn changed_modules_name_their_own_tests_and_exclude_unchanged_sources() {
    let mut case = changed_modules_case();
    case.write(
        "crates/rsid/src/session/rotation.rs",
        "mod recovery;\nmod fence_tests;\nmod abandon;\npub fn rot() -> u32 {\n    2\n}\n",
    );
    case.write(
        "crates/rsid/src/session/rotation/recovery.rs",
        &format!(
            "pub fn recover() -> u32 {{\n    3\n}}\n{}",
            gate("session-03", "recovery_keeps_the_controller")
        ),
    );
    case.write(
        "crates/rsid/src/session/mod.rs",
        "mod rotation;\nmod tools;\npub fn central() -> u32 {\n    1\n}\n",
    );
    case.write(
        "crates/rsid/tests/it.rs",
        "#[test]\nfn t() {\n    assert!(true);\n}\n",
    );
    case.commit();
    let r = case.check(&["--filters-only"]);
    assert_eq!(r.code, 0, "{}", r.summary);
    let mut records = BTreeMap::new();
    for record in r.receipt["changed_modules"].as_array().unwrap() {
        let key = (
            record["package"].as_str().unwrap().to_owned(),
            record.get("module").and_then(Value::as_str).map_or_else(
                || {
                    format!(
                        "{}:{}",
                        record["kind"].as_str().unwrap(),
                        record["target"].as_str().unwrap()
                    )
                },
                str::to_owned,
            ),
        );
        records.insert(key, record.clone());
    }
    let keys: BTreeSet<_> = records.keys().cloned().collect();
    let expected: BTreeSet<(String, String)> = [
        ("rsid", "session"),
        ("rsid", "session::rotation"),
        ("rsid", "session::rotation::recovery"),
        ("rsid", "test:it"),
    ]
    .iter()
    .map(|(p, m)| ((*p).into(), (*m).into()))
    .collect();
    assert_eq!(keys, expected);
    let get = |m: &str| records[&("rsid".to_owned(), m.to_owned())].clone();
    assert_eq!(
        get("session::rotation")["excluded"],
        json!(["session::rotation::abandon"])
    );
    assert_eq!(get("session::rotation::recovery")["excluded"], json!([]));
    assert_eq!(get("session::rotation::recovery")["tests"], json!(1));
    assert_eq!(
        get("session")["excluded"],
        json!(["session::rotation::abandon", "session::tools"])
    );
}

#[test]
fn a_comment_only_edit_changes_no_module() {
    let mut case = changed_modules_case();
    case.write(
        "crates/rsid/src/session/rotation.rs",
        "mod recovery;\nmod fence_tests;\nmod abandon;\n// a note\npub fn rot() {}\n",
    );
    case.commit();
    let r = case.check(&["--filters-only"]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(r.receipt["changed_modules"], json!([]));
}

#[test]
fn a_linux_host_derives_the_linux_runnable_observers_only() {
    let mut case = Case::empty();
    case.host = "linux".into();
    let gate_row =
        "#[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-other-01\"))]";
    let filler: String = (0..70)
        .map(|i| format!("    {gate_row}\n    #[test]\n    fn filler_{i}() {{}}\n"))
        .collect();
    case.write("crates/rsid/src/store/migrations/v150.rs", "// v150\n");
    case.write("crates/rsid/src/lib.rs", "mod agent_jobs;\n#[cfg(target_os = \"macos\")]\nmod mac_jobs;\n#[cfg(target_os = \"linux\")]\nmod linux_jobs;\n");
    case.write("crates/rsid/src/mac_jobs.rs", &format!("{gate_row}\n#[test]\nfn mac_file_test() {{ super::agent_jobs::ensure_build_slice(); }}\n"));
    case.write("crates/rsid/src/linux_jobs.rs", &format!("{gate_row}\n#[test]\nfn linux_file_test() {{ super::agent_jobs::ensure_build_slice(); }}\n"));
    let agent = format!(
        "pub fn ensure_build_slice() {{\n    1;\n}}\n\n#[cfg(test)]\nmod tests {{\n    use super::*;\n\n{filler}\n    {gate_row}\n    #[test]\n    fn linux_probe() {{ ensure_build_slice(); }}\n\n    {gate_row}\n    #[cfg(target_os = \"macos\")]\n    #[test]\n    fn macos_direct() {{ ensure_build_slice(); }}\n\n    #[cfg(target_os = \"macos\")]\n    mod macos_only {{\n        use super::*;\n        {gate_row}\n        #[test]\n        fn macos_inline_nested() {{ ensure_build_slice(); }}\n    }}\n    #[cfg(feature = \"x\")]\n    mod feature_only {{\n        use super::*;\n        {gate_row}\n        #[test]\n        fn feature_nested() {{ ensure_build_slice(); }}\n    }}\n}}\n"
    );
    case.write("crates/rsid/src/agent_jobs.rs", &agent);
    case.commit();
    case.rebase_base_here();
    case.write(
        "crates/rsid/src/agent_jobs.rs",
        &agent.replace("    1;", "    2;"),
    );
    case.commit();
    let r = case.check(&["--no-compile"]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(
        r.receipt["suggested_filters"],
        json!([
            "rsid=shard:other-01:test(/^agent_jobs::(?:\\w+::)*(?:feature_nested|linux_probe)$/)",
            "rsid=shard:other-01:test(/^linux_jobs::(?:\\w+::)*linux_file_test$/)"
        ])
    );
}

// ------------------------------------------------------------- split store receipts

fn split_store_case() -> Case {
    let mut case = Case::empty();
    case.manifests = true;
    case.write("crates/rsid/Cargo.toml", RSID_MANIFEST);
    case.write("crates/rsid-store/Cargo.toml", STORE_MANIFEST);
    case.write("crates/rsid/src/lib.rs", "mod foo;\nmod bar;\n");
    case.write("crates/rsid-store/src/lib.rs", "mod store;\n");
    case.write("crates/rsid-store/src/store/mod.rs", "mod q;\nmod r;\n");
    case.write("crates/rsid/src/foo.rs", &gate("store-02", "a"));
    case.write("crates/rsid/src/bar.rs", "pub fn bar() {}\nmod tests;\n");
    case.write("crates/rsid/src/bar/tests.rs", &gate("other-03", "b"));
    case.write("crates/rsid-store/src/store/q.rs", &gate("store-01", "c"));
    case.write("crates/rsid-store/src/store/r.rs", &gate("store-02", "d"));
    case.write(
        "crates/rsid-store/src/store/migrations/v150.rs",
        "// v150\n",
    );
    case.commit();
    case.rebase_base_here();
    case
}

/// A fake cargo: appends its arguments to a log and exits per `script`.
fn fake_cargo(case: &mut Case, body: &str) -> std::path::PathBuf {
    let dir = case.fx.path().join(".fake");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("cargo.sh");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\necho \"$@\" >> '{}'\n{body}",
            dir.join("cargo.log").display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    case.cargo = path.to_string_lossy().into_owned();
    dir.join("cargo.log")
}
fn cargo_commands(log: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .map(|t| t.lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

#[test]
fn an_rsid_store_source_change_compiles_its_shard_and_one_dependent_shard() {
    let mut case = split_store_case();
    let log = fake_cargo(&mut case, "");
    case.write(
        "crates/rsid-store/src/store/q.rs",
        &(gate("store-01", "c") + "const EDIT: u8 = 1;\n"),
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(
        r.receipt["shards"]["compiled_ok"],
        json!(["other-03", "store-01"])
    );
    let commands = cargo_commands(&log);
    assert!(commands.contains(&"check -p rsid-store --lib --tests --no-default-features --features test-shard-store-01".to_owned()), "{commands:?}");
    assert!(
        commands.contains(
            &"check -p rsid --lib --tests --no-default-features --features test-shard-other-03"
                .to_owned()
        ),
        "{commands:?}"
    );
    assert_eq!(
        r.receipt["suggested_filters"],
        json!(["rsid=shard:store-01:test(store::q)"])
    );
}

#[test]
fn a_shard_shared_by_both_packages_compiles_both() {
    let mut case = split_store_case();
    let log = fake_cargo(&mut case, "");
    case.write(
        "crates/rsid-store/src/store/r.rs",
        &(gate("store-02", "d") + "const EDIT: u8 = 1;\n"),
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert!(cargo_commands(&log).contains(&"check -p rsid -p rsid-store --lib --tests --no-default-features --features test-shard-store-02".to_owned()));
    assert_eq!(
        r.receipt["suggested_filters"],
        json!(["rsid=shard:store-02:test(store::r)"])
    );
}

#[test]
fn a_migration_only_store_change_compiles_a_fallback_shard_and_reports_the_number() {
    let mut case = split_store_case();
    let log = fake_cargo(&mut case, "");
    case.write(
        "crates/rsid-store/src/store/migrations/v151.rs",
        "// v151\n",
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(r.receipt["migrations"]["new"], json!([151]));
    assert_eq!(r.receipt["migrations"]["base_head"], json!(150));
    assert_eq!(r.receipt["migrations"]["numbering_conflict"], json!(false));
    assert_eq!(r.receipt["shards"]["fallback"], json!("store-01"));
    assert!(cargo_commands(&log).contains(&"check -p rsid-store --lib --tests --no-default-features --features test-shard-store-01".to_owned()));
    assert!(r.summary.contains("migrations=V151"), "{}", r.summary);
}

#[test]
fn a_migration_unit_added_to_the_pre_split_directory_fails_the_receipt() {
    let mut case = split_store_case();
    fake_cargo(&mut case, "");
    case.write(
        "crates/rsid/src/store/migrations/v151.rs",
        "// v151 in the dead directory\n",
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 1);
    assert_eq!(
        r.receipt["migrations"]["stray_pre_split"],
        json!(["crates/rsid/src/store/migrations/v151.rs"])
    );
    assert_eq!(r.receipt["ok"], json!(false));
    assert!(r.summary.contains("STRAY_PRE_SPLIT_MIGRATION"));
}

#[test]
fn a_pre_split_base_still_reports_its_head_from_the_old_directory() {
    let mut case = split_store_case();
    fake_cargo(&mut case, "");
    let original_base = case.base.clone();
    case.remove("crates/rsid-store/src/store/migrations/v150.rs");
    case.write("crates/rsid/src/store/migrations/v150.rs", "// v150\n");
    case.commit_on(&[&original_base]);
    case.rebase_base_here();
    case.remove("crates/rsid/src/store/migrations/v150.rs");
    case.write(
        "crates/rsid-store/src/store/migrations/v150.rs",
        "// v150\n",
    );
    case.write(
        "crates/rsid-store/src/store/migrations/v151.rs",
        "// v151\n",
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(r.receipt["migrations"]["base_head"], json!(150));
    assert_eq!(r.receipt["migrations"]["new"], json!([151]));
    assert_eq!(r.receipt["migrations"]["numbering_conflict"], json!(false));
    assert_eq!(r.receipt["migrations"]["stray_pre_split"], json!([]));
}

#[test]
fn workspace_packages_are_not_cleaned_by_default() {
    let mut case = split_store_case();
    let log = fake_cargo(&mut case, "");
    case.write(
        "crates/rsid-store/src/store/q.rs",
        &(gate("store-01", "c") + "const EDIT: u8 = 1;\n"),
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    let commands = cargo_commands(&log);
    assert!(
        !commands.is_empty() && commands.iter().all(|c| c.starts_with("check ")),
        "{commands:?}"
    );
}

#[test]
fn every_shard_failing_the_same_way_is_reported_as_a_suspected_stale_artifact() {
    let mut case = split_store_case();
    fake_cargo(
        &mut case,
        "echo 'error[E0609]: no field continue_from on type X' >&2\nexit 101\n",
    );
    case.write(
        "crates/rsid-store/src/store/q.rs",
        &(gate("store-01", "c") + "const EDIT: u8 = 1;\n"),
    );
    case.write(
        "crates/rsid-store/src/store/r.rs",
        &(gate("store-02", "d") + "const EDIT: u8 = 1;\n"),
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 1);
    let stale = &r.receipt["shards"]["shard_compile_suspect_stale"];
    assert!(
        stale["hint"]
            .as_str()
            .unwrap()
            .contains("CHECK_TOUCHED_SHARDS_CLEAN=1")
    );
    assert!(stale["error"].as_str().unwrap().contains("continue_from"));
    assert!(r.stderr.join("\n").contains("shard_compile_suspect_stale"));
}

#[test]
fn different_shard_failures_are_not_suspected_stale() {
    let mut case = split_store_case();
    fake_cargo(&mut case, "echo \"error: broke $*\" >&2\nexit 101\n");
    case.write(
        "crates/rsid-store/src/store/q.rs",
        &(gate("store-01", "c") + "const EDIT: u8 = 1;\n"),
    );
    case.write(
        "crates/rsid-store/src/store/r.rs",
        &(gate("store-02", "d") + "const EDIT: u8 = 1;\n"),
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 1);
    assert_eq!(
        r.receipt["shards"]["shard_compile_suspect_stale"],
        Value::Null
    );
}

#[test]
fn failed_compile_without_error_diagnostics_keeps_stderr_tail() {
    let mut case = split_store_case();
    fake_cargo(
        &mut case,
        "echo 'compiler wrapper unavailable' >&2\nexit 7\n",
    );
    case.write(
        "crates/rsid/src/foo.rs",
        &(gate("store-02", "a") + "const EDIT: u8 = 1;\n"),
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 1);
    assert_eq!(r.receipt["shards"]["compiled_failed"], json!(["store-02"]));
    assert!(
        r.stderr
            .join("\n")
            .contains("exit code: 7\ncompiler wrapper unavailable\n"),
        "{:?}",
        r.stderr
    );
}

#[test]
fn failed_compile_reports_first_error_and_span_before_warning_tail() {
    for error in [
        "error[E0609]: no field missing on type X",
        "error: fixture compile failed",
    ] {
        let mut case = split_store_case();
        let diagnostic = format!(
            "{error}\n  --> crates/rsid/src/foo.rs:12:7\n   |\n12 | value.missing\n   |       ^^^^^^^ unknown field\n\n"
        );
        let output = format!(
            "{}{diagnostic}{}",
            "warning: preceding noise\n".repeat(3000),
            "warning: trailing noise\n".repeat(3000)
        );
        fake_cargo(
            &mut case,
            &format!("cat >&2 <<'DIAGNOSTICS'\n{output}DIAGNOSTICS\nexit 101\n"),
        );
        case.write(
            "crates/rsid/src/foo.rs",
            &(gate("store-02", "a") + "const EDIT: u8 = 1;\n"),
        );
        case.commit();
        let r = case.check(&[]);
        assert_eq!(r.code, 1);
        assert!(r.summary.contains("FAIL"));
        assert_eq!(r.receipt["shards"]["compiled_failed"], json!(["store-02"]));
        let stderr = r.stderr.join("\n");
        assert!(stderr.contains(diagnostic.trim_end()), "{error}");
        assert!(stderr.find(error).unwrap() < stderr.find("warning: trailing noise").unwrap());
        assert!(stderr.contains("shard store-02 compile failed:"));
        assert!(stderr.contains("exit code: 101"));
        let without_progress: usize = r
            .stderr
            .iter()
            .filter(|l| !l.starts_with("check-touched-shards: ["))
            .map(|l| l.len() + 1)
            .sum();
        assert!(without_progress < 6500, "{without_progress}");
    }
}

#[test]
fn a_full_check_logs_each_phase_and_shard_with_its_build_slot_wait() {
    let mut case = split_store_case();
    let log = fake_cargo(
        &mut case,
        "echo \"rsi-phase 1700000000 slot-granted: waited 7s (build); running\" >&2\n",
    );
    case.write(
        "crates/rsid-store/src/store/q.rs",
        &(gate("store-01", "c") + "const EDIT: u8 = 1;\n"),
    );
    case.commit();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    let stderr = r.stderr.join("\n");
    assert!(
        stderr.contains("compiling shard store-01 (2/2)"),
        "{stderr}"
    );
    assert!(stderr.contains("shard store-01 ok in"));
    assert!(stderr.contains("(build slot wait 7s)"));
    assert!(stderr.contains("diff_and_sources done in"));
    let runs = r.receipt["shards"]["runs"].as_object().unwrap();
    assert_eq!(
        runs.keys().cloned().collect::<Vec<_>>(),
        ["other-03", "store-01"]
    );
    assert_eq!(runs["store-01"]["verdict"], json!("ok"));
    assert_eq!(runs["store-01"]["slot_wait_sec"], json!(7));
    assert!(
        runs["store-01"]["build_sec"].as_f64().unwrap()
            <= runs["store-01"]["seconds"].as_f64().unwrap()
    );
    assert_eq!(r.receipt["partial"], json!(false));
    assert!(!cargo_commands(&log).is_empty());
}

#[test]
fn filters_only_stays_quiet_on_stderr() {
    let mut case = split_store_case();
    case.write(
        "crates/rsid-store/src/store/q.rs",
        &(gate("store-01", "c") + "const EDIT: u8 = 1;\n"),
    );
    case.commit();
    let r = case.check(&["--filters-only", "--json"]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert!(r.stderr.is_empty(), "{:?}", r.stderr);
}

// ------------------------------------------------------------- leaf crate observers

const LEAF: &str = "crates/rsid-remote-read/src/projection.rs";
const PROJECTION: &str = "pub fn project_pending() -> u32 {\n    1\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn projects_one_pending_item() {\n        assert_eq!(project_pending(), 1);\n    }\n}\n";
const OWNER_PATH: &str = "crates/rsid/src/remote_read/owner.rs";

fn owner_text() -> String {
    gate("store-04", "owner_reads_the_projection").replace(
        "fn owner_reads_the_projection() {}",
        "fn owner_reads_the_projection() { assert_eq!(spawn_info_read(), 1); }",
    ) + "pub fn spawn_info_read() -> u32 {\n    project_pending()\n}\n"
}

fn leaf_case() -> Case {
    let mut case = Case::empty();
    case.write(
        "crates/rsid-remote-read/src/lib.rs",
        "mod projection;\npub use projection::project_pending;\n",
    );
    case.write(LEAF, PROJECTION);
    case.write(
        "crates/rsid/src/lib.rs",
        "pub mod remote_read;\npub mod session;\npub mod other;\n",
    );
    case.write(
        "crates/rsid/src/remote_read/mod.rs",
        "mod owner;\npub use rsid_remote_read::*;\n",
    );
    case.write(OWNER_PATH, &owner_text());
    case.write("crates/rsid/src/session/mod.rs", "mod queries;\n");
    case.write(
        "crates/rsid/src/session/queries.rs",
        &(gate("session-01", "session_probe_sees_the_projection").replace(
            "fn session_probe_sees_the_projection() {}",
            "fn session_probe_sees_the_projection() { assert_eq!(session_probe(), 1); }",
        ) + "pub fn session_probe() -> u32 {\n    project_pending()\n}\n"),
    );
    case.write(
        "crates/rsid/src/other.rs",
        &gate("other-01", "unrelated_daemon_test"),
    );
    case.commit();
    case.rebase_base_here();
    case
}
fn edit(case: &mut Case, path: &str, old: &str, new: &str) {
    let text = case.read(path);
    assert!(text.contains(old), "{old:?} not in {path}");
    case.write(path, &text.replacen(old, new, 1));
    case.commit();
}
fn rebase(case: &mut Case, path: &str, text: &str) {
    case.write(path, text);
    case.commit();
    case.rebase_base_here();
}
fn receipt(case: &Case) -> Value {
    let r = case.check(&["--filters-only", "--json"]);
    assert_eq!(r.code, 0, "{}", r.summary);
    r.receipt
}
fn has(filters: &Value, needle: &str) -> bool {
    strings(filters).iter().any(|f| f.contains(needle))
}

#[test]
fn a_leaf_production_edit_selects_its_tests_and_the_daemon_observers() {
    let mut case = leaf_case();
    edit(&mut case, LEAF, "    1\n}", "    2\n}");
    let r = receipt(&case);
    let filters = &r["suggested_filters"];
    assert!(has(filters, "rsid-remote-read=projection"), "{filters}");
    assert!(
        strings(filters).contains(&"rsid=shard:store-04:test(remote_read)".to_owned()),
        "{filters}"
    );
    assert!(
        has(filters, "session_probe_sees_the_projection"),
        "{filters}"
    );
    assert!(!has(filters, "unrelated_daemon_test"), "{filters}");
}

#[test]
fn a_leaf_production_edit_records_why_in_the_receipt() {
    let mut case = leaf_case();
    edit(&mut case, LEAF, "    1\n}", "    2\n}");
    let r = receipt(&case);
    let notes = r["leaf_observers"].as_array().unwrap();
    assert_eq!(
        notes.iter().map(|n| n["path"].clone()).collect::<Vec<_>>(),
        [json!(LEAF)]
    );
    assert_eq!(notes[0]["package"], json!("rsid-remote-read"));
    assert!(
        strings(&notes[0]["filters"]).contains(&"rsid=shard:store-04:test(remote_read)".to_owned())
    );
    assert!(
        notes[0]["reason"]
            .as_str()
            .unwrap()
            .contains("production edit")
    );
}

#[test]
fn harness_leaf_selects_its_tests_and_retained_daemon_observers() {
    let mut case = leaf_case();
    let leaf = "crates/rsid-harness/src/arguments.rs";
    case.write("crates/rsid-harness/src/lib.rs", "pub mod arguments;\n");
    case.write(leaf, PROJECTION);
    case.write(
        "crates/rsid/src/session/mod.rs",
        "mod harness; mod queries;\n",
    );
    case.write(
        "crates/rsid/src/session/harness/mod.rs",
        &format!(
            "pub use rsid_harness::arguments::*;\n{}",
            gate("session-05", "dispatch_validated_arguments").replace(
                "fn dispatch_validated_arguments() {}",
                "fn dispatch_validated_arguments() { assert_eq!(project_pending(), 1); }"
            )
        ),
    );
    case.commit();
    case.rebase_base_here();
    edit(&mut case, leaf, "    1\n}", "    2\n}");
    let r = receipt(&case);
    assert!(
        strings(&r["suggested_filters"]).contains(&"rsid-harness=arguments".to_owned()),
        "{}",
        r["suggested_filters"]
    );
    assert!(
        strings(&r["suggested_filters"])
            .contains(&"rsid=shard:session-05:test(session::harness)".to_owned()),
        "{}",
        r["suggested_filters"]
    );
    assert_eq!(r["leaf_observers"][0]["package"], json!("rsid-harness"));
}

#[test]
fn nested_harness_tool_edit_selects_the_leaf_module_and_daemon_observers() {
    let mut case = leaf_case();
    let leaf = "crates/rsid-harness/src/tools/file.rs";
    case.write("crates/rsid-harness/src/lib.rs", "pub mod tools;\n");
    case.write("crates/rsid-harness/src/tools/mod.rs", "pub mod file;\n");
    case.write(leaf, PROJECTION);
    case.write(
        "crates/rsid/src/session/mod.rs",
        "mod harness; mod queries;\n",
    );
    case.write(
        "crates/rsid/src/session/harness/mod.rs",
        &format!(
            "pub use rsid_harness::tools::file::*;\n{}",
            gate("session-05", "dispatch_validated_arguments").replace(
                "fn dispatch_validated_arguments() {}",
                "fn dispatch_validated_arguments() { assert_eq!(project_pending(), 1); }"
            )
        ),
    );
    case.commit();
    case.rebase_base_here();
    edit(&mut case, leaf, "    1\n}", "    2\n}");
    let r = receipt(&case);
    assert!(
        strings(&r["suggested_filters"]).contains(&"rsid-harness=tools::file".to_owned()),
        "{}",
        r["suggested_filters"]
    );
    assert!(
        strings(&r["suggested_filters"])
            .contains(&"rsid=shard:session-05:test(session::harness)".to_owned())
    );
    assert_eq!(r["leaf_observers"][0]["package"], json!("rsid-harness"));
}

#[test]
fn a_leaf_test_only_edit_selects_only_the_leaf_tests() {
    let mut case = leaf_case();
    edit(
        &mut case,
        LEAF,
        "assert_eq!(project_pending(), 1);",
        "assert_eq!(project_pending(), 1, \"still one\");",
    );
    let r = receipt(&case);
    assert_eq!(
        r["suggested_filters"],
        json!(["rsid-remote-read=projection"])
    );
    assert_eq!(r["leaf_observers"][0]["filters"], json!([]));
    assert!(
        r["leaf_observers"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("test code")
    );
}

#[test]
fn a_leaf_comment_only_edit_selects_no_daemon_observer() {
    let mut case = leaf_case();
    edit(
        &mut case,
        LEAF,
        "pub fn project_pending",
        "// The projection of one pending item.\npub fn project_pending",
    );
    let r = receipt(&case);
    assert!(
        !strings(&r["suggested_filters"])
            .iter()
            .any(|f| f.starts_with("rsid=shard:")),
        "{}",
        r["suggested_filters"]
    );
    assert_eq!(r["leaf_observers"], json!([]));
}

#[test]
fn a_leaf_export_edit_derives_no_filter_when_the_trace_is_unprovable() {
    let mut case = leaf_case();
    edit(
        &mut case,
        "crates/rsid-remote-read/src/lib.rs",
        "pub use projection::project_pending;",
        "pub use projection::{project_pending, other_pending};",
    );
    let r = receipt(&case);
    assert_eq!(r["suggested_filters"], json!([]));
    assert_eq!(
        r["unfiltered_leaf_paths"],
        json!(["crates/rsid-remote-read/src/lib.rs"])
    );
    assert_eq!(r["leaf_observers"][0]["unfiltered"], json!(true));
}

#[test]
fn unprovable_leaf_keeps_its_changed_tests_for_worker_verification() {
    let mut case = leaf_case();
    edit(&mut case, LEAF, "    1\n}", "    2\n}");
    edit(
        &mut case,
        "crates/rsid-remote-read/src/lib.rs",
        "pub use projection::project_pending;",
        "pub use projection::{project_pending, other_pending};",
    );
    let r = receipt(&case);
    assert_eq!(r["suggested_filters"], json!([]));
    assert!(
        strings(&r["worker_filters"]).contains(&"rsid-remote-read=projection".to_owned()),
        "{}",
        r["worker_filters"]
    );
}

#[test]
fn a_private_leaf_helper_reaches_the_daemon_observer_through_its_public_wrapper() {
    let mut case = leaf_case();
    rebase(
        &mut case,
        LEAF,
        "fn private_projection() -> u32 {\n    1\n}\n\npub fn project_pending() -> u32 {\n    private_projection()\n}\n",
    );
    edit(&mut case, LEAF, "    1\n}\n\npub fn", "    2\n}\n\npub fn");
    let r = receipt(&case);
    let filters = strings(&r["suggested_filters"]);
    assert!(
        filters.contains(&"rsid=shard:store-04:test(remote_read)".to_owned()),
        "{filters:?}"
    );
    assert!(
        filters
            .iter()
            .any(|f| f.contains("session_probe_sees_the_projection")
                && f.contains("shard:session-01:")),
        "{filters:?}"
    );
    assert!(!filters.iter().any(|f| f.contains("unrelated_daemon_test")));
    assert!(
        r["leaf_observers"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("sharded tests that name the change")
    );
}

#[test]
fn a_deep_private_leaf_chain_reaches_the_session_observer() {
    let mut case = leaf_case();
    rebase(
        &mut case,
        LEAF,
        "fn coverage_from_read() -> u32 {\n    1\n}\n\nfn pending_source_coverage() -> u32 {\n    coverage_from_read()\n}\n\npub fn finish_runtime_only_pending_sources() -> u32 {\n    pending_source_coverage()\n}\n",
    );
    rebase(
        &mut case,
        "crates/rsid/src/session/queries.rs",
        &(gate("session-01", "remote_runtime_candidate_snapshot_is_scalar").replace(
            "fn remote_runtime_candidate_snapshot_is_scalar() {}",
            "fn remote_runtime_candidate_snapshot_is_scalar() { assert_eq!(remote_runtime_only_pending_finish(), 1); }",
        ) + "pub fn remote_runtime_only_pending_finish() -> u32 {\n    finish_runtime_only_pending_sources()\n}\n"),
    );
    edit(
        &mut case,
        LEAF,
        "fn coverage_from_read() -> u32 {\n    1",
        "fn coverage_from_read() -> u32 {\n    2",
    );
    let r = receipt(&case);
    assert!(
        has(
            &r["suggested_filters"],
            "remote_runtime_candidate_snapshot_is_scalar"
        ),
        "{}",
        r["suggested_filters"]
    );
    assert!(!has(&r["suggested_filters"], "unrelated_daemon_test"));
}

#[test]
fn a_private_leaf_helper_nothing_observes_selects_the_facade_tests_only() {
    let mut case = leaf_case();
    rebase(
        &mut case,
        LEAF,
        "fn dead_helper() -> u32 {\n    1\n}\n\npub fn project_pending() -> u32 {\n    1\n}\n",
    );
    edit(
        &mut case,
        LEAF,
        "fn dead_helper() -> u32 {\n    1",
        "fn dead_helper() -> u32 {\n    2",
    );
    let r = receipt(&case);
    assert!(!has(
        &r["suggested_filters"],
        "session_probe_sees_the_projection"
    ));
    assert!(
        strings(&r["suggested_filters"])
            .contains(&"rsid=shard:store-04:test(remote_read)".to_owned())
    );
    assert!(
        r["leaf_observers"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("no other sharded test named the change")
    );
}

#[test]
fn an_ambiguous_public_leaf_name_derives_no_filter_and_says_why() {
    let mut case = leaf_case();
    rebase(
        &mut case,
        LEAF,
        "fn shared() -> u32 {\n    1\n}\n\npub fn shared_pending() -> u32 {\n    shared()\n}\n\npub fn shared_wrapper() -> u32 {\n    shared_pending()\n}\n",
    );
    rebase(
        &mut case,
        "crates/rsid/src/other.rs",
        &(gate("other-01", "unrelated_daemon_test")
            + "pub fn shared_pending() -> u32 {\n    0\n}\n"),
    );
    edit(
        &mut case,
        LEAF,
        "fn shared() -> u32 {\n    1",
        "fn shared() -> u32 {\n    2",
    );
    let r = receipt(&case);
    assert_eq!(r["suggested_filters"], json!([]));
    assert!(
        r["leaf_observers"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("ambiguous exported helper"),
        "{}",
        r["leaf_observers"]
    );
}

#[test]
fn a_retained_owner_edit_selects_the_owner_tests_and_no_leaf_filter() {
    let mut case = leaf_case();
    edit(
        &mut case,
        OWNER_PATH,
        "    project_pending()\n",
        "    project_pending() + 0\n",
    );
    let r = receipt(&case);
    assert!(
        strings(&r["suggested_filters"])
            .contains(&"rsid=shard:store-04:test(remote_read::owner)".to_owned()),
        "{}",
        r["suggested_filters"]
    );
    assert!(
        !strings(&r["suggested_filters"])
            .iter()
            .any(|f| f.starts_with("rsid-remote-read="))
    );
    assert_eq!(r["leaf_observers"], json!([]));
}

fn fixture_text(name: &str) -> String {
    std::fs::read_to_string(
        std::path::Path::new(rsi_common::manifest_dir!())
            .join("../../scripts/tests/fixtures/touched_shards_remote_chain")
            .join(name),
    )
    .unwrap()
}

#[test]
fn a_body_edit_in_coverage_from_read_reaches_the_session_observer() {
    let mut case = Case::empty();
    let projection = fixture_text("projection.rs.txt");
    assert!(projection.lines().count() > 200);
    case.write(
        "crates/rsid-remote-read/src/lib.rs",
        "mod projection;\npub use projection::{finish_runtime_only_pending_sources, pending_source_coverage};\n",
    );
    case.write(LEAF, &projection);
    case.write(
        "crates/rsid/src/lib.rs",
        "pub mod remote_read;\npub mod session;\npub mod other;\n",
    );
    case.write(
        "crates/rsid/src/remote_read/mod.rs",
        "mod owner;\npub use rsid_remote_read::*;\n",
    );
    case.write(
        "crates/rsid/src/remote_read/owner.rs",
        &gate("store-04", "owner_reads_the_projection"),
    );
    case.write(
        "crates/rsid/src/session/mod.rs",
        "mod queries;\n#[cfg(test)]\nmod tests;\n",
    );
    case.write(
        "crates/rsid/src/session/queries.rs",
        &fixture_text("queries.rs.txt"),
    );
    case.write(
        "crates/rsid/src/session/tests.rs",
        &fixture_text("session_tests.rs.txt"),
    );
    case.write(
        "crates/rsid/src/other.rs",
        &gate("other-01", "unrelated_daemon_test"),
    );
    case.commit();
    case.rebase_base_here();
    edit(
        &mut case,
        LEAF,
        "if available { count } else { 0 },",
        "if available { count } else { 1 },",
    );
    let r = receipt(&case);
    let filters = strings(&r["suggested_filters"]);
    assert!(
        filters.iter().any(|f| f
            .contains("remote_runtime_candidate_snapshot_is_scalar_and_nonblocking")
            && f.contains("shard:session-03:")),
        "{filters:?} {}",
        r["unfiltered_leaf_paths"]
    );
    assert_eq!(r["unfiltered_leaf_paths"], json!([]));
    assert!(!filters.iter().any(|f| f.contains("unrelated_daemon_test")));
    assert!(filters.contains(&"rsid=shard:store-04:test(remote_read)".to_owned()));
}

// ---------------------------------------------------------------- planning leaf

const PLANNING_LEAF: &str = "crates/rsi-graph/src/planning/graph.rs";
const PLANNING: &str = "pub fn plan_edges() -> u32 {\n    1\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn plans_one_edge() {\n        assert_eq!(plan_edges(), 1);\n    }\n}\n";

fn planning_case() -> Case {
    let mut case = Case::empty();
    case.write("crates/rsi-graph/src/lib.rs", "pub mod planning;\n");
    case.write(
        "crates/rsi-graph/src/planning/mod.rs",
        "pub mod graph;\npub use graph::plan_edges;\n",
    );
    case.write(PLANNING_LEAF, PLANNING);
    case.write(
        "crates/rsi-graph/src/compiler.rs",
        "pub fn compile_graph() -> u32 {\n    1\n}\n",
    );
    case.write(
        "crates/rsid/src/lib.rs",
        "pub mod topology;\npub mod session;\npub mod other;\n",
    );
    case.write("crates/rsid/src/topology/mod.rs", "mod tests;\n");
    case.write(
        "crates/rsid/src/topology/tests.rs",
        &(gate("store-04", "executor_runs_the_planned_edges").replace(
            "fn executor_runs_the_planned_edges() {}",
            "fn executor_runs_the_planned_edges() { assert_eq!(run_plan(), 1); }",
        ) + "pub fn run_plan() -> u32 {\n    plan_edges()\n}\n"),
    );
    case.write(
        "crates/rsid/src/session/mod.rs",
        "pub(crate) mod graph_runner;\n",
    );
    case.write(
        "crates/rsid/src/session/graph_runner.rs",
        &(gate("session-01", "graph_runner_uses_the_planned_edges").replace(
            "fn graph_runner_uses_the_planned_edges() {}",
            "fn graph_runner_uses_the_planned_edges() { assert_eq!(runner_edges(), 1); }",
        ) + "pub fn runner_edges() -> u32 {\n    plan_edges()\n}\n"),
    );
    case.write(
        "crates/rsid/src/other.rs",
        &gate("other-01", "unrelated_daemon_test"),
    );
    case.commit();
    case.rebase_base_here();
    case
}

#[test]
fn a_planning_production_edit_selects_the_topology_and_graph_runner_observers() {
    let mut case = planning_case();
    edit(&mut case, PLANNING_LEAF, "    1\n}", "    2\n}");
    let r = receipt(&case);
    let filters = &r["suggested_filters"];
    assert!(
        strings(filters).contains(&"rsi-graph=planning::graph".to_owned()),
        "{filters}"
    );
    assert!(has(filters, "executor_runs_the_planned_edges"), "{filters}");
    assert!(
        has(filters, "graph_runner_uses_the_planned_edges"),
        "{filters}"
    );
    assert!(!has(filters, "unrelated_daemon_test"));
    let notes = r["leaf_observers"].as_array().unwrap();
    assert_eq!(
        notes.iter().map(|n| n["path"].clone()).collect::<Vec<_>>(),
        [json!(PLANNING_LEAF)]
    );
    assert_eq!(notes[0]["package"], json!("rsi-graph"));
    assert!(
        notes[0]["reason"]
            .as_str()
            .unwrap()
            .contains("production edit")
    );
}

#[test]
fn an_unprovable_planning_edit_derives_no_filter() {
    let mut case = planning_case();
    edit(
        &mut case,
        "crates/rsi-graph/src/planning/mod.rs",
        "pub use graph::plan_edges;",
        "pub use graph::{plan_edges, other_edges};",
    );
    let r = receipt(&case);
    assert_eq!(r["suggested_filters"], json!([]));
    assert_eq!(
        r["unfiltered_leaf_paths"],
        json!(["crates/rsi-graph/src/planning/mod.rs"])
    );
}

#[test]
fn a_planning_test_only_edit_selects_only_the_planning_tests() {
    let mut case = planning_case();
    edit(
        &mut case,
        PLANNING_LEAF,
        "assert_eq!(plan_edges(), 1);",
        "assert_eq!(plan_edges(), 1, \"one edge\");",
    );
    let r = receipt(&case);
    assert_eq!(r["suggested_filters"], json!(["rsi-graph=planning::graph"]));
    assert_eq!(r["leaf_observers"][0]["filters"], json!([]));
    assert!(
        r["leaf_observers"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("test code")
    );
}

#[test]
fn a_retained_daemon_owner_edit_selects_the_owner_tests_and_no_planning_filter() {
    let mut case = planning_case();
    edit(
        &mut case,
        "crates/rsid/src/topology/tests.rs",
        "    plan_edges()\n",
        "    plan_edges() + 0\n",
    );
    let r = receipt(&case);
    assert!(
        strings(&r["suggested_filters"])
            .iter()
            .any(|f| f.contains("executor_runs_the_planned_edges") || f.contains("topology")),
        "{}",
        r["suggested_filters"]
    );
    assert!(
        !strings(&r["suggested_filters"])
            .iter()
            .any(|f| f.starts_with("rsi-graph="))
    );
    assert_eq!(r["leaf_observers"], json!([]));
}

#[test]
fn an_edit_outside_planning_keeps_the_generic_dependency_treatment() {
    let mut case = planning_case();
    edit(
        &mut case,
        "crates/rsi-graph/src/compiler.rs",
        "    1\n}",
        "    2\n}",
    );
    assert_eq!(receipt(&case)["leaf_observers"], json!([]));
}

// -------------------------------------------- rotation, merge results and run modes

const ROTATION_PATH: &str = "crates/rsid/src/session/rotation.rs";

/// Two rotation shards sharing an enum and a large private test helper.
fn rotation_observation_fixture(observers: usize) -> (Case, String) {
    let mut case = Case::new();
    case.write("crates/rsid/src/lib.rs", "pub mod session;\n");
    case.write("crates/rsid/src/session/mod.rs", "mod rotation;\n");
    let mut body = String::from(
        "#[derive(Clone, Copy)]\nenum PrePublicationFailure {\n    Metadata,\n}\nimpl PrePublicationFailure {\n    fn refusal_code(self) -> &'static str {\n        \"metadata_failed\"\n    }\n}\nfn assert_rotation(failure: PrePublicationFailure) {\n",
    );
    body += &"    check();\n".repeat(65);
    body += "    failure.refusal_code();\n}\n";
    for shard in ["session-03", "session-04"] {
        let suffix = &shard[shard.len() - 2..];
        let names = numbered(&format!("observer_{suffix}_"), observers);
        let calls: Vec<(&str, &str)> = names
            .iter()
            .map(|n| (n.as_str(), "assert_rotation"))
            .collect();
        body += &gated_tests(shard, &names, &calls);
        body += &gated_tests(shard, &numbered(&format!("unrelated_{suffix}_"), 40), &[]);
    }
    body += &gated_tests("session-04", &["z_changed_acceptance".into()], &[]);
    let original = format!(
        "#[cfg(test)]\nmod tests {{\n{}}}\n",
        body.lines()
            .map(|row| format!("    {row}\n"))
            .collect::<String>()
    );
    case.write(ROTATION_PATH, &original);
    let refusal = numbered("other_refusal_", 11);
    let calls: Vec<(&str, &str)> = refusal
        .iter()
        .map(|n| (n.as_str(), "refusal_code"))
        .collect();
    case.write(
        "crates/rsid/src/unrelated.rs",
        &format!(
            "fn refusal_code() {{}}\n{}",
            gated_tests("session-03", &refusal, &calls)
        ),
    );
    case.commit();
    case.rebase_base_here();
    (case, original)
}

#[test]
fn rotation_enum_and_large_helper_dry_run_names_real_observers() {
    let (mut case, original) = rotation_observation_fixture(14);
    case.write(
        ROTATION_PATH,
        &original
            .replace(
                "        Metadata,",
                "        ContextRead,\n        Metadata,",
            )
            .replace("\"metadata_failed\"", "\"context_custody_failed\"")
            .replacen("        check();", "        verify();", 1)
            .replace(
                "fn z_changed_acceptance() {}",
                "fn z_changed_acceptance() { verify(); }",
            ),
    );
    case.commit();
    let r = receipt(&case);
    let mut picked = Vec::new();
    for shard in ["session-03", "session-04"] {
        for i in 0..14 {
            picked.push(super::filters::Observer {
                module: "session::rotation".into(),
                name: format!("observer_{}_{i}", &shard[shard.len() - 2..]),
                shard: shard.into(),
            });
        }
    }
    picked.push(super::filters::Observer {
        module: "session::rotation".into(),
        name: "z_changed_acceptance".into(),
        shard: "session-04".into(),
    });
    assert_eq!(
        strings(&r["suggested_filters"]),
        super::filters::traced_filters(&picked)
    );
    assert_eq!(r["broad"], json!(false));
    assert_eq!(r["narrowed_filters"][0]["tests"], json!(29));
    assert_eq!(
        r["priority_filters"],
        json!([
            "rsid=shard:session-04:test(/^session::rotation::(?:\\w+::)*z_changed_acceptance$/)"
        ])
    );
}

#[test]
fn enum_only_edit_traces_through_unchanged_large_test_helper() {
    let (mut case, original) = rotation_observation_fixture(1);
    case.write(
        ROTATION_PATH,
        &original.replace(
            "        Metadata,",
            "        ContextRead,\n        Metadata,",
        ),
    );
    case.commit();
    let r = receipt(&case);
    assert_eq!(
        r["suggested_filters"],
        json!([
            "rsid=shard:session-03:test(/^session::rotation::(?:\\w+::)*observer_03_0$/)",
            "rsid=shard:session-04:test(/^session::rotation::(?:\\w+::)*observer_04_0$/)"
        ])
    );
    assert_eq!(r["broad"], json!(false));
}

#[test]
fn unresolved_rotation_enum_keeps_one_conservative_module_filter() {
    let (mut case, original) = rotation_observation_fixture(14);
    case.write(
        ROTATION_PATH,
        &original
            .replace(
                "        Metadata,",
                "        ContextRead(String),\n        Metadata,",
            )
            .replace(
                "fn z_changed_acceptance() {}",
                "fn z_changed_acceptance() { verify(); }",
            ),
    );
    case.commit();
    let r = receipt(&case);
    assert_eq!(
        r["suggested_filters"],
        json!(["rsid=shard:session-03:test(session::rotation)"])
    );
    assert_eq!(r["broad"], json!(true));
    assert_eq!(r["broad_filters"].as_array().unwrap().len(), 1);
    assert_eq!(r["broad_filters"][0]["tests"], json!(109));
    assert_eq!(
        r["merged_filters"][0]["covered_by"],
        r["suggested_filters"][0]
    );
    assert_eq!(
        r["priority_filters"],
        json!([
            "rsid=shard:session-04:test(/^session::rotation::(?:\\w+::)*z_changed_acceptance$/)"
        ])
    );
}

#[test]
fn field_addition_names_split_type_observers_and_keeps_replay() {
    // #2142: common observation fields must resolve through the journal type, while the
    // include!-mounted migration catalog keeps replay coverage without selecting the whole
    // store namespace.
    let mut case = Case::new();
    case.shards = vec![
        Shard {
            name: "store-01".into(),
            packages: vec!["rsid-store".into()],
        },
        Shard {
            name: "store-02".into(),
            packages: vec!["rsid-store".into()],
        },
        Shard {
            name: "session-05".into(),
            packages: vec!["rsid".into()],
        },
        Shard {
            name: "other-03".into(),
            packages: vec!["rsid".into()],
        },
    ];
    let replay = rewind_fixture(&mut case);
    case.write("crates/rsid-store/src/lib.rs", "pub mod store;\n");
    case.write(
        "crates/rsid-store/src/store/mod.rs",
        "pub mod journal;\nmod tests;\ninclude!(\"migrations/altered_objects.rs\");\n",
    );
    let catalog = "pub const MIGRATION_ALTERED_OBJECTS: &[u32] = &[150];\n";
    let catalog_path = "crates/rsid-store/src/store/migrations/altered_objects.rs";
    case.write(catalog_path, catalog);
    case.write(
        "crates/rsid-store/src/store/migrations/v150.rs",
        "// v150\n",
    );
    let root = "crates/rsid-store/src/store/journal.rs";
    let original = "pub struct JournalItem {\n    pub sequence: u64,\n}\n";
    case.write(root, original);
    for shard in 1..=4 {
        case.write(
            &format!("crates/rsid-store/src/store/unrelated_{shard}.rs"),
            &gated_tests(
                &format!("store-0{shard}"),
                &numbered(&format!("unrelated_{shard}_"), 70),
                &[],
            ),
        );
    }
    let consumer = "crates/rsid/src/session/journal.rs";
    let observers = format!(
        "fn load_journal() -> JournalItem {{\n    load_from_store()\n}}\nfn observe_journal() {{\n    load_journal();\n}}\n{}",
        gated_tests(
            "session-05",
            &["journal_observer".into(), "other_test".into()],
            &[("journal_observer", "observe_journal")]
        )
    );
    case.write(consumer, &observers);
    let common = numbered("common_", 11);
    let calls: Vec<(&str, &str)> = common
        .iter()
        .map(|n| (n.as_str(), "refusal_code"))
        .collect();
    case.write(
        "crates/rsid/src/common_fields.rs",
        &gated_tests("other-03", &common, &calls),
    );
    case.commit();
    case.rebase_base_here();
    case.write(
        root,
        &original.replace(
            "    pub sequence",
            "    pub refusal_code: Option<String>,\n    pub sequence",
        ),
    );
    case.commit();
    let field_only = receipt(&case);
    assert_eq!(
        field_only["suggested_filters"],
        json!(["rsid=shard:session-05:test(/^session::journal::(?:\\w+::)*journal_observer$/)"])
    );
    assert_eq!(field_only["broad"], json!(false));
    case.write(catalog_path, &catalog.replace("150]", "150, 151]"));
    case.write(
        consumer,
        &(observers.replace("fn other_test() {}", "fn other_test() { changed(); }")
            + &gated_tests(
                "session-05",
                &["z_added_acceptance".into()],
                &[("z_added_acceptance", "observe_journal")],
            )),
    );
    case.write(
        REWIND_PATH,
        &replay
            .replace("156;", "157;")
            .replace("old_tail();", "new_tail();"),
    );
    case.commit();
    let r = receipt(&case);
    let mut expected = set_of(&[
        "rsid=shard:session-05:test(session::journal)",
        "rsid=shard:store-02:test(external_migration)",
    ]);
    for name in [
        "migration_old",
        "migration_tail",
        "migration_replay",
        "migration_indirect",
        "schema_rewinds_correctly",
        "fixture_guard_rejects_drift",
    ] {
        expected.insert(format!("rsid=shard:store-01:test({name})"));
    }
    assert_eq!(
        filter_set(&Checked {
            code: 0,
            summary: String::new(),
            receipt: r.clone(),
            stderr: vec![]
        }),
        expected
    );
    assert_eq!(
        r["priority_filters"],
        json!([
            "rsid=shard:session-05:test(/^session::journal::(?:\\w+::)*(?:other_test|z_added_acceptance)$/)"
        ])
    );
    assert_eq!(r["broad"], json!(false));
}

#[test]
fn test_only_fixture_edit_with_quote_ending_comment_names_its_tests_and_no_bin() {
    // #2434: a `//` comment ending in a quote desynced the lexer for the rest of the file, so a
    // helper extraction in a test module went broad and a bin source scanner was built for a
    // diff that touches no production source.
    let tests = |edit: &str| {
        format!(
            "// Regression: the CLI echoes results as a \"user\"\nfn expect_user() {{ let _ = \"user\".to_string(); }}\n{}fn helper() {{ {edit} }}\n{}",
            gated_tests("session-03", &numbered("filler_", 70), &[]),
            gate("session-03", "uses_helper").replacen("{}", "{ helper(); }", 1)
        )
    };
    let mut case = Case::new();
    case.write("crates/rsid/src/lib.rs", "pub mod session;\n");
    case.write("crates/rsid/src/session/tests.rs", &tests("let _ = 1;"));
    case.write(
        "crates/rsid/src/bin/rsi-model-control-validate.rs",
        "fn fixture_root() -> String { let _ = \"crates/rsid/src\"; std::env::var(\"CARGO_MANIFEST_DIR\").unwrap() }\n#[test]\nfn validates() { let _ = fixture_root(); }\n",
    );
    case.commit();
    case.rebase_base_here();
    case.write("crates/rsid/src/session/tests.rs", &tests("let _ = 2;"));
    case.commit();
    let r = case.check(&["--filters-only"]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(r.receipt["broad_filters"], json!([]), "{}", r.summary);
    assert!(
        !strings(&r.receipt["suggested_filters"])
            .iter()
            .any(|f| f.contains("=bin:")),
        "{}",
        r.receipt["suggested_filters"]
    );
    assert!(
        has(&r.receipt["suggested_filters"], "uses_helper"),
        "{}",
        r.receipt["suggested_filters"]
    );
}

#[test]
fn several_merge_bases_diff_the_merge_result_not_the_stale_side() {
    // #2814: rolling moved on from a file the branch merged earlier, so the branch still
    // carries the older bytes. The merge result keeps rolling's version, so only the branch's
    // docs change is the candidate: docs-only.
    let mut case = Case::new();
    let (worker_first, rolling_first) = criss_cross(&mut case);
    // drop "worker last": the worker tip is its merge of rolling's first commit.
    let worker_merged = case.head.clone();
    case.write("docs/note.md", "# note\n");
    let worker_docs = case.commit_on(&[&worker_merged]);
    // rolling moves on from the merged state.
    let rolling_merged = case.base.clone();
    case.remove("docs/note.md");
    case.write("crates/rsid/src/alpha.rs", &gate("other-03", "alpha2"));
    case.write("crates/rsid/src/beta.rs", &gate("store-02", "beta2"));
    let moved = case.commit_on(&[&rolling_merged]);
    case.base = moved.clone();
    case.head = worker_docs.clone();
    let bases = case.fx.git(&["merge-base", "--all", &moved, &worker_docs]);
    assert_eq!(
        bases.split_whitespace().count(),
        2,
        "{bases} {worker_first} {rolling_first}"
    );
    let r = case.check(&["--no-compile"]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(r.receipt["changed_files"], json!(1));
    assert_eq!(r.receipt["shards"]["map"], json!({}));
    assert_eq!(r.receipt["suggested_filters"], json!([]));
}

fn clean_case() -> (Case, std::path::PathBuf) {
    let mut case = split_store_case();
    let log = fake_cargo(&mut case, "");
    case.write(
        "crates/rsid/Cargo.toml",
        &format!("[package]\nname = \"rsid\"\n{RSID_MANIFEST}"),
    );
    case.write(
        "crates/rsid-store/Cargo.toml",
        &format!("[package]\nname = \"rsid-store\"\n{STORE_MANIFEST}"),
    );
    case.write(
        "crates/rsid-store/src/store/q.rs",
        &(gate("store-01", "c") + "const EDIT: u8 = 1;\n"),
    );
    case.commit();
    (case, log)
}

#[test]
fn an_opted_in_clean_runs_first_for_a_target_under_the_repo() {
    let (mut case, log) = clean_case();
    let target = case.fx.path().join("target");
    case.env = vec![
        ("CHECK_TOUCHED_SHARDS_CLEAN".into(), "1".into()),
        (
            "CARGO_TARGET_DIR".into(),
            target.to_string_lossy().into_owned(),
        ),
    ];
    std::fs::create_dir_all(&target).unwrap();
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    let commands = cargo_commands(&log);
    assert_eq!(commands[0], "clean -p rsid -p rsid-store");
    assert!(
        commands[1..].iter().all(|c| c.starts_with("check ")),
        "{commands:?}"
    );
}

#[test]
fn an_opted_in_clean_never_touches_a_target_outside_the_repo() {
    let (mut case, log) = clean_case();
    let outside = tempfile::tempdir().unwrap();
    case.env = vec![
        ("CHECK_TOUCHED_SHARDS_CLEAN".into(), "1".into()),
        (
            "CARGO_TARGET_DIR".into(),
            outside
                .path()
                .join("shared-target")
                .to_string_lossy()
                .into_owned(),
        ),
    ];
    let r = case.check(&[]);
    assert_eq!(r.code, 0, "{}", r.summary);
    let commands = cargo_commands(&log);
    assert!(
        !commands.is_empty() && commands.iter().all(|c| c.starts_with("check ")),
        "{commands:?}"
    );
    assert!(r.stderr.join("\n").contains("not cleaning"));
}

#[test]
fn explicit_sharded_execution_keeps_both_shard_expressions() {
    let case_for = || wide_module_fixture(40);
    let mut case = case_for();
    let log = fake_cargo(&mut case, "");
    let r = case.check(&["--run-filters"]);
    assert_eq!(r.code, 0, "{}", r.summary);
    assert_eq!(
        r.receipt["filter_runs"]["ok"],
        json!([
            "rsid=shard:other-03:test(session)",
            "rsid=shard:session-01:test(session)"
        ])
    );
    let commands = cargo_commands(&log);
    assert!(commands.iter().any(|c| c.starts_with("nextest run -p rsid --lib --no-default-features --features test-shard-other-03 -E test(session)")), "{commands:?}");
}

#[test]
fn failed_compile_bounds_error_blocks_by_lines_and_characters() {
    for width in [80usize, 500] {
        let mut case = split_store_case();
        let first = "error[E0609]: first error\n  --> foo.rs:12:7\n";
        let second = "error: second error\n  --> bar.rs:20:3\n";
        let mut output = format!("{first}warning: between errors\n{second}");
        output += &format!("   | {}\n", "x".repeat(width)).repeat(100);
        output += &"warning: final compiler context\n".repeat(100);
        fake_cargo(
            &mut case,
            &format!("cat >&2 <<'DIAGNOSTICS'\n{output}DIAGNOSTICS\nexit 101\n"),
        );
        case.write(
            "crates/rsid/src/foo.rs",
            &(gate("store-02", "a") + "const EDIT: u8 = 1;\n"),
        );
        case.commit();
        let r = case.check(&[]);
        assert_eq!(r.code, 1);
        let stderr = r.stderr.join("\n");
        let excerpt = stderr[stderr.find(first).unwrap()..]
            .split("\n\nstderr tail:\n")
            .next()
            .unwrap()
            .to_owned();
        assert!(
            excerpt.contains(first) && excerpt.contains(second),
            "{width}"
        );
        assert!(
            excerpt.lines().count() <= 40,
            "{width}: {}",
            excerpt.lines().count()
        );
        assert!(excerpt.chars().count() <= 4000);
        assert!(stderr.contains("warning: final compiler context"));
        let rest: usize = r
            .stderr
            .iter()
            .filter(|l| !l.starts_with("check-touched-shards: ["))
            .map(|l| l.len() + 1)
            .sum();
        assert!(rest < 6500, "{width}: {rest}");
    }
}

#[test]
fn emitted_filters_are_the_shapes_the_lander_fixture_pins() {
    // #1510: rsi-rolling-land parses every line of this fixture. A shape the derivation
    // starts to emit that is missing here fails this test, so the two cannot drift apart.
    let fixture: BTreeSet<String> = std::fs::read_to_string(
        std::path::Path::new(rsi_common::manifest_dir!())
            .join("../../scripts/tests/fixtures/touched-shard-filters.txt"),
    )
    .unwrap()
    .lines()
    .map(str::to_owned)
    .collect();
    let mut emitted: BTreeSet<String> = BTreeSet::new();
    let mut case = Case::new();
    case.write("crates/rsid/src/rpc/agent_x.rs", &gate("other-03", "p"));
    case.commit();
    emitted.extend(strings(&case.check(&[]).receipt["suggested_filters"]));
    session_fixture(&mut case, 70);
    case.write(
        "crates/rsid/src/session/mod.rs",
        "mod big;\npub const LIMIT: u32 = 1;\npub fn used() { }\npub fn unused() {}\n",
    );
    case.commit();
    emitted.extend(strings(&case.check(&[]).receipt["suggested_filters"]));
    case.rebase_base_here();
    manager_actions_fixture(&mut case, &["a1".into(), "a2".into()]);
    case.write(
        "crates/rsid/src/session/tests/manager_actions.rs",
        &format!(
            "{}#[path = \"manager_issue_worker.rs\"]\nmod issue_worker;\nconst EDIT: u8 = 1;\n",
            gated_tests("other-03", &["a1".into(), "a2".into()], &[])
        ),
    );
    case.commit();
    emitted.extend(strings(&case.check(&[]).receipt["suggested_filters"]));
    case.rebase_base_here();
    anchored_module_fixture(&mut case);
    emitted.extend(strings(&case.check(&[]).receipt["suggested_filters"]));
    assert!(!emitted.is_empty(), "the synthetic diffs emitted no filter");
    let missing: Vec<_> = emitted.difference(&fixture).collect();
    assert!(
        missing.is_empty(),
        "update scripts/tests/fixtures/touched-shard-filters.txt: {missing:?}"
    );
}

#[test]
fn the_desktop_selection_agrees_with_the_desktop_test_gate() {
    use crate::desktop_selection::{selected, static_inputs};
    let inputs = static_inputs();
    for (changed, expected) in [
        (vec![], false),
        (vec!["desktop/a"], true),
        (vec!["docs/desktop/a"], false),
        (vec!["desktop-notes.md"], false),
        (vec!["x", "desktop/b/c"], true),
    ] {
        let paths: Vec<String> = changed.iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(selected(&paths, &inputs), expected, "{changed:?}");
        let mut case = Case::empty();
        case.write(
            "scripts/desktop-test-gate.py",
            "# the desktop lane's selector (stub)\n",
        );
        case.commit();
        case.rebase_base_here();
        for path in &changed {
            case.write(path, "x\n");
        }
        case.commit();
        if changed.is_empty() {
            continue;
        }
        let lane = case.check(&["--filters-only"]).receipt["desktop"]["selected"].clone();
        assert_eq!(lane, json!(expected), "{changed:?}");
    }
}

// ------------------------------------------------- unusual path names keep hunk coverage (#456)

const SPACED_LEAF: &str = "crates/rsid-remote-read/src/projection part.rs";

fn spaced_leaf_case() -> Case {
    let mut case = Case::empty();
    case.write(
        "crates/rsid-remote-read/src/lib.rs",
        "#[path = \"projection part.rs\"]\nmod projection;\npub use projection::project_pending;\n",
    );
    case.write(
        SPACED_LEAF,
        "fn private_projection() -> u32 {\n    1\n}\n\npub fn project_pending() -> u32 {\n    private_projection()\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn projects_one_pending_item() {\n        assert_eq!(project_pending(), 1);\n    }\n}\n",
    );
    case.write(
        "crates/rsid/src/lib.rs",
        "pub mod remote_read;\npub mod session;\npub mod other;\n",
    );
    case.write(
        "crates/rsid/src/remote_read/mod.rs",
        "mod owner;\npub use rsid_remote_read::*;\n",
    );
    case.write(OWNER_PATH, &owner_text());
    case.write("crates/rsid/src/session/mod.rs", "mod queries;\n");
    case.write(
        "crates/rsid/src/session/queries.rs",
        &(gate("session-01", "session_probe_sees_the_projection").replace(
            "fn session_probe_sees_the_projection() {}",
            "fn session_probe_sees_the_projection() { assert_eq!(session_probe(), 1); }",
        ) + "pub fn session_probe() -> u32 {\n    project_pending()\n}\n"),
    );
    case.write(
        "crates/rsid/src/other.rs",
        &gate("other-01", "unrelated_daemon_test"),
    );
    case.commit();
    case.rebase_base_here();
    case
}

#[test]
fn a_private_edit_in_a_file_with_a_space_in_its_name_reaches_the_daemon_observers() {
    let mut case = spaced_leaf_case();
    edit(
        &mut case,
        SPACED_LEAF,
        "    1\n}\n\npub fn",
        "    2\n}\n\npub fn",
    );
    let loaded = case.load(&case.base, &case.head).unwrap();
    assert!(
        loaded.hunks.get(SPACED_LEAF).is_some_and(|h| !h.is_empty()),
        "{:?}",
        loaded.hunks
    );
    let r = receipt(&case);
    let filters = strings(&r["suggested_filters"]);
    assert!(
        filters.contains(&"rsid=shard:store-04:test(remote_read)".to_owned()),
        "{filters:?}"
    );
    assert!(
        filters
            .iter()
            .any(|f| f.contains("session_probe_sees_the_projection")),
        "{filters:?}"
    );
    assert!(!filters.iter().any(|f| f.contains("unrelated_daemon_test")));
}

#[test]
fn hunks_and_added_lines_survive_every_unusual_path_name() {
    let names: Vec<&str> = if cfg!(unix) {
        vec![
            "crates/x/src/with space.rs",
            "crates/x/src/caf\u{e9}.rs",
            "crates/x/src/tab\tname.rs",
            "crates/x/src/quote\"name.rs",
            "crates/x/src/back\\slash.rs",
            "crates/x/src/trailing space .rs",
        ]
    } else {
        vec!["crates/x/src/with space.rs", "crates/x/src/caf\u{e9}.rs"]
    };
    let mut case = Case::empty();
    for name in &names {
        case.write(name, "pub fn a() {\n    1\n}\n");
    }
    case.write("crates/x/src/gone file.rs", "pub fn gone() {\n    1\n}\n");
    case.commit();
    case.rebase_base_here();
    for name in &names {
        case.write(name, "pub fn a() {\n    2\n}\n");
    }
    case.remove("crates/x/src/gone file.rs");
    case.write("crates/x/src/new file.rs", "pub fn fresh() {}\n");
    case.commit();
    let loaded = case.load(&case.base, &case.head).unwrap();
    for name in &names {
        let hunks = loaded.hunks.get(*name).unwrap_or_else(|| {
            panic!("no hunks for {name:?}: {:?}", loaded.hunks.keys());
        });
        assert_eq!(hunks.len(), 1, "{name:?}");
        assert_eq!((hunks[0].old_first, hunks[0].new_first), (2, 2), "{name:?}");
        assert_eq!(
            loaded.added_lines.get(*name),
            Some(&vec![(2, "    2".to_owned())]),
            "{name:?}"
        );
    }
    let gone = &loaded.hunks["crates/x/src/gone file.rs"];
    assert_eq!(gone.len(), 1, "a deleted file keeps its deletion hunk");
    assert_eq!(
        (gone[0].old_first, gone[0].old_count, gone[0].new_count),
        (1, 3, 0)
    );
    assert!(loaded.hunks.contains_key("crates/x/src/new file.rs"));
    assert_eq!(
        loaded.added_lines["crates/x/src/new file.rs"],
        vec![(1, "pub fn fresh() {}".to_owned())]
    );
}

// ------------------------------------- isolated-table proof resolves its own names (#457)

#[test]
fn an_index_on_a_missing_column_keeps_every_rewind_caller() {
    let mut case = Case::new();
    let r = additive_rewind_change(
        &mut case,
        "impl Store {\n    fn migrate_v151(&self) {\n        tx.execute_batch(\"CREATE TABLE fresh(id INTEGER); CREATE INDEX fresh_bad ON fresh(missing_column);\");\n        tx.pragma_update(None, \"user_version\", 151);\n    }\n}\n",
    );
    assert_eq!(r.code, 0, "{}", r.summary);
    let filters = filter_set(&r);
    assert!(
        filters.contains("rsid=shard:store-01:test(migration_old)"),
        "{filters:?}"
    );
    assert!(
        filters.contains("rsid=shard:store-02:test(external_migration)"),
        "{filters:?}"
    );
}

#[test]
fn the_isolated_table_proof_accepts_only_ddl_whose_every_name_resolves() {
    let classify = super::migration::additive_migration;
    let wrap = |sql: &str| {
        format!(
            "impl Store {{ fn migrate_v151(&self) {{ tx.execute_batch(\"{}\"); }} }}",
            sql.replace('"', "\\\"")
        )
    };
    let table = "CREATE TABLE t(id TEXT PRIMARY KEY, state TEXT, n INTEGER);";
    // Shapes SQLite accepts stay proven.
    for sql in [
        table.to_owned(),
        format!("{table} CREATE INDEX t_state ON t(state);"),
        format!("{table} CREATE UNIQUE INDEX t_n ON t(n DESC, state COLLATE NOCASE);"),
        format!("{table} CREATE INDEX t_q ON t(\"state\");"),
        "CREATE TABLE a(id TEXT PRIMARY KEY, b_id TEXT REFERENCES b(id) ON DELETE RESTRICT);".into(),
        "CREATE TABLE w(id TEXT, k TEXT, PRIMARY KEY(id, k)) WITHOUT ROWID;".into(),
        "CREATE TABLE s(id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT NOT NULL DEFAULT '');".into(),
        "CREATE TABLE f(a TEXT, b TEXT, UNIQUE(a, b), FOREIGN KEY(a, b) REFERENCES p(x, y) ON DELETE CASCADE);".into(),
        "CREATE TABLE c(id TEXT CHECK(rsi_uuid_is_canonical(id)), n INTEGER CHECK(n >= 0 AND length(id) = 36));".into(),
        format!("{table} CREATE TRIGGER t_guard BEFORE UPDATE ON t WHEN NEW.state='bad' BEGIN SELECT RAISE(ABORT,'bad'); END;"),
    ] {
        assert!(classify(&wrap(&sql)), "should stay proven: {sql}");
    }
    // Everything SQLite rejects, or this grammar cannot resolve, is not proven.
    for sql in [
        format!("{table} CREATE INDEX bad ON t(missing_column);"),
        format!("{table} CREATE INDEX bad ON t(state, nothing);"),
        format!("{table} CREATE UNIQUE INDEX bad ON t(\"nothing\");"),
        format!("{table} CREATE INDEX bad ON t(lower(state));"),
        format!("{table} CREATE INDEX bad ON t(state) WHERE nothing = 1;"),
        format!("{table} CREATE INDEX bad ON t(state COLLATE nonsense);"),
        format!("{table} CREATE INDEX t_state ON t(state); CREATE INDEX t_state ON t(n);"),
        format!("{table} CREATE INDEX t ON t(state);"),
        format!("{table} {table}"),
        "CREATE TABLE d(a TEXT, a INTEGER);".into(),
        "CREATE TABLE d(a TEXT, PRIMARY KEY(missing));".into(),
        "CREATE TABLE d(a TEXT, UNIQUE(missing));".into(),
        "CREATE TABLE d(a TEXT PRIMARY KEY, b TEXT PRIMARY KEY);".into(),
        "CREATE TABLE d(a TEXT, FOREIGN KEY(missing) REFERENCES p(x));".into(),
        "CREATE TABLE d(a TEXT, b TEXT, FOREIGN KEY(a, b) REFERENCES p(x));".into(),
        "CREATE TABLE d(a TEXT) WITHOUT ROWID;".into(),
        "CREATE TABLE d(a TEXT PRIMARY KEY AUTOINCREMENT);".into(),
        "CREATE TABLE d(a TEXT CHECK(missing > 0));".into(),
        "CREATE TABLE d(a TEXT CHECK(no_such_function(a)));".into(),
        "CREATE TABLE d(a TEXT CHECK(length(a, a)));".into(),
        "CREATE TABLE d(a TEXT CHECK(random() > 0));".into(),
        "CREATE TABLE d(a TEXT CHECK(a IN (SELECT 1)));".into(),
        "CREATE TABLE d(a TEXT, b TEXT DEFAULT (a));".into(),
        "CREATE TABLE d(a TEXT DEFAULT nothing);".into(),
        "CREATE TABLE d(a TEXT COLLATE nonsense);".into(),
        "CREATE TABLE d(a TEXT, b TEXT GENERATED ALWAYS AS (a) VIRTUAL);".into(),
        "CREATE TABLE d(a TEXT) STRICT;".into(),
        "CREATE TABLE d();".into(),
        "CREATE TABLE sqlite_x(a TEXT);".into(),
        format!(
            "{table} CREATE TRIGGER g BEFORE UPDATE ON t WHEN NEW.nothing = 1 BEGIN SELECT RAISE(ABORT,'x'); END;"
        ),
        format!(
            "{table} CREATE TRIGGER g BEFORE UPDATE ON t WHEN state = 'x' BEGIN SELECT RAISE(ABORT,'x'); END;"
        ),
        format!(
            "{table} CREATE TRIGGER g INSTEAD OF UPDATE ON t BEGIN SELECT RAISE(ABORT,'x'); END;"
        ),
        format!(
            "{table} CREATE TRIGGER g BEFORE UPDATE OF nothing ON t BEGIN SELECT RAISE(ABORT,'x'); END;"
        ),
        format!(
            "{table} CREATE TRIGGER g BEFORE UPDATE ON t BEGIN SELECT RAISE(ABORT,'x'); END; CREATE TRIGGER g AFTER INSERT ON t BEGIN SELECT RAISE(ABORT,'y'); END;"
        ),
    ] {
        assert!(!classify(&wrap(&sql)), "must not be proven: {sql}");
    }
}

// ------------------------- the isolated-table proof parses every expression (#462)

fn wrap_migration_sql(sql: &str) -> String {
    format!(
        "impl Store {{ fn migrate_v151(&self) {{ tx.execute_batch(\"{}\"); }} }}",
        sql.replace('"', "\\\"")
    )
}

#[test]
fn a_malformed_check_expression_keeps_every_rewind_caller() {
    for sql in [
        "CREATE TABLE fresh(id INTEGER CHECK(id +));",
        "CREATE INDEX IF definitely broken idx ON fresh(id);",
    ] {
        let sql = if sql.starts_with("CREATE INDEX") {
            format!("CREATE TABLE fresh(id INTEGER); {sql}")
        } else {
            sql.to_owned()
        };
        let mut case = Case::new();
        let r = additive_rewind_change(
            &mut case,
            &format!(
                "impl Store {{\n    fn migrate_v151(&self) {{\n        tx.execute_batch(\"{sql}\");\n        tx.pragma_update(None, \"user_version\", 151);\n    }}\n}}\n"
            ),
        );
        assert_eq!(r.code, 0, "{}", r.summary);
        let filters = filter_set(&r);
        assert!(
            filters.contains("rsid=shard:store-01:test(migration_old)"),
            "{sql}: {filters:?}"
        );
        assert!(
            filters.contains("rsid=shard:store-02:test(external_migration)"),
            "{sql}: {filters:?}"
        );
    }
}

#[test]
fn the_proof_parses_expressions_instead_of_checking_tokens() {
    let classify = super::migration::additive_migration;
    let check = |e: &str| {
        wrap_migration_sql(&format!(
            "CREATE TABLE t(id TEXT, n INTEGER, s TEXT, CHECK({e}));"
        ))
    };
    let column = |e: &str| {
        wrap_migration_sql(&format!(
            "CREATE TABLE t(id TEXT, s TEXT, n INTEGER CHECK({e}));"
        ))
    };
    let default = |e: &str| wrap_migration_sql(&format!("CREATE TABLE t(id TEXT DEFAULT ({e}));"));
    let valid = [
        "n >= 0 AND n < 10",
        "(id)",
        "((n)) > 0",
        "id IS NOT NULL",
        "n IN (1, 2, 3)",
        "n NOT IN (1)",
        "n BETWEEN 1 AND 5 OR n IS NULL",
        "length(trim(lower(s))) > 0",
        "s <> '(' AND s <> ')' AND s <> '+ - *' AND s <> 'a OR (b'",
        "\"id\" <> '' AND [n] > 0 AND `s` IS NULL",
        "id /* a + ( */ <> '' -- tail\n",
        "-n < 0 AND +n > -1",
        "NOT (n = 1)",
        "n % 2 = 0 AND n * 2 + 1 <= 9 AND n / 1 >= 0",
        "s LIKE 'a%'",
        "s NOT LIKE 'a%'",
        "s GLOB 'a*'",
        "1.5 < n AND .5 < n AND 1e3 > n AND 1. < n",
        "typeof(n) = 'integer'",
        "substr(s, 1, 2) = 'ab' AND instr(s, 'a') > 0",
        "n == 1 OR n != 2 OR n <> 3 OR n = 4",
        "rsi_uuid_is_canonical(id)",
    ];
    for e in valid {
        assert!(classify(&check(e)), "table CHECK should stay proven: {e}");
        assert!(classify(&column(e)), "column CHECK should stay proven: {e}");
    }
    let invalid = [
        "id +",
        "+",
        "* 1",
        "1 +",
        "(id",
        "id)",
        "((id)",
        "id = = 1",
        "id ==",
        "id AND",
        "OR id",
        "NOT",
        "id NOT",
        "id IS",
        "id IS NOT",
        "n BETWEEN 1",
        "n BETWEEN 1 AND",
        "n BETWEEN AND 1",
        "id IN ()",
        "n IN (1,)",
        "n IN (,1)",
        "n IN (1 2)",
        "n IN 1",
        "n IN (SELECT 1)",
        "1 2",
        "id id",
        "id n",
        "length()",
        "length(id,)",
        "length(,id)",
        "length(id id)",
        "length(id",
        "lower(upper(id)",
        "coalesce(id)",
        "CASE WHEN n > 0 THEN 1",
        "CASE WHEN n > 0 THEN 1 ELSE 2",
        "CASE END",
        "CASE WHEN THEN 1 END",
        "CASE WHEN n THEN END",
        "CASE n WHEN 1 THEN 2 ELSE END",
        "CASE WHEN n THEN 1 ELSE 2 END END",
        "n >=",
        "n > > 1",
        "n < = 1",
        "n ! = 1",
        "n = > 1",
        "n <=> 1",
        "id ||",
        "id | | s",
        "id ||| s",
        "s COLLATE",
        "s COLLATE bogus",
        "CAST(n)",
        "CAST(n AS)",
        "CAST(n TEXT)",
        "CAST(n AS TEXT",
        "1 . 5",
        "1.5.5",
        "n = 1abc",
        "n = 0x",
        "id LIKE",
        "id LIKE 'a' ESCAPE",
        "id FOO 1",
        "select 1",
        "n IN (1, select)",
        "random() = 1",
        "id = ?",
        "id = :p",
        "id = $p",
        "id = ?1",
        "(id, n) = (1, 2)",
        "id,",
        ",",
        "COUNT(*)",
        "EXISTS (SELECT 1)",
        "id.n = 1",
        "main.t.id = 1",
        "'unterminated",
        "n = 1 THEN",
        "n = 1 END",
        "n = 1 ELSE 2",
        "NOT NOT",
        "- - -",
        "()",
        "( )",
        "id IS NOT DISTINCT",
    ];
    for e in invalid {
        assert!(!classify(&check(e)), "table CHECK must not be proven: {e}");
        assert!(
            !classify(&column(e)),
            "column CHECK must not be proven: {e}"
        );
    }
    for e in ["1 +", "()", "(1 2)", "1 +  ", "(1", "length()", "id", "n"] {
        assert!(
            !classify(&default(e)),
            "DEFAULT expression must not be proven: {e}"
        );
    }
    for e in ["1", "1 + 2", "lower('A')", "length('x') > 0"] {
        assert!(classify(&default(e)), "DEFAULT should stay proven: {e}");
    }
}

#[test]
fn a_trigger_condition_and_body_must_parse_in_full() {
    let classify = super::migration::additive_migration;
    let table = "CREATE TABLE t(id TEXT PRIMARY KEY, n INTEGER, state TEXT);";
    let trigger = |when: &str, body: &str| {
        wrap_migration_sql(&format!(
            "{table} CREATE TRIGGER g BEFORE UPDATE ON t {when} BEGIN {body} END;"
        ))
    };
    let raise = "SELECT RAISE(ABORT,'bad');";
    for when in [
        "",
        "WHEN NEW.id IS NOT OLD.id",
        "WHEN NEW.n > OLD.n AND NEW.state <> OLD.state",
        "WHEN NEW.state = '(' OR NEW.state = ')'",
        "WHEN (NEW.n + 1) * 2 > OLD.n",
        "WHEN length(NEW.id) = 36 /* c */",
    ] {
        assert!(
            classify(&trigger(when, raise)),
            "should stay proven: {when}"
        );
    }
    for when in [
        "WHEN NEW.id +",
        "WHEN NEW.id IS NOT OLD.id AND",
        "WHEN (NEW.id",
        "WHEN NEW.n =",
        "WHEN",
        "WHEN NEW.id OLD.id",
        "WHEN NEW.nothing = 1",
        "WHEN NEW. = 1",
        "WHEN NEW.id IN ()",
        "WHEN CASE WHEN NEW.n THEN 1",
    ] {
        assert!(
            !classify(&trigger(when, raise)),
            "must not be proven: {when}"
        );
    }
    for body in [
        "SELECT RAISE(ABORT,'a'); SELECT RAISE(ABORT,'b');",
        "SELECT RAISE(IGNORE);",
        "SELECT RAISE(ABORT,'a')",
        "UPDATE sessions SET state = ( 'q' );",
        "",
    ] {
        assert!(
            !classify(&trigger("", body)),
            "body must not be proven: {body}"
        );
    }
    assert!(!classify(&wrap_migration_sql(&format!(
        "{table} CREATE TRIGGER IF broken g BEFORE UPDATE ON t BEGIN {raise} END;"
    ))));
    assert!(classify(&wrap_migration_sql(&format!(
        "{table} CREATE TRIGGER IF NOT EXISTS g BEFORE UPDATE ON t BEGIN {raise} END;"
    ))));
    assert!(!classify(&wrap_migration_sql(&format!(
        "{table} CREATE TRIGGER g BEFORE UPDATE ON main.t BEGIN {raise} END;"
    ))));
}

#[test]
fn if_clauses_qualified_targets_and_name_folding_follow_sqlite() {
    let classify = super::migration::additive_migration;
    let sql = |s: &str| classify(&wrap_migration_sql(s));
    let table = "CREATE TABLE t(id INTEGER);";
    assert!(sql(&format!("{table} CREATE INDEX i ON t(id);")));
    assert!(sql(&format!(
        "{table} CREATE INDEX IF NOT EXISTS i ON t(id);"
    )));
    assert!(sql(&format!("{table} CREATE INDEX main.i ON t(id);")));
    assert!(sql("CREATE TABLE main.t(id INTEGER);"));
    for bad in [
        "CREATE INDEX IF definitely broken idx ON t(id);",
        "CREATE INDEX IF NOT i ON t(id);",
        "CREATE INDEX IF EXISTS i ON t(id);",
        "CREATE INDEX main.i ON main.t(id);",
        "CREATE INDEX other.i ON t(id);",
        "CREATE INDEX i ON t.x(id);",
    ] {
        assert!(!sql(&format!("{table} {bad}")), "must not be proven: {bad}");
    }
    // SQLite folds ASCII case only: `Ä` and `ä` are different columns.
    assert!(!sql(
        "CREATE TABLE t(\"\u{c4}\" INTEGER CHECK(\u{e4} > 0));"
    ));
    assert!(sql("CREATE TABLE t(\"\u{e4}\" INTEGER CHECK(\u{e4} > 0));"));
    assert!(sql("CREATE TABLE t(Id INTEGER CHECK(iD > 0));"));
    // Reserved words and malformed names are not plain identifiers.
    for bad in [
        "CREATE TABLE select(id TEXT);",
        "CREATE TABLE t(select TEXT);",
        "CREATE TABLE t(id TEXT, from TEXT);",
        "CREATE TABLE t(1abc TEXT);",
        "CREATE TABLE t(id select);",
        "CREATE TABLE t(id TEXT CHECK(id = from));",
        "CREATE TABLE t(id TEXT AUTOINCREMENT);",
        "CREATE TABLE t(\"\" TEXT);",
    ] {
        assert!(!sql(bad), "must not be proven: {bad}");
    }
    assert!(sql(
        "CREATE TABLE t(\"select\" TEXT, key TEXT, action TEXT);"
    ));
}

#[test]
fn number_lookalikes_and_unterminated_tails_do_not_fool_the_proof() {
    let classify = super::migration::additive_migration;
    let sql = |s: &str| classify(&wrap_migration_sql(s));
    // `e5` is a name, not an exponent; `1e5` is a number.
    assert!(sql("CREATE TABLE t(e5 INTEGER CHECK(e5 > 0 AND 1e5 > 0));"));
    assert!(!sql("CREATE TABLE t(a INTEGER CHECK(e5 > 0));"));
    assert!(!sql("CREATE TABLE t(a INTEGER CHECK(a > 1e));"));
    assert!(!sql("CREATE TABLE t(a INTEGER CHECK(a > .));"));
    assert!(!sql("CREATE TABLE t(a INTEGER) /* never closed"));
    assert!(!sql(
        "CREATE TABLE t(a INTEGER); CREATE TABLE u(b INTEGER) /*"
    ));
    assert!(!sql("CREATE TABLE t(a VARCHAR(10)(20));"));
    assert!(!sql("CREATE TABLE t(a (10));"));
    assert!(!sql("CREATE TABLE t(a VARCHAR(10) TEXT);"));
    assert!(sql(
        "CREATE TABLE t(a VARCHAR(10), b NUMERIC(10, 2), c UNSIGNED BIG INT);"
    ));
    assert!(!sql(
        "CREATE TABLE t(a TEXT, b TEXT REFERENCES t(a) ON DELETE \"cascade\");"
    ));
    assert!(sql(
        "CREATE TABLE t(a TEXT, b TEXT REFERENCES t(a) ON DELETE CASCADE);"
    ));
}

// ---------------- the expression proof is an allowlist, not a grammar (#462)

/// Every shipped migration the Rust selector calls additive, recorded when the
/// expression allowlist replaced the grammar. A migration leaving or entering this
/// list is a visible diff here, so the allowlist cannot silently narrow below what the
/// real store needs (or widen past it).
const RECORDED_ADDITIVE_MIGRATIONS: &[&str] = &[
    "v003", "v004", "v006", "v008", "v011", "v012", "v013", "v014", "v017", "v019", "v020", "v022",
    "v023", "v024", "v025", "v026", "v027", "v031", "v036", "v038", "v039", "v040", "v041", "v042",
    "v044", "v045", "v047", "v050", "v067", "v070", "v096", "v139", "v158", "v160", "v166", "v183",
    "v189",
];

/// The subset the new-table proof (`isolated_table_migration`) accepts by itself.
const RECORDED_ISOLATED_MIGRATIONS: &[&str] = &["v166", "v183", "v189"];

/// The newest migration the two lists above were recorded against; later files are
/// new and are not judged here.
const RECORDED_THROUGH_MIGRATION: u32 = 190;

/// How many `vNNN.rs` files exist up to that number (the numbering has a gap).
const RECORDED_MIGRATION_FILES: usize = 190;

#[test]
fn every_shipped_migration_keeps_its_recorded_classification() {
    let dir = std::path::Path::new(rsi_common::manifest_dir!())
        .join("../rsid-store/src/store/migrations");
    let mut judged = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        let Some(stem) = name.strip_suffix(".rs") else {
            continue;
        };
        let Some(number) = stem.strip_prefix('v').and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        if number > RECORDED_THROUGH_MIGRATION {
            continue;
        }
        let text = std::fs::read_to_string(dir.join(&name)).unwrap();
        let additive = super::migration::additive_migration(&text);
        let isolated = super::migration::code_without_comments(&text)
            .is_some_and(|code| super::migration::isolated_table_migration(&code));
        assert_eq!(
            additive,
            RECORDED_ADDITIVE_MIGRATIONS.contains(&stem),
            "{stem}: additive classification changed"
        );
        assert_eq!(
            isolated,
            RECORDED_ISOLATED_MIGRATIONS.contains(&stem),
            "{stem}: isolated-table classification changed"
        );
        judged += 1;
    }
    assert_eq!(
        judged, RECORDED_MIGRATION_FILES,
        "every recorded migration file must exist"
    );
}

/// Constructs outside the allowlist: not proven, so every rewind caller stays.
#[test]
fn constructs_outside_the_expression_allowlist_are_not_proven() {
    let classify = super::migration::additive_migration;
    let check = |e: &str| {
        wrap_migration_sql(&format!(
            "CREATE TABLE t(id TEXT, n INTEGER, s TEXT, CHECK({e}));"
        ))
    };
    let many = |term: &str, sep: &str, count: usize| vec![term; count].join(sep);
    let outside: Vec<String> = [
        // the reviewer's rejected inputs
        "id GLOB 'a*' ESCAPE '!'",
        "id LIKE 'a%' ESCAPE '!'",
        "id +",
        "(id",
        "((id)",
        "id IN ()",
        "CASE WHEN n > 0 THEN 1",
        // other constructs outside the allowlist
        "CASE WHEN n > 0 THEN 1 ELSE 0 END = 1",
        "CAST(n AS TEXT) = '1'",
        "s COLLATE NOCASE = 'a'",
        "s REGEXP 'a'",
        "s MATCH 'a'",
        "s LIKE n",
        "s GLOB lower(s)",
        "n IN (SELECT 1)",
        "EXISTS (SELECT 1)",
        "(SELECT 1) = 1",
        "n = ?",
        "n = ?1",
        "n = :p",
        "n = @p",
        "n = $p",
        "n IN (1, n)",
        "n IN (lower(s))",
        "s || 'x' = 'ax'",
        "n & 1 = 1",
        "n | 1 = 1",
        "n << 1 > 0",
        "~n < 0",
        "n ISNULL",
        "n NOTNULL",
        "n NOT NULL",
        "0x1F < n",
        "n = 0x",
        "n = 12345678901234567890",
        "RAISE(ABORT, 'x') = 1",
        "coalesce(s, id) = 'x'",
        "abs(n) = 1",
        "upper(s) = 'x'",
        "hex(s) = ''",
        "random() = 1",
        "strftime('%Y', 'now') = '2000'",
        "count(*) = 1",
        "sum(n) = 1",
        "lower(s) OVER () = 'x'",
        "lower(s) FILTER (WHERE n) = 'x'",
        "json_extract(s, '$') = 1",
        "rsi_sha256_digest_is_canonical(s)",
        "length(s, s)",
        "length()",
        "substr(s)",
        "substr(s, 1, 2, 3)",
        "instr(s)",
        "unknown_function(s)",
        "main.t.id = 1",
        "t.id = 1",
        "n BETWEEN 1 AND",
        "NOT NOT NOT",
        "CURRENT_TIMESTAMP IS NOT NULL",
    ]
    .into_iter()
    .map(str::to_owned)
    .chain([
        // oversized: a flat left-associative chain stays shallow but SQLite caps the tree
        many("n", " + ", 1001),
        many("n", " + ", 200),
        many("n > 0", " AND ", 100),
        // too many function arguments
        format!("length({})", many("n", ", ", 1001)),
        format!("substr({})", many("n", ", ", 9)),
        // too deep a parenthesis nest
        format!("{}n{} > 0", "(".repeat(40), ")".repeat(40)),
        format!("{}n > 0", "NOT ".repeat(40)),
        format!("{}n > 0", "- ".repeat(40)),
        // too long an IN list
        format!("n IN ({})", many("1", ", ", 33)),
    ])
    .collect();
    for e in &outside {
        assert!(
            !classify(&check(e)),
            "must not be proven: {}",
            e.chars().take(80).collect::<String>()
        );
    }
    // the same constructs in a column CHECK, a DEFAULT and a trigger WHEN
    let table = "CREATE TABLE t(id TEXT PRIMARY KEY, n INTEGER, s TEXT);";
    for (e, when) in [
        ("id GLOB 'a*' ESCAPE '!'", "NEW.id GLOB 'a*' ESCAPE '!'"),
        ("CAST(n AS TEXT) = '1'", "CAST(NEW.n AS TEXT) = '1'"),
    ] {
        assert!(!classify(&wrap_migration_sql(&format!(
            "CREATE TABLE t(id TEXT, n INTEGER CHECK({e}));"
        ))));
        assert!(!classify(&wrap_migration_sql(&format!(
            "CREATE TABLE t(id TEXT DEFAULT ({e}));"
        ))));
        assert!(!classify(&wrap_migration_sql(&format!(
            "{table} CREATE TRIGGER g BEFORE UPDATE ON t WHEN {when} BEGIN SELECT RAISE(ABORT,'x'); END;"
        ))));
    }
    // the largest shapes inside every bound are still proven
    for e in [
        many("n", " + ", 100),
        format!("n IN ({})", many("1", ", ", 32)),
        format!("{}n{} > 0", "(".repeat(10), ")".repeat(10)),
        "substr(s, 1, 2) = 'ab'".to_owned(),
    ] {
        assert!(classify(&check(&e)), "must stay proven: {e}");
    }
}

/// The reviewer's three same-class witnesses, end to end: with an additive rewind
/// helper change the proof must not narrow, so ordinary rewind callers stay selected.
#[test]
fn sql_sqlite_rejects_never_narrows_the_rewind_callers() {
    let flat_chain = vec!["id"; 1001].join(" + ");
    let many_args = vec!["id"; 1001].join(", ");
    for check in [
        "id GLOB 'a*' ESCAPE '!'".to_owned(),
        flat_chain,
        format!("coalesce({many_args})"),
    ] {
        let mut case = Case::new();
        let r = additive_rewind_change(
            &mut case,
            &format!(
                "impl Store {{\n    fn migrate_v151(&self) {{\n        tx.execute_batch(\"CREATE TABLE fresh(id INTEGER CHECK({check}));\");\n        tx.pragma_update(None, \"user_version\", 151);\n    }}\n}}\n"
            ),
        );
        assert_eq!(r.code, 0, "{}", r.summary);
        let filters = filter_set(&r);
        let shown: String = check.chars().take(60).collect();
        assert!(
            filters.contains("rsid=shard:store-01:test(migration_old)"),
            "{shown}: {filters:?}"
        );
        assert!(
            filters.contains("rsid=shard:store-02:test(external_migration)"),
            "{shown}: {filters:?}"
        );
    }
}

#[test]
fn sql_with_a_nul_byte_or_an_oversized_shape_is_not_proven() {
    let classify = super::migration::additive_migration;
    let sql = |s: &str| classify(&wrap_migration_sql(s));
    assert!(sql("CREATE TABLE t(a TEXT DEFAULT 'x');"));
    assert!(!sql("CREATE TABLE t(a TEXT DEFAULT 'x\\u0000y');"));
    let columns = |n: usize| {
        format!(
            "CREATE TABLE t({});",
            (0..n)
                .map(|i| format!("c{i} TEXT"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    assert!(sql(&columns(64)));
    assert!(!sql(&columns(65)));
    assert!(!sql(&columns(2001)));
    let statements = |n: usize| {
        (0..n)
            .map(|i| format!("CREATE TABLE t{i}(a TEXT);"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    assert!(sql(&statements(64)));
    assert!(!sql(&statements(65)));
    // DEFAULT literals are strict too: hexadecimal and over-long integers are out
    assert!(sql("CREATE TABLE t(a INTEGER DEFAULT -12);"));
    assert!(!sql("CREATE TABLE t(a INTEGER DEFAULT 0x10);"));
    assert!(!sql(
        "CREATE TABLE t(a INTEGER DEFAULT 1234567890123456789012);"
    ));
    assert!(!sql("CREATE TABLE t(a TEXT DEFAULT CURRENT_TIMESTAMP);"));
    assert!(sql(
        "CREATE TABLE t(a TEXT DEFAULT NULL, b INTEGER DEFAULT TRUE);"
    ));
}

/// #462 residual: SQLite rejects an index (or a PRIMARY KEY, UNIQUE, FOREIGN KEY) with
/// too many columns, even when one column repeats, so every repeatable list the
/// new-table proof accepts has a small bound and anything past it is not proven. Each
/// shape is checked at its bound (proven, rewind callers narrowed to the chain pins)
/// and one past it (not proven, every ordinary rewind caller stays selected), and the
/// reviewer's 2001-occurrence lists are checked end to end as well.
#[test]
fn every_repeatable_ddl_list_is_bounded_and_over_the_bound_keeps_all_rewind_callers() {
    let repeat = |item: &str, sep: &str, n: usize| vec![item; n].join(sep);
    // (label, SQL builder over a count, largest count that is still proven)
    #[allow(clippy::type_complexity)]
    let shapes: Vec<(&str, Box<dyn Fn(usize) -> String>, usize)> = vec![
        (
            "CREATE INDEX column list",
            Box::new(move |n| {
                format!(
                    "CREATE TABLE t(id INTEGER); CREATE INDEX i ON t({});",
                    repeat("id", ",", n)
                )
            }),
            16,
        ),
        (
            "table UNIQUE column list",
            Box::new(move |n| {
                format!(
                    "CREATE TABLE t(id INTEGER, UNIQUE({}));",
                    repeat("id", ",", n)
                )
            }),
            16,
        ),
        (
            "table PRIMARY KEY column list",
            Box::new(move |n| {
                format!(
                    "CREATE TABLE t(id INTEGER, PRIMARY KEY({}));",
                    repeat("id", ",", n)
                )
            }),
            16,
        ),
        (
            "FOREIGN KEY child and parent lists",
            Box::new(move |n| {
                format!(
                    "CREATE TABLE t(id INTEGER, FOREIGN KEY({}) REFERENCES p({}));",
                    repeat("id", ",", n),
                    repeat("id", ",", n)
                )
            }),
            16,
        ),
        (
            "table-level constraints",
            Box::new(move |n| {
                format!(
                    "CREATE TABLE t(id INTEGER, {});",
                    repeat("CHECK(id > 0)", ", ", n)
                )
            }),
            64,
        ),
        (
            "declared columns",
            Box::new(move |n| {
                let columns: Vec<String> = (0..n).map(|i| format!("c{i} TEXT")).collect();
                format!("CREATE TABLE t({});", columns.join(", "))
            }),
            64,
        ),
        (
            "constraint clauses on one column",
            Box::new(move |n| {
                format!("CREATE TABLE t(id INTEGER {});", repeat("NOT NULL", " ", n))
            }),
            16,
        ),
        (
            "words in one type name",
            Box::new(move |n| format!("CREATE TABLE t(id {});", repeat("word", " ", n))),
            4,
        ),
        (
            "ON/DEFERRABLE clauses of one REFERENCES",
            Box::new(move |n| {
                format!(
                    "CREATE TABLE t(id INTEGER REFERENCES p {});",
                    repeat("ON DELETE CASCADE", " ", n)
                )
            }),
            8,
        ),
        (
            "statements in one batch",
            Box::new(move |n| {
                (0..n)
                    .map(|i| format!("CREATE TABLE t{i}(a TEXT);"))
                    .collect::<Vec<_>>()
                    .join(" ")
            }),
            64,
        ),
        (
            "IN list",
            Box::new(move |n| {
                format!(
                    "CREATE TABLE t(id INTEGER CHECK(id IN ({})));",
                    repeat("1", ", ", n)
                )
            }),
            32,
        ),
        (
            "tokens in one expression",
            Box::new(move |n| {
                // n terms joined by `+`, then `> 0`: 2n + 1 tokens inside the CHECK
                format!(
                    "CREATE TABLE t(id INTEGER CHECK({} > 0));",
                    repeat("id", " + ", n)
                )
            }),
            // 127 terms are 255 tokens; 128 are 257, over the 256-token bound
            127,
        ),
        (
            "parenthesis depth",
            Box::new(move |n| {
                format!(
                    "CREATE TABLE t(id INTEGER CHECK({}id > 0{}));",
                    "(".repeat(n),
                    ")".repeat(n)
                )
            }),
            15,
        ),
    ];
    for (label, build, bound) in &shapes {
        let at_bound = wrap_migration_sql(&build(*bound));
        assert!(
            super::migration::additive_migration(&at_bound),
            "{label}: {bound} items must stay proven"
        );
        let mut case = Case::new();
        let r = additive_rewind_change(
            &mut case,
            &format!(
                "impl Store {{\n    fn migrate_v151(&self) {{\n        tx.execute_batch(\"{}\");\n        tx.pragma_update(None, \"user_version\", 151);\n    }}\n}}\n",
                build(*bound)
            ),
        );
        assert_eq!(r.code, 0, "{label}: {}", r.summary);
        assert_eq!(filter_set(&r), set_of(&CHAIN_PINS), "{label} at its bound");
        // one past the bound, and the reviewer-scale list
        for n in [*bound + 1, 2001] {
            let sql = build(n);
            assert!(
                !super::migration::additive_migration(&wrap_migration_sql(&sql)),
                "{label}: {n} items must not be proven"
            );
            let mut case = Case::new();
            let r = additive_rewind_change(
                &mut case,
                &format!(
                    "impl Store {{\n    fn migrate_v151(&self) {{\n        tx.execute_batch(\"{sql}\");\n        tx.pragma_update(None, \"user_version\", 151);\n    }}\n}}\n"
                ),
            );
            assert_eq!(r.code, 0, "{label} {n}: {}", r.summary);
            let filters = filter_set(&r);
            assert!(
                filters.contains("rsid=shard:store-01:test(migration_old)"),
                "{label} {n}: {filters:?}"
            );
            assert!(
                filters.contains("rsid=shard:store-02:test(external_migration)"),
                "{label} {n}: {filters:?}"
            );
        }
    }
}
