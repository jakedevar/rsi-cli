//! Portable bundle tests (#1406). Every secret is a fake `sk-test-...` value;
//! the absence assertions on them are leaked-secret checks, not hidden-identity
//! checks.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::test_support::test_session;
use crate::vault::{Slot, VaultHandleBuilder, VaultSettings, slots};
use rsi_common::types::{ConversationEvent, EventType, IssueStatus, NewIssue, Project, Role};
use std::sync::Arc;
use uuid::Uuid;

const OLD_ROOT: &str = "/home/olduser/code";

fn open(dir: &Path, name: &str) -> Store {
    Store::open(&dir.join(name)).expect("Store::open runs the migrations")
}

fn new_issue(project_id: Uuid, title: &str, body: &str) -> NewIssue {
    NewIssue {
        project_id,
        title: title.to_string(),
        body: body.to_string(),
        priority: Some(2),
        labels: vec!["portability".to_string()],
        created_by_session_id: None,
        assignee: None,
        idea_id: None,
        source_event_id: None,
        source_finding_ref: None,
    }
}

struct Fixture {
    project: Uuid,
    session: Uuid,
}

/// A source install with durable state plus history: two projects, settings,
/// rules, policies, Issues with events and a dependency, a manager policy held
/// by a live session, and sessions with conversation events.
fn seed(store: &Store) -> Fixture {
    let now = chrono::Utc::now();
    let project = Uuid::new_v4();
    for (id, name, path) in [
        (project, "rsi", format!("{OLD_ROOT}/rsi")),
        (Uuid::new_v4(), "notes", "/srv/notes".to_string()),
    ] {
        store
            .insert_project(&Project {
                id,
                name: name.to_string(),
                path: Some(PathBuf::from(path)),
                description: Some(format!("{name} project")),
                color: "#89b4fa".to_string(),
                context_files: Some(vec![PathBuf::from("AGENTS.md")]),
                created_at: now,
                updated_at: now,
            })
            .unwrap();
    }

    let session = Uuid::new_v4();
    store
        .insert_session(&test_session(
            session,
            PathBuf::from(format!("{OLD_ROOT}/rsi")),
        ))
        .unwrap();
    store
        .insert_session(&test_session(Uuid::new_v4(), PathBuf::from("/tmp")))
        .unwrap();
    for sequence in 1..=3 {
        store
            .insert_event(&ConversationEvent {
                id: 0,
                session_id: session,
                sequence,
                event_type: EventType::Message,
                role: Some(Role::Assistant),
                content: format!("history message {sequence}"),
                tool_name: None,
                tool_input: None,
                created_at: now,
                offload_id: None,
                tool_use_id: None,
                metadata: None,
            })
            .unwrap();
    }

    for (key, value) in [
        ("context_rotation_claude_pct", "45"),
        ("api_route.bedrock", "harness"),
        ("provider_profile", "aws_only"),
        ("internal_watchdog_probe_nonce", "nonce"),
        (&format!("manager_operator_pause:{session}") as &str, "soft"),
    ] {
        store.set_daemon_setting(key, value).unwrap();
    }

    store
        .conn
        .execute_batch(&format!(
            // sql-dynamic-ok: test fixture; only generated UUIDs are interpolated.
            "INSERT INTO permission_rules (tool_pattern, action, scope, scope_id, priority, created_at, updated_at)
               VALUES ('Bash(git push*)', 'Ask', 'global', NULL, 5, '2026-10-01T00:00:00.000000000Z', '2026-10-01T00:00:00.000000000Z'),
                      ('Edit', 'Allow', 'project', '{project}', 1, '2026-10-01T00:00:00.000000000Z', '2026-10-01T00:00:00.000000000Z');
             INSERT INTO model_budget_policies (policy_key, scope_kind, scope_id, max_calls, updated_at)
               VALUES ('global-cap', 'global', 'global', 1000, '2026-10-01T00:00:00.000000000Z'),
                      ('session-cap', 'session', '{session}', 10, '2026-10-01T00:00:00.000000000Z');
             INSERT INTO harness_manager_v2_policies (project_id, manager_session_id, scope_version, row_version, policy_json, updated_at)
               VALUES ('{project}', '{session}', 1, 1,
                       '{{\"mode\":\"execute\",\"paused_epic_ids\":[\"{session}\"],\"group_ids\":[\"{session}\"],\"max_active_sessions\":50,\"note\":\"{session}\"}}',
                       '2026-10-01T00:00:00.000000000Z');"
        ))
        .unwrap();

    let first = store
        .create_issue(&new_issue(
            project,
            "Portable RSI",
            "carry the durable state",
        ))
        .unwrap();
    let second = store
        .create_issue(&new_issue(project, "AWS-only profile", "companion Issue"))
        .unwrap();
    store
        .update_issue_status(first.id, IssueStatus::InProgress)
        .unwrap();
    store.add_issue_dep(first.id, second.id).unwrap();
    Fixture { project, session }
}

