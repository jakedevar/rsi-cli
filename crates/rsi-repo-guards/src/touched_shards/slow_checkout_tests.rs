//! The slow tier as the REAL checkout declares it (#2270, #2298; port of the
//! `ConfigTest` class of the retired `scripts/tests/test_slow_tier.py`, #489).
//!
//! `.config/slow-tier.toml` names the tests, `.config/nextest.toml`'s default-filter leaves
//! them out of the default profiles, and a drift here silently hides tests from the default
//! tier. Each invariant is a pure check over text (or a directory) so a test can prove it
//! rejects a broken input as well as accepting the checked-in one. No program is executed
//! except `git check-ignore`.
use super::slow::{
    CONFIG, NEXTEST_CONFIG, SlowTier, parse_slow_tier, slow_tier_filter, test_fn_spans,
    write_slow_tier_filter,
};
use super::tests::Fixture;
use crate::testutil::repo_root;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

type Check = Result<(), String>;

/// Tests the tier must name: the released-migration history it exists for.
const HISTORY: [&str; 5] = [
    "store::tests::h1_rb_full_v84_graph_is_nonempty_rewindable_and_replayable",
    "store::tests::d05_v79_fresh_v77_v76_chain_failpoints_reopen_and_fingerprint",
    "store::tests::issue_v97_every_fault_rolls_back_and_repairs_fresh_copied_and_large_fixtures",
    "store::tests::v112_v113_convergence_tests::v112_identity_faults_roll_back_and_retry",
    "store::tests::v95_deployed_v94_v95_failpoints_roll_back_exactly_then_retry",
];
/// Current-behaviour tests next to them that must stay in the default tier.
const DEFAULT_TIER: [&str; 2] = [
    "store::tests::test_store_open_creates_schema",
    "store::tests::test_insert_and_load_session",
];
/// #2298: tests that open the CURRENT schema and assert current behaviour. A version number
/// or the word "replay" in their names once put them in the tier; their subject is not a
/// rewound, upgraded or replayed historical schema, so a non-migration change to what they
/// observe must run them (the tier is excluded from every other diff).
const CURRENT_BEHAVIOUR: [&str; 14] = [
    "store::tests::closure_operator_same_key_replays_and_different_payload_refuses_without_effect",
    "store::tests::d02_idea_event_cas_tests::d02_idea_event_cas_two_file_connections_have_one_cas_winner",
    "store::tests::v96_tool_use_id_and_metadata_roundtrip",
    "store::tests::h1_v85_fresh_catalog_is_private_and_idempotent",
    "store::tests::h1_v85_request_key_grammar_and_claim_family_coupling_are_exhaustive",
    "store::tests::h1_v87_session_fence_finalize_requires_complete_terminal_bundle",
    "store::tests::h1_v87_session_fence_initial_bind_requires_exact_active_authority",
    "store::tests::h1_v87_session_fence_ordered_transactions_commit_or_rollback_atomically",
    "store::tests::h1_v87_session_fence_provider_live_writes_are_exact_next_sequence",
    "store::tests::h1_v88_static_state_machine_production_affinity_predicate_owners_are_exact",
    "store::tests::tag_default_empty_after_v43_upgrade",
    "store::tests::test_schema_migration_idempotent",
    "store::tests::test_v48_daemon_settings_table_created",
    "store::tests::v95_reopen_rejects_latest_run_pointer_catalog_drift",
];
const REQUIRED_TRIGGERS: [&str; 2] = [
    "crates/rsid-store/src/store/migrations/",
    "tools/released-migrations.json",
];

