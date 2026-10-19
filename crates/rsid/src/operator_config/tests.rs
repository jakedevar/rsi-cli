//! hub #3039 operator configuration RPCs. Every test works in a temporary
//! directory: no test reads or writes the operator's `~/.claude/settings.json`,
//! `~/.rsi/state.json` or vault. Keys are obviously fake.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::config::{Config, RuntimeConfig};
use crate::vault::{VaultHandleBuilder, VaultSettings};
use rsi_common::operator_config::{
    CLAUDE_HOOKS_ABSENT_REVISION, CLAUDE_HOOKS_CONFLICT, CustomProviderKeyState,
    METHOD_DELETE_CLAUDE_HOOKS, METHOD_DELETE_CUSTOM_PROVIDER, METHOD_DELETE_MESSAGE_BRIDGE,
    METHOD_GET_CUSTOM_PROVIDER, METHOD_GET_MESSAGE_BRIDGE, METHOD_LIST_CLAUDE_HOOKS,
    METHOD_LIST_CUSTOM_PROVIDER_ROUTES, METHOD_LIST_CUSTOM_PROVIDERS, METHOD_LIST_MESSAGE_BRIDGES,
    METHOD_PUT_CLAUDE_HOOKS, METHOD_PUT_CUSTOM_PROVIDER, METHOD_PUT_MESSAGE_BRIDGE,
    METHOD_SET_CUSTOM_PROVIDER_ROUTE,
};
use std::fs;
use std::sync::Arc;
use tracing_subscriber::layer::SubscriberExt as _;
use uuid::Uuid;

pub(super) const FAKE_KEY: &str = "fake-key-3039-not-a-real-credential";
const FAKE_KEY_ROTATED: &str = "fake-key-3039-rotated-not-real";

pub(super) struct Fixture {
    _root: tempfile::TempDir,
    pub(super) vault_dir: std::path::PathBuf,
    pub(super) store: Store,
    pub(super) vault: VaultHandle,
    pub(super) runtime_config: Arc<RuntimeConfig>,
    pub(super) paths: OperatorConfigPaths,
}

impl Fixture {
    pub(super) fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let vault_dir = root.path().join("vault");
        let vault = VaultHandleBuilder::new(Arc::new(VaultSettings::default()))
            .dir(&vault_dir)
            .env(|_| None)
            .open()
            .unwrap();
        let paths = OperatorConfigPaths::under(root.path());
        Self {
            vault_dir,
            store: Store::open_in_memory().unwrap(),
            vault,
            runtime_config: RuntimeConfig::from_config(&Config::default()),
            paths,
            _root: root,
        }
    }

    pub(super) fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        handle(
            &OperatorConfigContext {
                store: &self.store,
                vault: &self.vault,
                runtime_config: &self.runtime_config,
                paths: &self.paths,
            },
            method,
            false,
            &params,
        )
    }

    pub(super) fn ok(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
        self.call(method, params)
            .unwrap_or_else(|error| panic!("{method} failed: {error}"))
    }
}

struct LogLayer {
    events: Arc<std::sync::Mutex<Vec<String>>>,
}

struct FieldVisitor(Vec<String>);

impl tracing::field::Visit for FieldVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.push(format!("{}={:?}", field.name(), value));
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for LogLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visitor = FieldVisitor(Vec::new());
        event.record(&mut visitor);
        self.events.lock().unwrap().push(visitor.0.join(" "));
    }
}

