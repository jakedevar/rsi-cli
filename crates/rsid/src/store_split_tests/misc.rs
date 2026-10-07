// Tests moved out of `rsid-store` (issue #1021 S4); see `mod.rs`.
use crate::config::{Config, RuntimeConfig};
use crate::provider_capabilities::{ContextBudgetRequest, ProviderCapabilityRegistry};
use rsi_common::provider_capabilities::{CapabilityConfidence, CapabilitySource};
use rsi_common::types::{NewIssue, SessionProvider};
use rsid_store::idea_control::{IdeaControlError, IssueLinkConstraintClass};
use rsid_store::store::Store;
use rsid_store::store::d04_test_project_id;
use rsid_store::test_support::make_test_session;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use uuid::Uuid;

fn make_new_issue(title: &str) -> NewIssue {
    NewIssue {
        project_id: d04_test_project_id(),
        title: title.to_string(),
        body: String::new(),
        priority: None,
        labels: Vec::new(),
        created_by_session_id: None,
        assignee: None,
        idea_id: None,
        source_event_id: None,
        source_finding_ref: None,
    }
}

// moved from rsid-store/src/sandbox/cleanup.rs (issue #1021 S4: needs rsid modules)
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn generic_lifecycle_has_no_destructive_worktree_reachability() {
    let lifecycle = include_str!("../session/lifecycle.rs");
    let launch = include_str!("../session/launch.rs");
    for source in [lifecycle, launch] {
        for forbidden in [
            "git_worktree::destroy",
            "destroy_clean_worktree",
            "remove_worktree_non_force_locked",
            "remove_worktree_after_source_ref_delete_non_force_locked",
            "delete_ref_compare_locked",
        ] {
            assert!(
                !source.contains(forbidden),
                "generic lifecycle source reaches destructive primitive {forbidden}"
            );
        }
    }

    let git_worktree = include_str!("../../../rsid-store/src/sandbox/git_worktree.rs");
    for signature in ["pub fn destroy(", "pub fn destroy_by_path("] {
        let offset = git_worktree
            .find(signature)
            .unwrap_or_else(|| panic!("missing raw helper {signature}"));
        let prefix_start = offset.saturating_sub(160);
        assert!(
            git_worktree[prefix_start..offset].contains("#[cfg(test)]"),
            "raw helper {signature} is not test-only"
        );
    }
}

// moved from rsid-store/src/config.rs (issue #1021 S4: needs rsid modules)
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn codegraph_indexing_defaults_off_and_validates_rpc_values() {
    let runtime = RuntimeConfig::from_config(&Config::default());
    let shared = Arc::clone(&runtime.codegraph_indexing_enabled);
    assert_eq!(runtime.to_json()["codegraph_indexing_enabled"], false);
    assert_eq!(
        runtime.persisted_field_value("codegraph_indexing_enabled"),
        Some(serde_json::json!(false))
    );

    let params: crate::rpc::UpdateDaemonConfigParams = serde_json::from_value(
        serde_json::json!({"field": "codegraph_indexing_enabled", "value": true}),
    )
    .unwrap();
    assert_eq!(runtime.update_field(&params.field, &params.value), Ok(true));
    assert!(shared.load(Ordering::Relaxed));
    assert_eq!(runtime.to_json()["codegraph_indexing_enabled"], true);

    for invalid in [
        serde_json::json!(1),
        serde_json::json!("true"),
        serde_json::Value::Null,
    ] {
        assert!(runtime.update_field(&params.field, &invalid).is_err());
        assert!(shared.load(Ordering::Relaxed));
    }
    assert_eq!(
        runtime.update_field(&params.field, &serde_json::json!(false)),
        Ok(true)
    );
    assert!(!shared.load(Ordering::Relaxed));
}