fn read(root: &Path, relative: &str) -> String {
    std::fs::read_to_string(root.join(relative)).unwrap_or_else(|e| panic!("{relative}: {e}"))
}
fn config_of(root: &Path) -> SlowTier {
    parse_slow_tier(&read(root, CONFIG)).unwrap()
}
fn nextest_of(text: &str) -> toml::Value {
    toml::from_str(text).unwrap_or_else(|e| panic!("{NEXTEST_CONFIG}: {e}"))
}
fn profile<'a>(nextest: &'a toml::Value, name: &str) -> Result<&'a toml::Value, String> {
    nextest
        .get("profile")
        .and_then(|p| p.get(name))
        .ok_or_else(|| format!("{NEXTEST_CONFIG} has no [profile.{name}]"))
}
fn rust_sources(dir: &Path, out: &mut Vec<(PathBuf, String)>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|x| x == "rs") {
            let text = std::fs::read_to_string(&path).unwrap();
            out.push((path, text));
        }
    }
}
fn package_sources(root: &Path, config: &SlowTier) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    rust_sources(
        &root.join("crates").join(&config.package).join("src"),
        &mut out,
    );
    out
}
/// Every name a source declares as a `#[test]` fn; an unusual layout falls back to any `fn`.
fn declared_fns(text: &str) -> Vec<String> {
    if let Some(spans) = test_fn_spans(text) {
        return spans.into_iter().map(|s| s.name).collect();
    }
    text.lines()
        .filter_map(|line| line.trim_start().strip_prefix("fn "))
        .map(|rest| {
            rest.trim_start()
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect::<String>()
        })
        .filter(|name| !name.is_empty())
        .collect()
}