fn mentions(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
#[test]
fn put_key_round_trips_to_the_vault_and_no_response_row_or_log_carries_it() {
    let fixture = Fixture::new();
    let logs = Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(LogLayer {
        events: Arc::clone(&logs),
    });
    let mut responses = Vec::new();
    let id = tracing::subscriber::with_default(subscriber, || {
        let created = fixture.ok(
            METHOD_PUT_CUSTOM_PROVIDER,
            serde_json::json!({
                "name": "Groq",
                "base_url": "https://api.example.invalid/openai/v1",
                "default_model": "llama-test",
                "api_key": {"action": "set", "secret": FAKE_KEY},
            }),
        );
        let id: Uuid = serde_json::from_value(created["id"].clone()).unwrap();
        responses.push(created);
        responses.push(fixture.ok(METHOD_LIST_CUSTOM_PROVIDERS, serde_json::Value::Null));
        responses.push(fixture.ok(METHOD_GET_CUSTOM_PROVIDER, serde_json::json!({"id": id})));
        responses.push(fixture.ok(
            METHOD_SET_CUSTOM_PROVIDER_ROUTE,
            serde_json::json!({"role": "title_model", "id": id}),
        ));
        // A malformed request carrying the key is refused without echoing it.
        let refused = fixture
            .call(
                METHOD_PUT_CUSTOM_PROVIDER,
                serde_json::json!({"name": "x", "api_key": {"action": "set", "secret": FAKE_KEY}, "bogus": 1}),
            )
            .unwrap_err();
        responses.push(serde_json::json!(refused.to_string()));
        id
    });

    // Positive end state: the vault holds the key and the role resolves it.
    assert_eq!(
        fixture.vault.custom_provider_key(id).unwrap().expose(),
        FAKE_KEY
    );
    let list = &responses[1]["providers"][0];
    assert_eq!(list["name"], "Groq");
    assert_eq!(list["api_key"]["state"], "set");
    assert_eq!(
        list["api_key"]["fingerprint"],
        fixture
            .vault
            .custom_provider_key_metadata(id)
            .fingerprint
            .unwrap()
    );
    assert_eq!(responses[3]["api_key_set"], true);
    assert_eq!(
        fixture.runtime_config.title_model_api_key.read().as_deref(),
        Some(FAKE_KEY)
    );
    assert_eq!(
        fixture
            .runtime_config
            .title_model_base_url
            .read()
            .as_deref(),
        Some("https://api.example.invalid/openai/v1")
    );

    // The key file is the vault's own, 0600; the provider rows are clean.
    let key_file = fixture.vault_dir.join("custom-provider-keys.json");
    assert_eq!(
        mentions(&fs::read_to_string(&key_file).unwrap(), FAKE_KEY),
        1
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            fs::metadata(&key_file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    for response in &responses {
        assert_eq!(mentions(&response.to_string(), FAKE_KEY), 0, "{response}");
    }
    for event in logs.lock().unwrap().iter() {
        assert_eq!(mentions(event, FAKE_KEY), 0, "{event}");
    }
    for (_, value) in fixture
        .store
        .list_daemon_settings_with_prefix("custom_provider")
        .unwrap()
    {
        assert_eq!(mentions(&value, FAKE_KEY), 0, "{value}");
    }
    assert_eq!(
        mentions(&fixture.runtime_config.to_json().to_string(), FAKE_KEY),
        0
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
#[test]
fn edit_keeps_or_replaces_the_key_and_delete_tombstones_and_clears_roles() {
    let fixture = Fixture::new();
    let created = fixture.ok(
        METHOD_PUT_CUSTOM_PROVIDER,
        serde_json::json!({
            "name": "Local",
            "base_url": "http://127.0.0.1:8080/v1",
            "api_key": {"action": "set", "secret": FAKE_KEY},
        }),
    );
    let id: Uuid = serde_json::from_value(created["id"].clone()).unwrap();
    let first = fixture.vault.custom_provider_key_metadata(id).fingerprint;

    // Keep (the default) leaves the key; the other fields change.
    let kept = fixture.ok(
        METHOD_PUT_CUSTOM_PROVIDER,
        serde_json::json!({"id": id, "name": "Local 2", "base_url": "http://127.0.0.1:9090/v1"}),
    );
    assert_eq!(kept["name"], "Local 2");
    assert_eq!(kept["api_key"]["state"], "set");
    assert_eq!(
        fixture.vault.custom_provider_key_metadata(id).fingerprint,
        first
    );

    fixture.ok(
        METHOD_SET_CUSTOM_PROVIDER_ROUTE,
        serde_json::json!({"role": "dream_model", "id": id}),
    );
    // A rotated key reaches the role without a new route call.
    fixture.ok(
        METHOD_PUT_CUSTOM_PROVIDER,
        serde_json::json!({
            "id": id, "name": "Local 2", "base_url": "http://127.0.0.1:9090/v1",
            "api_key": {"action": "set", "secret": FAKE_KEY_ROTATED},
        }),
    );
    assert_eq!(
        fixture.runtime_config.dream_model_api_key.read().as_deref(),
        Some(FAKE_KEY_ROTATED)
    );
    assert_eq!(
        fixture
            .runtime_config
            .dream_model_base_url
            .read()
            .as_deref(),
        Some("http://127.0.0.1:9090/v1")
    );

    // The route read shows the stored assignment (name and key metadata
    // only) for every role, in role order; a rotated key shows the new
    // fingerprint and never the key.
    let routes = fixture.ok(METHOD_LIST_CUSTOM_PROVIDER_ROUTES, serde_json::Value::Null);
    let roles: Vec<_> = routes["routes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|route| route["role"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        roles,
        [
            "title_model",
            "memory_model_fallback",
            "dream_model",
            "prompt_compile_model"
        ]
    );
    assert_eq!(routes["routes"][2]["id"], serde_json::json!(id));
    assert_eq!(routes["routes"][2]["name"], "Local 2");
    assert_eq!(routes["routes"][2]["api_key"]["state"], "set");
    assert_eq!(
        routes["routes"][2]["api_key"]["fingerprint"],
        serde_json::json!(fixture.vault.custom_provider_key_metadata(id).fingerprint)
    );
    assert_eq!(routes["routes"][0]["id"], serde_json::Value::Null);
    assert_eq!(routes["routes"][0]["api_key"], serde_json::Value::Null);
    let text = routes.to_string();
    assert!(!text.contains(FAKE_KEY) && !text.contains(FAKE_KEY_ROTATED));

    let deleted = fixture.ok(METHOD_DELETE_CUSTOM_PROVIDER, serde_json::json!({"id": id}));
    assert_eq!(deleted["deleted"], true);
    assert_eq!(
        deleted["cleared_routes"],
        serde_json::json!(["dream_model"])
    );
    assert_eq!(
        fixture.vault.custom_provider_key_metadata(id).state,
        CustomProviderKeyState::Absent
    );
    assert_eq!(*fixture.runtime_config.dream_model_api_key.read(), None);
    let list = fixture.ok(METHOD_LIST_CUSTOM_PROVIDERS, serde_json::Value::Null);
    assert_eq!(list["providers"], serde_json::json!([]));
    let routes = fixture.ok(METHOD_LIST_CUSTOM_PROVIDER_ROUTES, serde_json::Value::Null);
    assert!(
        routes["routes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|route| route["id"].is_null())
    );
    // The row is tombstoned, not removed.
    let row = fixture
        .store
        .get_daemon_setting(&format!("custom_provider.{id}"))
        .unwrap()
        .unwrap();
    assert!(row.contains("deleted_at"), "{row}");
    assert!(
        fixture
            .call(METHOD_GET_CUSTOM_PROVIDER, serde_json::json!({"id": id}))
            .is_err()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
#[test]
fn provider_fields_are_validated_and_urls_may_not_carry_credentials() {
    let fixture = Fixture::new();
    for (name, base_url) in [
        ("", "https://example.invalid/v1"),
        ("ok", ""),
        ("ok", "ftp://example.invalid"),
        ("ok", "https://user:pass@example.invalid/v1"),
        ("bad\nname", "https://example.invalid/v1"),
    ] {
        let error = fixture
            .call(
                METHOD_PUT_CUSTOM_PROVIDER,
                serde_json::json!({"name": name, "base_url": base_url}),
            )
            .unwrap_err();
        assert!(
            matches!(error, DaemonError::InvalidParam(_)),
            "{name} {base_url}: {error}"
        );
    }
    let error = fixture
        .call(
            METHOD_PUT_CUSTOM_PROVIDER,
            serde_json::json!({"name": "a", "base_url": "https://example.invalid", "api_key": {"action": "set", "secret": "has space"}}),
        )
        .unwrap_err();
    assert!(error.to_string().contains("whitespace"), "{error}");
    assert_eq!(
        fixture.ok(METHOD_LIST_CUSTOM_PROVIDERS, serde_json::Value::Null)["providers"],
        serde_json::json!([])
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
#[test]
fn hooks_put_is_atomic_preserves_other_keys_and_refuses_a_stale_revision() {
    let fixture = Fixture::new();
    let absent = fixture.ok(METHOD_LIST_CLAUDE_HOOKS, serde_json::Value::Null);
    assert_eq!(absent["exists"], false);
    assert_eq!(absent["revision"], CLAUDE_HOOKS_ABSENT_REVISION);

    let settings = &fixture.paths.claude_settings;
    fs::create_dir_all(settings.parent().unwrap()).unwrap();
    fs::write(
        settings,
        r#"{"env": {"FOO": "1"}, "permissions": {"allow": ["Bash"]}, "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "echo stop"}]}]}}"#,
    )
    .unwrap();
    let listed = fixture.ok(METHOD_LIST_CLAUDE_HOOKS, serde_json::Value::Null);
    assert_eq!(listed["exists"], true);
    assert_eq!(
        listed["hooks"]["Stop"][0]["hooks"][0]["command"],
        "echo stop"
    );
    let revision = listed["revision"].as_str().unwrap().to_owned();

    // A stale revision is refused with the current one.
    let stale = fixture
        .call(
            METHOD_PUT_CLAUDE_HOOKS,
            serde_json::json!({"expected_revision": CLAUDE_HOOKS_ABSENT_REVISION, "hooks": {}}),
        )
        .unwrap_err();
    let DaemonError::StructuredRpc { message, data, .. } = stale else {
        panic!("expected a typed conflict, got {stale}");
    };
    assert!(message.starts_with(CLAUDE_HOOKS_CONFLICT), "{message}");
    assert_eq!(data["current_revision"], revision.as_str());

    let put = fixture.ok(
        METHOD_PUT_CLAUDE_HOOKS,
        serde_json::json!({
            "expected_revision": revision,
            "hooks": {
                "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "guard.sh", "timeout": 30}]}],
                "FutureEvent": [{"hooks": [{"type": "command", "command": "later.sh"}]}]
            }
        }),
    );
    assert_ne!(put["revision"], revision.as_str());
    let written: serde_json::Value = serde_json::from_slice(&fs::read(settings).unwrap()).unwrap();
    assert_eq!(written["env"]["FOO"], "1");
    assert_eq!(written["permissions"]["allow"][0], "Bash");
    assert_eq!(written["hooks"]["PreToolUse"][0]["matcher"], "Bash");
    assert_eq!(written["hooks"]["PreToolUse"][0]["hooks"][0]["timeout"], 30);
    assert_eq!(
        written["hooks"]["FutureEvent"][0]["hooks"][0]["command"],
        "later.sh"
    );
    assert!(written["hooks"].get("Stop").is_none());
    // No temporary file is left beside it.
    let siblings: Vec<_> = fs::read_dir(settings.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(siblings, [std::ffi::OsString::from("settings.json")]);

    // Invalid hooks are refused before any write.
    let invalid = fixture
        .call(
            METHOD_PUT_CLAUDE_HOOKS,
            serde_json::json!({
                "expected_revision": put["revision"],
                "hooks": {"Stop": [{"hooks": [{"type": "command", "command": " "}]}]}
            }),
        )
        .unwrap_err();
    assert!(matches!(invalid, DaemonError::InvalidParam(_)), "{invalid}");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
#[test]
fn bridge_put_writes_the_tui_config_shape_and_get_reads_it_back() {
    let fixture = Fixture::new();
    let listed = fixture.ok(METHOD_LIST_MESSAGE_BRIDGES, serde_json::Value::Null);
    assert_eq!(listed["bridges"][0]["kind"], "signal");
    assert_eq!(listed["bridges"][0]["config"], serde_json::Value::Null);
    assert_eq!(listed["bridges"][1]["kind"], "imessage");

    let put = fixture.ok(
        METHOD_PUT_MESSAGE_BRIDGE,
        serde_json::json!({
            "kind": "signal",
            "config": {"enabled": true, "account": "+15550000000", "allow_from": "+15550000001, +15550000002", "working_dir": "/tmp/work"}
        }),
    );
    assert_eq!(put["config"]["account"], "+15550000000");
    let text = fs::read_to_string(&fixture.paths.signal_bridge).unwrap();
    assert_eq!(
        text,
        "enabled = true\npoll_interval_ms = 2000\ndm_policy = \"allowlist\"\n\
allow_from = [\"+15550000001\", \"+15550000002\"]\nmax_message_length = 4000\n\
echo_cache_ttl_ms = 5000\ndebounce_ms = 500\ndefault_working_dir = \"/tmp/work\"\n\
default_provider = \"Claude\"\naccount = \"+15550000000\"\n"
    );
    let got = fixture.ok(
        METHOD_GET_MESSAGE_BRIDGE,
        serde_json::json!({"kind": "signal"}),
    );
    assert_eq!(got["config"]["allow_from"], "+15550000001, +15550000002");
    assert_eq!(got["config"]["working_dir"], "/tmp/work");

    for (kind, config) in [
        (
            "signal",
            serde_json::json!({"enabled": true, "account": "", "working_dir": "/tmp"}),
        ),
        (
            "imessage",
            serde_json::json!({"enabled": true, "working_dir": ""}),
        ),
    ] {
        let error = fixture
            .call(
                METHOD_PUT_MESSAGE_BRIDGE,
                serde_json::json!({"kind": kind, "config": config}),
            )
            .unwrap_err();
        assert!(
            matches!(error, DaemonError::InvalidParam(_)),
            "{kind}: {error}"
        );
    }
    assert!(!fixture.paths.imessage_bridge.exists());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
#[test]
fn hooks_delete_removes_only_the_hooks_key_and_honours_the_revision() {
    let fixture = Fixture::new();
    // Nothing to delete: an absent file stays absent.
    let absent = fixture.ok(
        METHOD_DELETE_CLAUDE_HOOKS,
        serde_json::json!({"expected_revision": CLAUDE_HOOKS_ABSENT_REVISION}),
    );
    assert_eq!(absent["exists"], false);
    assert!(!fixture.paths.claude_settings.exists());

    let settings = &fixture.paths.claude_settings;
    fs::create_dir_all(settings.parent().unwrap()).unwrap();
    fs::write(
        settings,
        r#"{"env": {"FOO": "1"}, "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "echo stop"}]}]}}"#,
    )
    .unwrap();
    let revision = fixture.ok(METHOD_LIST_CLAUDE_HOOKS, serde_json::Value::Null)["revision"]
        .as_str()
        .unwrap()
        .to_owned();

    // A stale revision is refused with the current one and changes nothing.
    let stale = fixture
        .call(
            METHOD_DELETE_CLAUDE_HOOKS,
            serde_json::json!({"expected_revision": CLAUDE_HOOKS_ABSENT_REVISION}),
        )
        .unwrap_err();
    let DaemonError::StructuredRpc { message, data, .. } = stale else {
        panic!("expected a typed conflict, got {stale}");
    };
    assert!(message.starts_with(CLAUDE_HOOKS_CONFLICT), "{message}");
    assert_eq!(data["current_revision"], revision.as_str());
    let before: serde_json::Value = serde_json::from_slice(&fs::read(settings).unwrap()).unwrap();
    assert_eq!(
        before["hooks"]["Stop"][0]["hooks"][0]["command"],
        "echo stop"
    );

    let deleted = fixture.ok(
        METHOD_DELETE_CLAUDE_HOOKS,
        serde_json::json!({"expected_revision": revision}),
    );
    assert_eq!(deleted["exists"], true);
    assert_eq!(deleted["hooks"], serde_json::json!({}));
    let written: serde_json::Value = serde_json::from_slice(&fs::read(settings).unwrap()).unwrap();
    assert!(written.get("hooks").is_none(), "{written}");
    assert_eq!(written["env"]["FOO"], "1");

    // Idempotent with the fresh revision.
    let again = fixture.ok(
        METHOD_DELETE_CLAUDE_HOOKS,
        serde_json::json!({"expected_revision": deleted["revision"]}),
    );
    assert_eq!(again["hooks"], serde_json::json!({}));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
#[test]
fn bridge_delete_removes_the_file_and_is_idempotent() {
    let fixture = Fixture::new();
    fixture.ok(
        METHOD_PUT_MESSAGE_BRIDGE,
        serde_json::json!({
            "kind": "imessage",
            "config": {"enabled": true, "working_dir": "/tmp/work"}
        }),
    );
    fixture.ok(
        METHOD_PUT_MESSAGE_BRIDGE,
        serde_json::json!({
            "kind": "signal",
            "config": {"enabled": true, "account": "+15550000000", "working_dir": "/tmp/work"}
        }),
    );
    assert!(fixture.paths.imessage_bridge.exists());

    let deleted = fixture.ok(
        METHOD_DELETE_MESSAGE_BRIDGE,
        serde_json::json!({"kind": "imessage"}),
    );
    assert_eq!(deleted["kind"], "imessage");
    assert_eq!(deleted["config"], serde_json::Value::Null);
    assert!(!fixture.paths.imessage_bridge.exists());
    // The other bridge is untouched.
    let signal = fixture.ok(
        METHOD_GET_MESSAGE_BRIDGE,
        serde_json::json!({"kind": "signal"}),
    );
    assert_eq!(signal["config"]["account"], "+15550000000");

    // Deleting an absent bridge succeeds.
    let again = fixture.ok(
        METHOD_DELETE_MESSAGE_BRIDGE,
        serde_json::json!({"kind": "imessage"}),
    );
    assert_eq!(again["config"], serde_json::Value::Null);

    // An unknown kind is an invalid request.
    let error = fixture
        .call(
            METHOD_DELETE_MESSAGE_BRIDGE,
            serde_json::json!({"kind": "sms"}),
        )
        .unwrap_err();
    assert!(matches!(error, DaemonError::InvalidParam(_)), "{error}");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
#[test]
fn tokened_callers_are_refused_before_parsing_for_every_method() {
    let fixture = Fixture::new();
    for method in rsi_common::operator_config::OPERATOR_METHODS {
        let error = handle(
            &OperatorConfigContext {
                store: &fixture.store,
                vault: &fixture.vault,
                runtime_config: &fixture.runtime_config,
                paths: &fixture.paths,
            },
            method,
            true,
            &serde_json::json!({"api_key": {"action": "set", "secret": FAKE_KEY}}),
        )
        .unwrap_err();
        let text = error.to_string();
        assert!(
            text.contains("not available to session-attributed callers"),
            "{method}: {text}"
        );
        assert_eq!(mentions(&text, FAKE_KEY), 0);
    }
    assert!(!fixture.paths.claude_settings.exists());
}