// moved from rsid-store/src/store/tests.rs (issue #1021 S4: needs rsid modules)
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn discovery_only_catalog_identity_survives_store_reopen() {
    let directory = tempfile::tempdir().expect("create discovery-only catalog fixture");
    let database = directory.path().join("discovery-only-catalog.sqlite");
    let store = Store::open(&database).expect("open discovery-only catalog store");
    let mut session = make_test_session();
    session.provider = SessionProvider::Codex;
    session.model = Some("gpt-6-astra".to_string());
    let registry = ProviderCapabilityRegistry::default();
    registry
        .refresh_codex_catalog(
            "codex-cli 0.153.0",
            include_bytes!("../../tests/fixtures/codex-models-0.155.1.json"),
            "2026-09-03T12:34:56.123456789Z"
                .parse()
                .expect("fixed discovery observation"),
        )
        .expect("parse discovery-only future catalog");
    let mut request = ContextBudgetRequest::new(SessionProvider::Codex, "gpt-6-astra");
    request.configured_tokens = Some(400_000);
    let budget = registry.resolve_context_budget(request);
    assert_eq!(budget.active_tokens, 258_400);
    assert_eq!(budget.capacity.configured_tokens, Some(400_000));
    assert_eq!(budget.capacity.effective_percent, None);
    assert_eq!(budget.evidence.source, CapabilitySource::LegacyUnverified);
    assert_eq!(budget.evidence.confidence, CapabilityConfidence::Degraded);
    assert_eq!(
        budget.evidence.source_version.as_deref(),
        Some("codex-cli 0.153.0")
    );
    assert!(budget.evidence.source_digest.is_some());
    assert!(!budget.authorizes_threshold_rotation());
    session.context_window = Some(budget.active_tokens);
    session.resolved_context_budget = Some(budget);
    store
        .insert_session(&session)
        .expect("persist discovery-only catalog identity");
    drop(store);

    let reopened = Store::open(&database).expect("reopen discovery-only catalog store");
    let loaded = reopened
        .get_session(session.id)
        .unwrap()
        .expect("discovery-only session survives reopen");
    assert_eq!(loaded.context_window, session.context_window);
    let loaded_budget = loaded
        .resolved_context_budget
        .expect("discovery-only evidence survives reopen");
    let expected_budget = session
        .resolved_context_budget
        .expect("fixture has discovery-only evidence");
    assert_eq!(loaded_budget.active_tokens, expected_budget.active_tokens);
    assert_eq!(
        loaded_budget.evidence, expected_budget.evidence,
        "the exact untrusted catalog identity must survive restart"
    );
    assert_eq!(loaded_budget.capacity.configured_tokens, Some(400_000));
    assert_eq!(loaded_budget.capacity.provider_default_tokens, None);
    assert_eq!(loaded_budget.capacity.provider_max_tokens, None);
    assert_eq!(loaded_budget.capacity.effective_percent, None);
    assert!(!loaded_budget.authorizes_threshold_rotation());
}

// moved from rsid-store/src/store/tests.rs (issue #1021 S4: needs rsid modules)
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn d04_uuidv5_domains_and_display_numbers_are_unchanged() {
    let store = std::sync::Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
    let active = std::sync::Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
    let completed = std::sync::Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
    let (sender, _receiver) = tokio::sync::mpsc::channel(1);
    let control = crate::session::agent_verbs::AgentControlHandle::new(
        active,
        completed,
        std::sync::Arc::clone(&store),
        std::sync::Arc::new(crate::bus::EventBus::new(4)),
        std::sync::Arc::new(crate::session::spawn_coordinator::SpawnCoordinator::new(
            sender,
        )),
    );
    let caller = Uuid::new_v4();
    store
        .lock()
        .await
        .insert_session(&{
            let mut session = make_test_session();
            session.id = caller;
            session
        })
        .unwrap();
    let params = rsi_common::rpc::AgentCreateIssueParams {
        project_id: None,
        title: "attributed".into(),
        body: String::new(),
        priority: None,
        labels: vec![],
        assignee: None,
        idempotency_key: "d04-pinned-domain".into(),
        harness: false,
        source_issue: None,
    };
    let attributed = control
        .agent_create_issue(caller, params.clone())
        .await
        .unwrap();
    let mut name = b"agent-create\0".to_vec();
    name.extend_from_slice(caller.to_string().as_bytes());
    name.push(0);
    name.extend_from_slice(params.idempotency_key.as_bytes());
    assert_eq!(
        attributed.issue.id,
        Uuid::new_v5(
            &Uuid::from_u128(0x5da3_4881_ecc9_54fb_a7da_3913_bc97_8130),
            &name
        )
    );
    let operator = store
        .lock()
        .await
        .create_issue(&make_new_issue("operator"))
        .unwrap();
    assert_eq!(
        (attributed.issue.display_number, operator.display_number),
        (1, 2),
        "UUIDv5 attributed creation must not fork global display allocation"
    );
}

// moved from rsid-store/src/store/tests.rs (issue #1021 S4: needs rsid modules)
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
#[allow(clippy::unwrap_used)]
fn d04_agent_and_native_schemas_reject_linkage_spoof_fields() {
    let schema = rsi_common::agent_control_schema::AgentControlVerbV1::CreateIssue
        .descriptor()
        .parameters();
    // #1235: project_id is the global seat's optional target project, checked
    // against the grant in the Store; it is never creator identity.
    let mut targeted = serde_json::json!({"title":"x","idempotency_key":"k"});
    targeted["project_id"] = serde_json::json!(Uuid::new_v4());
    assert!(
        serde_json::from_value::<rsi_common::rpc::AgentCreateIssueParams>(targeted.clone())
            .is_ok_and(|params| params.project_id.is_some())
    );
    assert!(
        crate::session::harness::tools::rsi_control::agent_create_issue_from_args(&targeted)
            .is_ok()
    );
    assert_eq!(
        schema["properties"]["project_id"]["default"],
        serde_json::Value::Null
    );
    for field in [
        "idea_id",
        "source_event_id",
        "source_finding_ref",
        "created_by_session_id",
        "caller_session_id",
        "actor_id",
    ] {
        let mut payload = serde_json::json!({"title":"x","idempotency_key":"k"});
        payload[field] = serde_json::json!(Uuid::new_v4());
        // Tokened AgentCreateIssue deserializes into this strict request.
        assert!(
            serde_json::from_value::<rsi_common::rpc::AgentCreateIssueParams>(payload.clone())
                .is_err(),
            "tokened RPC accepted {field}"
        );
        // Harness parses before invoking the guarded Store path.
        assert!(
            crate::session::harness::tools::rsi_control::agent_create_issue_from_args(&payload)
                .is_err(),
            "Harness accepted {field}"
        );
        // CodexAppServer registers this exact shared schema/parser pair.
        assert!(
            schema["properties"].get(field).is_none(),
            "native schema exposed {field}"
        );
    }
}