fn rows(store: &Store, sql: &str) -> Vec<Vec<Value>> {
    let mut statement = store.conn.prepare(sql).unwrap();
    let width = statement.column_count();
    statement
        .query_map([], |row| {
            (0..width)
                .map(|index| row.get_ref(index).map(sql_to_json))
                .collect::<std::result::Result<Vec<_>, _>>()
        })
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap()
}

fn count(store: &Store, table: &str) -> i64 {
    store
        .conn
        .query_row(&format!("SELECT COUNT(*) FROM \"{table}\""), [], |row| {
            // sql-dynamic-ok: test helper over static table names.
            row.get(0)
        })
        .unwrap()
}

fn round_trip(
    dir: &Path,
    source: &Store,
    options: &PortableImportOptions,
) -> (Store, PortableImportReport) {
    let bundle = source.export_portable_bundle(&[]).unwrap();
    let path = dir.join("install.rsi-bundle.json");
    write_bundle(&path, &bundle, true).unwrap();
    let target = open(dir, "target.db");
    let report = target
        .import_portable_bundle(&read_bundle(&path).unwrap(), options)
        .unwrap();
    (target, report)
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn portable_round_trip_carries_durable_state_and_leaves_history_empty() {
    let dir = tempfile::tempdir().unwrap();
    let source = open(dir.path(), "source.db");
    let fixture = seed(&source);
    let (target, report) = round_trip(dir.path(), &source, &PortableImportOptions::default());

    for sql in [
        "SELECT * FROM projects ORDER BY id",
        "SELECT * FROM issues ORDER BY id",
        "SELECT * FROM issue_events ORDER BY id",
        "SELECT * FROM issue_deps ORDER BY issue_id",
        "SELECT * FROM model_budget_policies WHERE scope_kind='global' ORDER BY policy_key",
        "SELECT tool_pattern, action, scope, scope_id, priority FROM permission_rules ORDER BY tool_pattern",
    ] {
        assert_eq!(rows(&target, sql), rows(&source, sql), "{sql}");
    }
    assert_eq!(count(&target, "issues"), 2);
    assert_eq!(count(&target, "issue_events"), 3);
    assert_eq!(count(&target, "issue_deps"), 1);
    for (key, value) in [
        ("context_rotation_claude_pct", "45"),
        ("api_route.bedrock", "harness"),
        ("provider_profile", "aws_only"),
    ] {
        assert_eq!(
            target.get_daemon_setting(key).unwrap().as_deref(),
            Some(value),
            "{key}"
        );
    }
    // Runtime state of the old install does not travel.
    assert_eq!(
        target
            .get_daemon_setting("internal_watchdog_probe_nonce")
            .unwrap(),
        None
    );
    assert_eq!(
        target
            .get_daemon_setting(&format!("manager_operator_pause:{}", fixture.session))
            .unwrap(),
        None
    );
    assert_eq!(
        target
            .conn
            .query_row(
                "SELECT COUNT(*) FROM model_budget_policies WHERE scope_kind='session'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    // Every history table is empty on the new machine.
    for table in LEFT_BEHIND_TABLES {
        assert_eq!(count(&target, table), 0, "{table}");
    }
    for table in [
        "harness_manager_v2_policies",
        "manager_nodes",
        "global_manager_grants",
    ] {
        assert_eq!(count(&target, table), 0, "{table}");
    }
    assert_eq!(report.inserted.get("issues"), Some(&2));
    assert_eq!(report.manager_template_rows, 1);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);

    // The manager policy travels as a session-free template.
    let bundle = source.export_portable_bundle(&[]).unwrap();
    assert_eq!(bundle.left_behind.get("sessions"), Some(&2));
    assert_eq!(bundle.left_behind.get("conversation_events"), Some(&3));
    let template = &bundle.manager_templates["harness_manager_v2_policies"][0];
    assert_eq!(
        template["project_id"],
        Value::String(fixture.project.to_string())
    );
    assert_eq!(
        template["policy_json"]["max_active_sessions"],
        Value::from(50)
    );
    assert_eq!(
        template["policy_json"]["paused_epic_ids"],
        serde_json::json!([])
    );
    let text = serde_json::to_string(&bundle.manager_templates).unwrap();
    assert!(
        !text.contains(&fixture.session.to_string()),
        "a template carries a live session id"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn portable_bundle_carries_no_vault_material_or_secret_env_values() {
    let dir = tempfile::tempdir().unwrap();
    let source = open(dir.path(), "source.db");
    seed(&source);

    // Every known secret env name holds a distinct fake credential.
    let env: std::collections::HashMap<String, String> = slots::scrubbed_env_var_names()
        .chain(slots::AWS_SECRET_ENV_VARS.iter().copied())
        .enumerate()
        .map(|(index, name)| (name.to_string(), format!("sk-test-env-{index:04}-{name}")))
        .collect();
    let env_names: Vec<String> = env.keys().cloned().collect();
    let env_reader = env.clone();
    let vault = VaultHandleBuilder::new(Arc::new(VaultSettings::default()))
        .dir(dir.path().join("vault"))
        .env(move |name| env_reader.get(name).cloned())
        .open()
        .unwrap();
    vault
        .set(Slot::Anthropic, "sk-test-vault-anthropic-0001")
        .unwrap();
    vault
        .set(Slot::Bedrock, "sk-test-vault-bedrock-0002")
        .unwrap();
    vault
        .set_mcp("github", "sk-test-vault-mcp-github-0003")
        .unwrap();
    // Secret-named settings never travel, whatever their value.
    source
        .set_daemon_setting("openrouter_api_key", "sk-test-setting-0004")
        .unwrap();
    source
        .set_daemon_setting("linear.credential", "sk-test-setting-0005")
        .unwrap();

    let secrets = vault.known_secret_values();
    assert_eq!(secrets.len(), 3 + env.len());
    let bundle = source.export_portable_bundle(&secrets).unwrap();
    let path = dir.path().join("bundle.json");
    write_bundle(&path, &bundle, false).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();

    for secret in secrets.iter().map(SecretString::expose) {
        assert!(!text.contains(secret), "bundle leaks a vault/env secret");
    }
    for value in ["sk-test-setting-0004", "sk-test-setting-0005", "sk-test-"] {
        assert!(!text.contains(value), "bundle leaks a secret-named setting");
    }
    for name in &env_names {
        assert!(
            !text.contains(name.as_str()),
            "bundle names secret env var {name}"
        );
    }
    // A secret-named key is counted by an opaque label, not named.
    assert_eq!(
        bundle
            .withheld_settings
            .iter()
            .filter(|label| label.starts_with("redacted:"))
            .count(),
        2
    );
    assert!(!text.contains("openrouter_api_key"));

    // A credential pasted into carried text refuses the whole export.
    let project = source.load_projects().unwrap()[0].id;
    source
        .create_issue(&new_issue(
            project,
            "leak",
            "key sk-test-vault-anthropic-0001",
        ))
        .unwrap();
    let error = source
        .export_portable_bundle(&secrets)
        .unwrap_err()
        .to_string();
    assert!(error.contains("portable_export_secret_found"), "{error}");
    assert!(error.contains("issues row"), "{error}");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn portable_import_refuses_a_bundle_from_a_newer_schema() {
    let dir = tempfile::tempdir().unwrap();
    let source = open(dir.path(), "source.db");
    seed(&source);
    let mut bundle = source.export_portable_bundle(&[]).unwrap();
    bundle.schema_version = super::super::LATEST_SCHEMA_VERSION + 1;
    let path = dir.path().join("newer.json");
    write_bundle(&path, &bundle, false).unwrap();

    let error = read_bundle(&path).unwrap_err().to_string();
    assert!(error.contains("portable_bundle_schema_too_new"), "{error}");
    let target = open(dir.path(), "target.db");
    let error = target
        .import_portable_bundle(&bundle, &PortableImportOptions::default())
        .unwrap_err()
        .to_string();
    assert!(error.contains("portable_bundle_schema_too_new"), "{error}");
    assert_eq!(count(&target, "projects"), 0);

    bundle.schema_version = super::super::LATEST_SCHEMA_VERSION;
    bundle.format_version = PORTABLE_BUNDLE_FORMAT_VERSION + 1;
    let error = target
        .import_portable_bundle(&bundle, &PortableImportOptions::default())
        .unwrap_err()
        .to_string();
    assert!(error.contains("portable_bundle_format_too_new"), "{error}");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn portable_import_refuses_a_non_empty_database_unless_merging() {
    let dir = tempfile::tempdir().unwrap();
    let source = open(dir.path(), "source.db");
    seed(&source);
    let (target, _) = round_trip(dir.path(), &source, &PortableImportOptions::default());
    let bundle = source.export_portable_bundle(&[]).unwrap();

    let error = target
        .import_portable_bundle(&bundle, &PortableImportOptions::default())
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("portable_import_target_not_empty"),
        "{error}"
    );
    assert_eq!(count(&target, "issues"), 2);

    let merge = PortableImportOptions {
        merge: true,
        ..PortableImportOptions::default()
    };
    let report = target.import_portable_bundle(&bundle, &merge).unwrap();
    assert_eq!(report.inserted.get("issues"), Some(&0));
    assert_eq!(report.skipped.get("issues"), Some(&2));
    assert_eq!(report.skipped.get("permission_rules"), Some(&2));
    for (table, expected) in [
        ("projects", 2),
        ("issues", 2),
        ("issue_events", 3),
        ("issue_deps", 1),
        ("permission_rules", 2),
    ] {
        assert_eq!(count(&target, table), expected, "{table}");
    }

    // A failing row rolls the whole import back.
    let fresh = open(dir.path(), "fresh.db");
    let mut broken = bundle.clone();
    broken.tables.get_mut("issue_events").unwrap()[0].insert(
        "issue_id".to_string(),
        Value::String(Uuid::new_v4().to_string()),
    );
    assert!(
        fresh
            .import_portable_bundle(&broken, &PortableImportOptions::default())
            .is_err()
    );
    for table in ["projects", "issues", "issue_events"] {
        assert_eq!(count(&fresh, table), 0, "{table}");
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn portable_import_remaps_project_paths() {
    let dir = tempfile::tempdir().unwrap();
    let source = open(dir.path(), "source.db");
    let fixture = seed(&source);
    let new_root = dir.path().join("work");
    let options = PortableImportOptions {
        merge: false,
        path_remaps: vec![PathRemap {
            from: format!("{OLD_ROOT}/"),
            to: new_root.to_string_lossy().into_owned(),
        }],
        dry_run: false,
    };

    // A dry run previews the remapped paths and commits nothing.
    let preview = open(dir.path(), "preview.db");
    let dry = preview
        .import_portable_bundle(
            &source.export_portable_bundle(&[]).unwrap(),
            &PortableImportOptions {
                dry_run: true,
                ..options.clone()
            },
        )
        .unwrap();
    assert!(dry.dry_run);
    let previewed = dry.projects.iter().find(|p| p.name == "rsi").unwrap();
    let expected = new_root.join("rsi").to_string_lossy().into_owned();
    assert_eq!(previewed.path.as_deref(), Some(expected.as_str()));
    for table in ["projects", "issues", "issue_events"] {
        assert_eq!(count(&preview, table), 0, "{table}");
    }

    let (target, report) = round_trip(dir.path(), &source, &options);

    let project = target
        .load_projects()
        .unwrap()
        .into_iter()
        .find(|project| project.id == fixture.project)
        .unwrap();
    assert_eq!(project.path, Some(new_root.join("rsi")));
    let notes = target
        .load_projects()
        .unwrap()
        .into_iter()
        .find(|project| project.name == "notes")
        .unwrap();
    assert_eq!(notes.path, Some(PathBuf::from("/srv/notes")));
    let remapped = report
        .projects
        .iter()
        .find(|project| project.name == "rsi")
        .unwrap();
    assert_eq!(
        remapped.remapped_from.as_deref(),
        Some("/home/olduser/code/rsi")
    );

    // Bundles from another OS remap too; a sibling prefix does not match.
    let remaps = [PathRemap {
        from: r"C:\Users\jake".to_string(),
        to: "/home/jake".to_string(),
    }];
    assert_eq!(
        remap_path(r"C:\Users\jake\code\rsi", &remaps).map(PathBuf::from),
        Some(PathBuf::from("/home/jake").join("code").join("rsi"))
    );
    assert_eq!(
        remap_path(r"C:\Users\jake", &remaps).as_deref(),
        Some("/home/jake")
    );
    assert_eq!(remap_path(r"C:\Users\jakeb\code", &remaps), None);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn portable_settings_filter_withholds_runtime_and_secret_keys() {
    for key in [
        "context_rotation_claude_pct",
        "api_route.openrouter",
        "provider_profile",
        "mcp.server.github",
        "openrouter_context_budget_tokens",
    ] {
        assert!(!setting_is_withheld(key), "{key}");
    }
    for key in [
        "c5.autofile.activation.v1",
        "model_control_circuits",
        "target_reclaim_recent_cursor",
        "worker_baton:52bb1028-a500-4bd0-82b9-f81d2c7b5feb",
        "some_scope:52bb1028-a500-4bd0-82b9-f81d2c7b5feb:x",
        "linear_api_key",
        "remote.bearer",
    ] {
        assert!(setting_is_withheld(key), "{key}");
    }
}

/// #1448: a withheld settings key can itself be a credential
/// (`credential:<token>`); it must reach neither the bundle nor the summary.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn portable_withheld_setting_key_holding_a_secret_never_leaves() {
    const CANARY: &str = "sk-test-canary-1448-abcdef";
    let dir = tempfile::tempdir().unwrap();
    let source = open(dir.path(), "source.db");
    seed(&source);
    source
        .set_daemon_setting(&format!("credential:{CANARY}"), "x")
        .unwrap();
    // Runtime-state key that is not secret-named but holds the secret.
    source
        .set_daemon_setting(&format!("worker_baton:{CANARY}"), "x")
        .unwrap();
    source
        .set_daemon_setting("worker_baton:plain-runtime-key", "x")
        .unwrap();
    let secrets = [SecretString::new(CANARY.to_string())];

    let bundle = source.export_portable_bundle(&secrets).unwrap();
    let summary = bundle.summary();
    for text in [
        serde_json::to_string(&bundle).unwrap(),
        serde_json::to_string(&summary).unwrap(),
    ] {
        assert!(!text.contains(CANARY), "credential reached the output");
    }
    // The operator can still tell what was withheld.
    assert_eq!(
        bundle
            .withheld_settings
            .iter()
            .filter(|label| label.starts_with("redacted:"))
            .count(),
        2
    );
    assert!(
        bundle
            .withheld_settings
            .contains(&"worker_baton:plain-runtime-key".to_string())
    );
}

/// #1448: the scan covers the whole serialized bundle and the summary, not
/// only carried tables, whatever field or map key holds the credential.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn portable_secret_scan_covers_every_field_of_the_bundle() {
    const CANARY: &str = "sk-test-canary-1448-abcdef";
    let secrets = [SecretString::new(CANARY.to_string())];
    let clean = || PortableBundle {
        format: PORTABLE_BUNDLE_FORMAT.to_string(),
        format_version: PORTABLE_BUNDLE_FORMAT_VERSION,
        schema_version: 1,
        exported_at: "2026-01-01T00:00:00.000000000Z".to_string(),
        source_os: "linux".to_string(),
        tables: BTreeMap::new(),
        manager_templates: BTreeMap::new(),
        left_behind: BTreeMap::new(),
        withheld_settings: Vec::new(),
    };
    refuse_secrets(&clean(), &secrets).unwrap();

    let mut cases: Vec<(&str, PortableBundle)> = Vec::new();
    let mut b = clean();
    b.withheld_settings = vec![format!("credential:{CANARY}")];
    cases.push(("withheld_settings", b));
    let mut b = clean();
    b.source_os = format!("os {CANARY}");
    cases.push(("source_os", b));
    let mut b = clean();
    b.left_behind.insert(format!("t_{CANARY}"), 1);
    cases.push(("left_behind key", b));
    let mut b = clean();
    b.tables.insert(
        format!("t_{CANARY}"),
        vec![Map::from_iter([("c".to_string(), Value::Null)])],
    );
    cases.push(("unknown table key", b));
    let mut b = clean();
    b.tables.insert(
        "unknown".to_string(),
        vec![Map::from_iter([(
            "c".to_string(),
            serde_json::json!({"nested": [format!("{{\"k\":\"{CANARY}\"}}")]}),
        )])],
    );
    cases.push(("nested JSON string in unknown table", b));
    for (name, bundle) in cases {
        let error = refuse_secrets(&bundle, &secrets).unwrap_err().to_string();
        assert!(
            error.contains("portable_export_secret_found"),
            "{name}: {error}"
        );
        assert!(!error.contains(CANARY), "{name}: error names the secret");
    }
}