/// Case 1: the checked-in default-filter is exactly the one generated from the config.
fn check_default_filter(config: &SlowTier, nextest: &toml::Value) -> Check {
    let actual = profile(nextest, "default")?
        .get("default-filter")
        .and_then(toml::Value::as_str)
        .ok_or("[profile.default] has no default-filter string")?;
    let expected = slow_tier_filter(config);
    if actual == expected {
        return Ok(());
    }
    Err(format!(
        "the default-filter drifted from {CONFIG}: run `check-touched-shards --write-slow-tier-filter`"
    ))
}
/// Case 2: full profiles run everything; gate profiles inherit the tier.
fn check_profiles(nextest: &toml::Value) -> Check {
    for name in ["ci-full", "rsid-qa"] {
        let filter = profile(nextest, name)?
            .get("default-filter")
            .and_then(toml::Value::as_str);
        if filter != Some("all()") {
            return Err(format!("{name} must set default-filter = \"all()\""));
        }
    }
    for name in ["rsid-fast", "test-timings"] {
        let p = profile(nextest, name)?;
        if p.get("default-filter").is_some() {
            return Err(format!(
                "{name} must inherit the tier, not set a default-filter"
            ));
        }
        if p.get("inherits").and_then(toml::Value::as_str) != Some("default") {
            return Err(format!("{name} must inherit = \"default\""));
        }
    }
    Ok(())
}
/// Case 3: the generator rewrites the checked-in files to themselves.
fn check_generator_idempotent(slow_text: &str, nextest_text: &str) -> Check {
    let temp = tempfile::tempdir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(temp.path().join(".config")).map_err(|e| e.to_string())?;
    std::fs::write(temp.path().join(CONFIG), slow_text).map_err(|e| e.to_string())?;
    std::fs::write(temp.path().join(NEXTEST_CONFIG), nextest_text).map_err(|e| e.to_string())?;
    write_slow_tier_filter(temp.path())?;
    let rewritten = std::fs::read_to_string(temp.path().join(NEXTEST_CONFIG)).unwrap();
    if rewritten == nextest_text {
        Ok(())
    } else {
        Err(format!(
            "the generator changes the checked-in {NEXTEST_CONFIG}"
        ))
    }
}
/// Case 4: every named test is a `#[test]` fn declared exactly once in the package sources.
fn check_named_tests_declared_once(config: &SlowTier, sources: &[(PathBuf, String)]) -> Check {
    let mut declared: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (path, text) in sources {
        for name in declared_fns(text) {
            declared
                .entry(name)
                .or_default()
                .push(path.file_name().unwrap().to_string_lossy().into_owned());
        }
    }
    for name in &config.tests {
        let leaf = name.rsplit("::").next().unwrap();
        let found = declared.get(leaf).map_or(0, Vec::len);
        if found != 1 {
            return Err(format!(
                "{name}: expected one #[test] fn {leaf} in the store sources, found {:?}",
                declared.get(leaf)
            ));
        }
    }
    Ok(())
}
/// Case 5: the tier names the migration history it exists for and not the default tests.
fn check_names_history(config: &SlowTier) -> Check {
    for name in HISTORY {
        if !config.tests.iter().any(|t| t == name) {
            return Err(format!("the tier must name {name}"));
        }
    }
    for name in DEFAULT_TIER {
        if config.tests.iter().any(|t| t == name) {
            return Err(format!("{name} belongs to the default tier"));
        }
    }
    Ok(())
}
/// Case 6: current-behaviour tests stay in the default tier, and the list is not stale.
fn check_current_behaviour(config: &SlowTier, sources: &[(PathBuf, String)]) -> Check {
    for name in CURRENT_BEHAVIOUR {
        if config.tests.iter().any(|t| t == name) {
            return Err(format!(
                "{name}: current-behaviour tests belong to the default tier (#2298)"
            ));
        }
        let leaf = name.rsplit("::").next().unwrap();
        let needle = format!("fn {leaf}");
        let defined = sources.iter().any(|(_, text)| {
            text.match_indices(&needle).any(|(at, _)| {
                !text[at + needle.len()..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_')
                    && !text[..at]
                        .chars()
                        .next_back()
                        .is_some_and(|c| c.is_alphanumeric() || c == '_')
            })
        });
        if !defined {
            return Err(format!("{name}: no longer defined in the store sources"));
        }
    }
    Ok(())
}
/// Case 7: every trigger path exists, and the migration triggers are present.
fn check_trigger_paths(config: &SlowTier, root: &Path) -> Check {
    for trigger in &config.trigger_paths {
        let path = root.join(trigger);
        let exists = if trigger.ends_with('/') {
            path.is_dir()
        } else {
            path.is_file()
        };
        if !exists {
            return Err(format!("trigger path {trigger} does not exist"));
        }
    }
    for required in REQUIRED_TRIGGERS {
        if !config.trigger_paths.iter().any(|t| t == required) {
            return Err(format!("trigger_paths must include {required}"));
        }
    }
    Ok(())
}
/// Case 8: the config is not ignored by `.gitignore` (`check-ignore`: 0 ignored, 1 not).
fn check_not_gitignored(root: &Path) -> Check {
    let status = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["check-ignore", "-q", CONFIG])
        .status()
        .map_err(|e| e.to_string())?;
    match status.code() {
        Some(1) => Ok(()),
        Some(0) => Err(format!("{CONFIG} is ignored by .gitignore")),
        other => Err(format!("git check-ignore failed: {other:?}")),
    }
}

// ---- the real checkout (cases 1-8) ----------------------------------------------------

#[test]
fn real_default_filter_is_generated_from_the_config() {
    let root = repo_root();
    let nextest = nextest_of(&read(&root, NEXTEST_CONFIG));
    check_default_filter(&config_of(&root), &nextest).unwrap();
}
#[test]
fn real_full_profiles_run_every_test_and_gate_profiles_inherit_the_tier() {
    let root = repo_root();
    check_profiles(&nextest_of(&read(&root, NEXTEST_CONFIG))).unwrap();
}
#[test]
fn real_write_command_is_idempotent_on_the_checked_in_files() {
    let root = repo_root();
    check_generator_idempotent(&read(&root, CONFIG), &read(&root, NEXTEST_CONFIG)).unwrap();
}
#[test]
fn real_every_named_test_is_declared_exactly_once_in_the_store_sources() {
    let root = repo_root();
    let config = config_of(&root);
    assert!(!config.tests.is_empty());
    check_named_tests_declared_once(&config, &package_sources(&root, &config)).unwrap();
}
#[test]
fn real_tier_names_the_migration_history_it_exists_for() {
    check_names_history(&config_of(&repo_root())).unwrap();
}
#[test]
fn real_current_behaviour_tests_run_in_the_default_tier() {
    let root = repo_root();
    let config = config_of(&root);
    check_current_behaviour(&config, &package_sources(&root, &config)).unwrap();
}
#[test]
fn real_every_trigger_path_exists() {
    let root = repo_root();
    check_trigger_paths(&config_of(&root), &root).unwrap();
}
#[test]
fn real_slow_tier_config_is_not_gitignored() {
    check_not_gitignored(&repo_root()).unwrap();
}