// moved from rsid-store/src/store/tests.rs (issue #1021 S4: needs rsid modules)
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
#[allow(clippy::unwrap_used)]
fn d04_link_constraint_classes_are_typed_and_rpc_data_is_redacted() {
    use crate::store::ideas::{SqliteFailureClass, classify_sqlite_error, map_sql_error};

    fn sqlite_failure(code: i32) -> rusqlite::Error {
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(code),
            Some("raw SQL must stay private".into()),
        )
    }

    for (sqlite_code, failure_class, constraint_class) in [
        (
            rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY,
            SqliteFailureClass::ForeignKey,
            IssueLinkConstraintClass::ForeignKey,
        ),
        (
            rusqlite::ffi::SQLITE_CONSTRAINT_CHECK,
            SqliteFailureClass::Check,
            IssueLinkConstraintClass::Check,
        ),
        (
            rusqlite::ffi::SQLITE_CONSTRAINT_NOTNULL,
            SqliteFailureClass::NotNull,
            IssueLinkConstraintClass::NotNull,
        ),
        (
            rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE,
            SqliteFailureClass::UniqueOrPrimary,
            IssueLinkConstraintClass::UniquePrimary,
        ),
        (
            rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY,
            SqliteFailureClass::UniqueOrPrimary,
            IssueLinkConstraintClass::UniquePrimary,
        ),
    ] {
        assert_eq!(
            classify_sqlite_error(&sqlite_failure(sqlite_code)),
            failure_class
        );
        let error = map_sql_error(sqlite_failure(sqlite_code));
        assert_eq!(
            error,
            IdeaControlError::ConstraintViolation {
                class: constraint_class
            }
        );
        let rpc = crate::rpc::link_issue_rpc_error(&error);
        let crate::error::DaemonError::StructuredRpc {
            rpc_code,
            message,
            data,
        } = rpc
        else {
            panic!("constraint error must remain a structured RPC failure");
        };
        assert_eq!(rpc_code, rsi_common::rpc::INTERNAL_ERROR);
        assert_eq!(message, "Issue link integrity constraint failed");
        assert_eq!(data["code"], "ISSUE_LINK_CONSTRAINT_FAILURE");
        assert_eq!(data["details"]["class"], constraint_class.as_str());
        assert!(!data.to_string().contains("raw SQL must stay private"));
    }
}

// Pioneer half of the vault `imported_entry_keeps_its_source_env_name` test
// (issue #1021 S4: `pioneer` is an `rsid` module; the vault half stays in
// `rsid-store`).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn pioneer_credential_source_follows_the_vault_import_origin() {
    use rsid_store::vault::{CredentialSource, Slot, VaultHandleBuilder, VaultSettings};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    let root = tempfile::tempdir().unwrap();
    let env = Arc::new(Mutex::new(HashMap::from([(
        "PIONEER_API_KEY".to_string(),
        "sk-test-pioneer-fallback-0014".to_string(),
    )])));
    let reader = Arc::clone(&env);
    let vault = VaultHandleBuilder::new(Arc::new(VaultSettings::default()))
        .dir(root.path().join("vault"))
        .env(move |name| reader.lock().unwrap().get(name).cloned())
        .open()
        .unwrap();
    vault.import_from_env().unwrap();
    env.lock().unwrap().clear();
    let resolved = vault.resolve(Slot::Pioneer).unwrap().unwrap();
    assert_eq!(resolved.source, CredentialSource::Vault);
    assert_eq!(
        crate::pioneer::pioneer_credential_from(&vault)
            .unwrap()
            .source(),
        crate::pioneer::PioneerCredentialSource::ApiKey
    );
    // A later Set has no env origin and injects under the primary name.
    vault
        .set(Slot::Pioneer, "sk-test-pioneer-set-0015")
        .unwrap();
    assert_eq!(
        crate::pioneer::pioneer_credential_from(&vault)
            .unwrap()
            .source(),
        crate::pioneer::PioneerCredentialSource::AiInference
    );
}