// ---- negative proofs: each check rejects a broken input -------------------------------

fn tier(tests: &[&str], triggers: &[&str]) -> SlowTier {
    SlowTier {
        package: "rsid-store".into(),
        trigger_paths: triggers.iter().map(|s| (*s).into()).collect(),
        tests: tests.iter().map(|s| (*s).into()).collect(),
    }
}
fn nextest_with(default_filter: &str, full: &str, fast: &str) -> String {
    format!(
        "[profile.default]\ndefault-filter = '{default_filter}'\n\
         [profile.rsid-fast]\ninherits = \"default\"\n{fast}\n\
         [profile.rsid-qa]\ninherits = \"rsid-fast\"\n{full}\n\
         [profile.ci-full]\ninherits = \"default\"\n{full}\n\
         [profile.test-timings]\ninherits = \"default\"\n"
    )
}

#[test]
fn a_filter_that_drifted_from_the_config_is_refused() {
    let config = tier(&["store::tests::a"], &[]);
    let good = nextest_with(&slow_tier_filter(&config), "default-filter = \"all()\"", "");
    check_default_filter(&config, &nextest_of(&good)).unwrap();
    let drifted = tier(&["store::tests::a", "store::tests::b"], &[]);
    let error = check_default_filter(&drifted, &nextest_of(&good)).unwrap_err();
    assert!(error.contains("drifted"), "{error}");
}
#[test]
fn profiles_that_lose_the_full_run_or_the_inherited_tier_are_refused() {
    let filter = slow_tier_filter(&tier(&["store::tests::a"], &[]));
    let all = "default-filter = \"all()\"";
    check_profiles(&nextest_of(&nextest_with(&filter, all, ""))).unwrap();
    let narrowed = nextest_with(&filter, "default-filter = \"test(x)\"", "");
    let error = check_profiles(&nextest_of(&narrowed)).unwrap_err();
    assert!(error.contains("all()"), "{error}");
    let own_filter = nextest_with(&filter, all, "default-filter = \"all()\"");
    let error = check_profiles(&nextest_of(&own_filter)).unwrap_err();
    assert!(error.contains("must inherit the tier"), "{error}");
    let missing = "[profile.default]\ndefault-filter = 'x'\n";
    assert!(
        check_profiles(&nextest_of(missing))
            .unwrap_err()
            .contains("no [profile.")
    );
}
#[test]
fn a_generator_that_would_rewrite_the_nextest_file_is_refused() {
    let slow = "package = \"rsid-store\"\ntrigger_paths = []\ntests = [\"store::tests::a\"]\n";
    let filter = slow_tier_filter(&parse_slow_tier(slow).unwrap());
    let good = nextest_with(&filter, "default-filter = \"all()\"", "");
    check_generator_idempotent(slow, &good).unwrap();
    let stale = nextest_with(
        "not (package(rsid-store) & test(/^(?:store::tests::old)$/))",
        "default-filter = \"all()\"",
        "",
    );
    let error = check_generator_idempotent(slow, &stale).unwrap_err();
    assert!(error.contains("changes the checked-in"), "{error}");
}
#[test]
fn a_named_test_declared_zero_or_twice_is_refused() {
    let config = tier(&["store::tests::slow_one"], &[]);
    let one = [(
        PathBuf::from("a.rs"),
        "#[test]\nfn slow_one() {\n}\n".to_owned(),
    )];
    check_named_tests_declared_once(&config, &one).unwrap();
    let error = check_named_tests_declared_once(&config, &[]).unwrap_err();
    assert!(error.contains("found None"), "{error}");
    let twice = [
        one[0].clone(),
        (
            PathBuf::from("b.rs"),
            "#[test]\nfn slow_one() {\n}\n".to_owned(),
        ),
    ];
    let error = check_named_tests_declared_once(&config, &twice).unwrap_err();
    assert!(error.contains("a.rs") && error.contains("b.rs"), "{error}");
    // A plain (non-test) fn of the same name does not count as the test.
    let helper = [(PathBuf::from("c.rs"), "fn slow_one() {\n}\n".to_owned())];
    assert!(check_named_tests_declared_once(&config, &helper).is_err());
}
#[test]
fn a_tier_that_forgets_the_history_or_takes_a_default_test_is_refused() {
    let mut names: Vec<&str> = HISTORY.to_vec();
    check_names_history(&tier(&names, &[])).unwrap();
    names.pop();
    let error = check_names_history(&tier(&names, &[])).unwrap_err();
    assert!(error.contains("must name"), "{error}");
    let mut with_default: Vec<&str> = HISTORY.to_vec();
    with_default.push(DEFAULT_TIER[0]);
    let error = check_names_history(&tier(&with_default, &[])).unwrap_err();
    assert!(error.contains("default tier"), "{error}");
}
#[test]
fn a_current_behaviour_test_in_the_tier_or_gone_from_the_source_is_refused() {
    let source: String = CURRENT_BEHAVIOUR
        .iter()
        .map(|n| format!("fn {}() {{}}\n", n.rsplit("::").next().unwrap()))
        .collect();
    let sources = [(PathBuf::from("s.rs"), source.clone())];
    check_current_behaviour(&tier(&HISTORY, &[]), &sources).unwrap();
    let mut leaked = HISTORY.to_vec();
    leaked.push(CURRENT_BEHAVIOUR[3]);
    let error = check_current_behaviour(&tier(&leaked, &[]), &sources).unwrap_err();
    assert!(error.contains("#2298"), "{error}");
    let stale = [(
        PathBuf::from("s.rs"),
        source.replace("fn tag_default_empty", "fn renamed"),
    )];
    let error = check_current_behaviour(&tier(&HISTORY, &[]), &stale).unwrap_err();
    assert!(error.contains("no longer defined"), "{error}");
}
#[test]
fn a_missing_trigger_path_or_required_trigger_is_refused() {
    let fixture = Fixture::new();
    fixture.write("crates/rsid-store/src/store/migrations/v1.rs", "");
    fixture.write("tools/released-migrations.json", "{}");
    let good = tier(&[], &REQUIRED_TRIGGERS);
    check_trigger_paths(&good, fixture.path()).unwrap();
    let missing = tier(
        &[],
        &[
            "crates/rsid-store/src/store/migrations/",
            "tools/released-migrations.json",
            "nope.rs",
        ],
    );
    let error = check_trigger_paths(&missing, fixture.path()).unwrap_err();
    assert!(error.contains("nope.rs"), "{error}");
    // A directory trigger that is a file, and a file trigger that is a directory.
    fixture.write("tools/dir/file", "");
    let wrong_kind = tier(&[], &["tools/dir"]);
    assert!(check_trigger_paths(&wrong_kind, fixture.path()).is_err());
    let unrequired = tier(&[], &["tools/released-migrations.json"]);
    let error = check_trigger_paths(&unrequired, fixture.path()).unwrap_err();
    assert!(error.contains("must include"), "{error}");
}
#[test]
fn a_gitignored_slow_tier_config_is_refused() {
    let fixture = Fixture::new();
    fixture.write(CONFIG, "package = \"rsid-store\"\n");
    check_not_gitignored(fixture.path()).unwrap();
    fixture.write(".gitignore", ".config/\n");
    let error = check_not_gitignored(fixture.path()).unwrap_err();
    assert!(error.contains("ignored by .gitignore"), "{error}");
}
