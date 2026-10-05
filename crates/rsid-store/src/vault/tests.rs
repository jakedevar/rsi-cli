//! Vault resolution, persistence, import, metadata and check tests. All
//! secrets are fake `sk-test-...` values; absence assertions on them are
//! leaked-secret checks, not hidden-identity checks.

use super::check::ProbeOutcome;

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
#[test]
fn live_credit_exhaustion_is_fingerprint_bound() {
    let dir = tempfile::tempdir().unwrap();
    let vault = VaultHandleBuilder::new(Arc::new(VaultSettings::default()))
        .dir(dir.path().join("vault"))
        .env(|_| None)
        .open()
        .unwrap();
    let old = vault.set(Slot::Openai, "sk-old").unwrap();
    assert!(vault.mark_exhausted(Slot::Openai, &old, 402));
    let refusal = vault.admission(Slot::Openai).unwrap_err();
    assert_eq!(refusal.check.class, CredentialCheckClass::Exhausted);
    assert_eq!(refusal.check.detail_code, "live_credit_exhausted");
    let new = vault.rotate(Slot::Openai, "sk-new").unwrap();
    assert!(!vault.mark_exhausted(Slot::Openai, &old, 402));
    assert!(vault.admission(Slot::Openai).is_ok());
    assert!(vault.mark_exhausted(Slot::Openai, &new, 402));
}
use super::check::tests::{ScriptedProbe, http};
use super::*;
use chrono::Duration as ChronoDuration;
use rsi_common::provider_credentials::{
    CliExposure, CredentialRoute, ImportSkipReason, ListProviderCredentialsResult,
};
use std::collections::HashMap;
use std::sync::Mutex;

const ENV_SECRET: &str = "sk-test-env-value-0001";
const VAULT_SECRET: &str = "sk-test-vault-value-0002";
const GENERATED: &str = "bedrock-api-key-test-generated-0003";

/// Fake generator: Bedrock only, counts generations.
#[derive(Default)]
struct FakeGenerator {
    generated: std::sync::atomic::AtomicUsize,
}

impl DynamicSource for FakeGenerator {
    fn available(&self, slot: Slot) -> bool {
        slot == Slot::Bedrock
    }

    fn generate(&self, slot: Slot) -> Option<Result<SecretString, String>> {
        (slot == Slot::Bedrock).then(|| {
            self.generated
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(SecretString::new(GENERATED.into()))
        })
    }
}

/// Settable fake clock.
#[derive(Clone)]
struct FakeClock(Arc<Mutex<DateTime<Utc>>>);

impl FakeClock {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(
            DateTime::parse_from_rfc3339("2026-09-24T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        )))
    }
    fn advance(&self, secs: i64) {
        let mut now = self.0.lock().unwrap();
        *now += ChronoDuration::seconds(secs);
    }
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap()
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    dir: PathBuf,
    settings: Arc<VaultSettings>,
    env: Arc<Mutex<HashMap<String, String>>>,
    probe: Arc<ScriptedProbe>,
    clock: FakeClock,
    vault: VaultHandle,
}

fn fixture_with(env: &[(&str, &str)], probe: ScriptedProbe) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("vault");
    let settings = Arc::new(VaultSettings::default());
    let env = Arc::new(Mutex::new(
        env.iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect::<HashMap<_, _>>(),
    ));
    let probe = Arc::new(probe);
    let clock = FakeClock::new();
    let vault = build(&dir, &settings, &env, &probe, &clock);
    Fixture {
        _root: root,
        dir,
        settings,
        env,
        probe,
        clock,
        vault,
    }
}

fn build(
    dir: &Path,
    settings: &Arc<VaultSettings>,
    env: &Arc<Mutex<HashMap<String, String>>>,
    probe: &Arc<ScriptedProbe>,
    clock: &FakeClock,
) -> VaultHandle {
    let env_reader = Arc::clone(env);
    let clock = clock.clone();
    VaultHandleBuilder::new(Arc::clone(settings))
        .dir(dir)
        .env(move |name| env_reader.lock().unwrap().get(name).cloned())
        .dynamic(Arc::new(FakeGenerator::default()))
        .probe(Arc::clone(probe) as Arc<dyn CredentialProbe>)
        .clock(move || clock.now())
        .open()
        .unwrap()
}

fn fixture(env: &[(&str, &str)]) -> Fixture {
    fixture_with(env, ScriptedProbe::default())
}

fn resolved(vault: &VaultHandle, slot: Slot) -> Option<(String, CredentialSource)> {
    vault
        .resolve(slot)
        .unwrap()
        .map(|resolved| (resolved.secret.expose().to_string(), resolved.source))
}

// ---------------------------------------------------------------------------
// Resolution order
// ---------------------------------------------------------------------------

/// The per-slot resolution-order table (note "K: Key vault", steps 1-5):
///
/// | vault entry | tombstone | env var | `env_compat` | generator | result      |
/// |-------------|-----------|---------|------------|-----------|-------------|
/// | yes         | -         | yes     | on         | yes       | Vault       |
/// | no          | yes       | yes     | on         | yes       | None        |
/// | no          | no        | yes     | on         | yes       | `EnvCompat` |
/// | no          | no        | yes     | off        | yes       | Generator*  |
/// | no          | no        | no      | on         | yes       | Generator*  |
/// | no          | no        | no      | on         | no        | None        |
///
/// `*` only for `bedrock`; every other slot has no dynamic source (None).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
#[test]
fn resolution_order_table_holds_for_every_slot() {
    for slot in Slot::ALL {
        let env_var = slots::env_vars(slot)[0];
        let has_generator = slot == Slot::Bedrock;
        let generated = has_generator.then(|| (GENERATED.to_string(), CredentialSource::Generator));

        // Row 1: vault entry wins over env and generator.
        let f = fixture(&[(env_var, ENV_SECRET)]);
        f.vault.set(slot, VAULT_SECRET).unwrap();
        assert_eq!(
            resolved(&f.vault, slot),
            Some((VAULT_SECRET.to_string(), CredentialSource::Vault)),
            "{slot} row 1"
        );
        assert_eq!(f.vault.state(slot), CredentialState::Vault, "{slot}");

        // Row 2: tombstone suppresses env and generator.
        f.vault.clear(slot).unwrap();
        assert_eq!(resolved(&f.vault, slot), None, "{slot} row 2");
        assert_eq!(f.vault.state(slot), CredentialState::Cleared, "{slot}");
        assert!(!f.vault.resolvable(slot), "{slot}");

        // Row 3: env compat.
        let f = fixture(&[(env_var, ENV_SECRET)]);
        assert_eq!(
            resolved(&f.vault, slot),
            Some((ENV_SECRET.to_string(), CredentialSource::EnvCompat)),
            "{slot} row 3"
        );
        assert_eq!(f.vault.state(slot), CredentialState::EnvCompat, "{slot}");

        // Row 4: env compat off skips env, falls through to the generator.
        f.settings.env_compat.store(false, Ordering::SeqCst);
        assert_eq!(resolved(&f.vault, slot), generated, "{slot} row 4");

        // Row 5 / 6: no env.
        let f = fixture(&[]);
        assert_eq!(resolved(&f.vault, slot), generated, "{slot} row 5/6");
        assert_eq!(
            f.vault.state(slot),
            if has_generator {
                CredentialState::Generator
            } else {
                CredentialState::Absent
            },
            "{slot}"
        );
        assert_eq!(f.vault.resolvable(slot), has_generator, "{slot}");
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
#[test]
fn every_legacy_env_name_resolves_its_slot_in_order() {
    for slot in Slot::ALL {
        for (index, name) in slots::env_vars(slot).iter().enumerate() {
            let f = fixture(&[(name, ENV_SECRET)]);
            let resolved = f.vault.resolve(slot).unwrap().unwrap();
            assert_eq!(resolved.env_var, Some(*name), "{slot} #{index}");
        }
        if let [first, second, ..] = slots::env_vars(slot) {
            let f = fixture(&[(second, "sk-test-second"), (first, "sk-test-first")]);
            assert_eq!(
                f.vault.resolve(slot).unwrap().unwrap().secret.expose(),
                "sk-test-first",
                "{slot}: first name has precedence"
            );
        }
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
#[test]
fn clear_then_resolve_with_env_still_set_is_none_and_list_shows_cleared() {
    let f = fixture(&[("OPEN_ROUTER", ENV_SECRET)]);
    assert!(f.vault.resolve(Slot::Openrouter).unwrap().is_some());
    f.vault.clear(Slot::Openrouter).unwrap();
    assert!(f.vault.resolve(Slot::Openrouter).unwrap().is_none());

    let entry = f
        .vault
        .list()
        .credentials
        .into_iter()
        .find(|entry| entry.slot == Slot::Openrouter)
        .unwrap();
    assert_eq!(entry.state, CredentialState::Cleared);
    assert_eq!(
        entry.fingerprint.as_deref(),
        Some(&*secret::fingerprint(ENV_SECRET))
    );
    assert_eq!(entry.cleared_at, Some(f.clock.now()));

    // Durable: a reopened vault keeps the tombstone.
    let reopened = build(&f.dir, &f.settings, &f.env, &f.probe, &f.clock);
    assert!(reopened.resolve(Slot::Openrouter).unwrap().is_none());
    assert_eq!(reopened.state(Slot::Openrouter), CredentialState::Cleared);

    // Set removes the tombstone.
    reopened.set(Slot::Openrouter, VAULT_SECRET).unwrap();
    assert_eq!(reopened.state(Slot::Openrouter), CredentialState::Vault);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
#[test]
fn bedrock_without_entry_or_env_resolves_generator_and_is_never_persisted() {
    let f = fixture(&[]);
    // Force the vault file into existence with another slot.
    f.vault.set(Slot::Openai, VAULT_SECRET).unwrap();
    let resolved = f.vault.resolve(Slot::Bedrock).unwrap().unwrap();
    assert_eq!(resolved.source, CredentialSource::Generator);
    assert_eq!(resolved.secret.expose(), GENERATED);

    let on_disk = store::load(&f.dir).unwrap();
    assert!(!on_disk.entries.contains_key(&Slot::Bedrock));
    let raw = std::fs::read_to_string(f.dir.join(store::VAULT_FILE_NAME)).unwrap();
    assert!(!raw.contains(GENERATED));

    // Import never runs or persists the generator.
    let result = f.vault.import_from_env().unwrap();
    assert!(result.imported.is_empty());
    assert!(result.skipped.contains(&ImportSkipped {
        slot: Slot::Bedrock,
        reason: ImportSkipReason::AbsentFromEnv,
    }));
    assert!(
        !store::load(&f.dir)
            .unwrap()
            .entries
            .contains_key(&Slot::Bedrock)
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
#[test]
fn generic_fallback_honoured_for_harness_unless_cleared_or_compat_off() {
    let f = fixture(&[("RSI_API_KEY", "sk-test-generic")]);
    assert_eq!(
        f.vault
            .resolve_with_generic_fallback(Some(Slot::Groq))
            .unwrap()
            .env_var,
        Some("RSI_API_KEY")
    );
    // The generic fallback never satisfies plain slot resolution.
    assert!(f.vault.resolve(Slot::Groq).unwrap().is_none());
    f.vault.clear(Slot::Groq).unwrap();
    assert!(
        f.vault
            .resolve_with_generic_fallback(Some(Slot::Groq))
            .is_none()
    );
    f.settings.env_compat.store(false, Ordering::SeqCst);
    assert!(f.vault.resolve_with_generic_fallback(None).is_none());
}

// ---------------------------------------------------------------------------
// Mutations, import, persistence
// ---------------------------------------------------------------------------

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
#[test]
fn set_rotate_clear_persist_and_rotate_requires_entry() {
    let f = fixture(&[]);
    assert!(matches!(
        f.vault.rotate(Slot::Anthropic, "sk-test-new"),
        Err(VaultError::NothingToRotate(Slot::Anthropic))
    ));
    let first = f.vault.set(Slot::Anthropic, "  sk-test-first  ").unwrap();
    assert_eq!(first, secret::fingerprint("sk-test-first"));
    let second = f.vault.rotate(Slot::Anthropic, "sk-test-second").unwrap();
    let meta = f.vault.metadata(Slot::Anthropic);
    assert_eq!(meta.fingerprint.as_deref(), Some(second.as_str()));
    assert_eq!(
        meta.rotated_from_fingerprint.as_deref(),
        Some(first.as_str())
    );

    let reopened = build(&f.dir, &f.settings, &f.env, &f.probe, &f.clock);
    assert_eq!(
        reopened
            .resolve(Slot::Anthropic)
            .unwrap()
            .unwrap()
            .secret
            .expose(),
        "sk-test-second"
    );
    for bad in ["", "   ", "sk-test with space", "sk-test\u{7}"] {
        assert!(matches!(
            f.vault.set(Slot::Anthropic, bad),
            Err(VaultError::InvalidSecret(_))
        ));
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
#[test]
fn import_copies_static_env_and_skips_entries_and_tombstones() {
    let f = fixture(&[
        ("OPEN_ROUTER", "sk-test-or"),
        ("ANTHROPIC_API_KEY", "sk-test-an"),
        ("OPENAI_API_KEY", "sk-test-oa"),
        ("AWS_BEARER_TOKEN_BEDROCK", "bedrock-api-key-static-test"),
    ]);
    f.vault.set(Slot::Anthropic, VAULT_SECRET).unwrap();
    f.vault.clear(Slot::Openai).unwrap();
    let result = f.vault.import_from_env().unwrap();
    let imported: Vec<_> = result.imported.iter().map(|entry| entry.slot).collect();
    assert_eq!(imported, vec![Slot::Openrouter, Slot::Bedrock]);
    assert_eq!(result.imported[0].env_var, "OPEN_ROUTER");
    assert_eq!(
        result.imported[0].fingerprint,
        secret::fingerprint("sk-test-or")
    );
    assert!(result.skipped.contains(&ImportSkipped {
        slot: Slot::Anthropic,
        reason: ImportSkipReason::PresentInVault,
    }));
    assert!(result.skipped.contains(&ImportSkipped {
        slot: Slot::Openai,
        reason: ImportSkipReason::Cleared,
    }));
    assert_eq!(
        f.vault
            .resolve(Slot::Anthropic)
            .unwrap()
            .unwrap()
            .secret
            .expose(),
        VAULT_SECRET
    );
    assert!(f.vault.resolve(Slot::Openai).unwrap().is_none());
    // Imported entries now resolve from the vault even with env removed.
    f.env.lock().unwrap().clear();
    assert_eq!(f.vault.state(Slot::Openrouter), CredentialState::Vault);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
#[test]
fn open_refuses_looser_file_mode_with_path() {
    use std::os::unix::fs::PermissionsExt;
    let f = fixture(&[]);
    f.vault.set(Slot::Openai, VAULT_SECRET).unwrap();
    let path = f.dir.join(store::VAULT_FILE_NAME);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    let error = VaultHandleBuilder::new(Arc::clone(&f.settings))
        .dir(&f.dir)
        .open()
        .unwrap_err();
    assert!(matches!(error, VaultStoreError::LooseMode { .. }));
    assert!(error.to_string().contains(&path.display().to_string()));
}

// ---------------------------------------------------------------------------
// Metadata carries no secret
// ---------------------------------------------------------------------------

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
#[test]
fn list_metadata_has_no_secret_field_and_round_trip_carries_no_secret_bytes() {
    let f = fixture(&[("OPENAI_API_KEY", ENV_SECRET)]);
    f.vault.set(Slot::Openrouter, VAULT_SECRET).unwrap();
    f.vault
        .rotate(Slot::Openrouter, "sk-test-rotated-0004")
        .unwrap();
    f.vault.clear(Slot::Anthropic).unwrap();
    let list = f.vault.list();
    let json = serde_json::to_string(&list).unwrap();
    for secret in [ENV_SECRET, VAULT_SECRET, "sk-test-rotated-0004", GENERATED] {
        assert!(!json.contains(secret), "secret bytes leaked into list JSON");
    }
    let round_trip: ListProviderCredentialsResult = serde_json::from_str(&json).unwrap();
    assert_eq!(round_trip, list);

    // Field-set pin: every metadata key is a known secret-free field.
    let allowed = [
        "slot",
        "state",
        "fingerprint",
        "set_at",
        "rotated_from_fingerprint",
        "cleared_at",
        "check",
        "generation",
        "route",
        "cli_exposure",
        "last_cli_exposure_at",
    ];
    let value = serde_json::to_value(&list).unwrap();
    for entry in value["credentials"].as_array().unwrap() {
        for key in entry.as_object().unwrap().keys() {
            assert!(allowed.contains(&key.as_str()), "unexpected field {key}");
        }
    }

    // Positive state view.
    let by_slot: HashMap<_, _> = list
        .credentials
        .iter()
        .map(|entry| (entry.slot, entry))
        .collect();
    assert_eq!(by_slot[&Slot::Openrouter].state, CredentialState::Vault);
    assert_eq!(by_slot[&Slot::Openai].state, CredentialState::EnvCompat);
    assert_eq!(by_slot[&Slot::Anthropic].state, CredentialState::Cleared);
    assert_eq!(by_slot[&Slot::Bedrock].state, CredentialState::Generator);
    assert_eq!(by_slot[&Slot::Groq].state, CredentialState::Absent);
    assert_eq!(by_slot[&Slot::Openrouter].route, CredentialRoute::CodexCli);
    assert_eq!(by_slot[&Slot::Openrouter].cli_exposure, CliExposure::Always);
    assert_eq!(by_slot[&Slot::Anthropic].route, CredentialRoute::Harness);
    assert_eq!(list.credentials.len(), Slot::ALL.len());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
#[test]
fn health_summary_reports_missing_slots_without_secrets() {
    let f = fixture(&[("OPEN_ROUTER", ENV_SECRET)]);
    let mut summary = f.vault.health_summary();
    let openrouter = summary
        .credentials
        .iter()
        .find(|credential| credential.slot == Slot::Openrouter)
        .expect("health includes every credential slot");
    assert_eq!(openrouter.state, CredentialState::EnvCompat);
    assert_eq!(
        openrouter.env_var_names,
        vec!["OPEN_ROUTER".to_string(), "OPENROUTER_API_KEY".to_string()]
    );
    assert!(summary.missing.contains(&Slot::Openai));
    assert!(!summary.missing.contains(&Slot::Openrouter));

    f.vault.clear(Slot::Openrouter).unwrap();
    summary = f.vault.health_summary();
    let openrouter = summary
        .credentials
        .iter()
        .find(|credential| credential.slot == Slot::Openrouter)
        .expect("health includes every credential slot");
    assert_eq!(openrouter.state, CredentialState::Cleared);
    assert!(summary.missing.contains(&Slot::Openrouter));

    let json = serde_json::to_string(&summary).unwrap();
    assert!(!json.contains(ENV_SECRET));
    let parsed: rsi_common::rpc::ProviderCredentialHealthSummary =
        serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, summary);
    assert!(parsed.missing.contains(&Slot::Openai));
}

// ---------------------------------------------------------------------------
// Checks and launch admission
// ---------------------------------------------------------------------------

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
#[tokio::test]
async fn transient_check_failure_still_admits_launch() {
    for outcome in [
        http(503, ""),
        http(429, ""),
        ProbeOutcome::Timeout,
        ProbeOutcome::Connect,
    ] {
        let f = fixture_with(
            &[("OPEN_ROUTER", ENV_SECRET)],
            ScriptedProbe::new([outcome.clone()]),
        );
        assert!(
            f.vault.admit_launch(Slot::Openrouter).await.is_ok(),
            "{outcome:?}"
        );
        let check = f.vault.metadata(Slot::Openrouter).check.unwrap();
        assert_eq!(check.class, CredentialCheckClass::Unknown, "{outcome:?}");
        assert_eq!(f.probe.calls(), 1);
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
#[tokio::test]
async fn real_timeout_is_bounded_and_admits() {
    tokio::time::pause();
    let mut probe = ScriptedProbe::new([http(200, "{}")]);
    probe.delay = Some(std::time::Duration::from_secs(30));
    let f = fixture_with(&[("OPENAI_API_KEY", ENV_SECRET)], probe);
    assert!(f.vault.admit_launch(Slot::Openai).await.is_ok());
    let check = f.vault.metadata(Slot::Openai).check.unwrap();
    assert_eq!(check.class, CredentialCheckClass::Unknown);
    assert_eq!(check.detail_code, "timeout");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
#[tokio::test]
async fn transient_result_rechecks_at_most_once_per_60s() {
    let f = fixture_with(
        &[("OPEN_ROUTER", ENV_SECRET)],
        ScriptedProbe::new([http(503, ""), http(503, ""), http(503, "")]),
    );
    f.vault.admit_launch(Slot::Openrouter).await.unwrap();
    f.clock.advance(30);
    f.vault.admit_launch(Slot::Openrouter).await.unwrap();
    assert_eq!(f.probe.calls(), 1, "no recheck inside 60 s");
    f.clock.advance(31);
    f.vault.admit_launch(Slot::Openrouter).await.unwrap();
    assert_eq!(f.probe.calls(), 2, "recheck after 60 s");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
#[tokio::test]
async fn exhausted_check_refuses_launch_and_top_up_after_ttl_admits() {
    let f = fixture_with(
        &[("OPEN_ROUTER", ENV_SECRET)],
        ScriptedProbe::new([
            http(200, r#"{"data":{"limit":5,"usage":5,"limit_remaining":0}}"#),
            http(
                200,
                r#"{"data":{"limit":25,"usage":5,"limit_remaining":20}}"#,
            ),
        ]),
    );
    let refusal = f.vault.admit_launch(Slot::Openrouter).await.unwrap_err();
    assert_eq!(refusal.slot, Slot::Openrouter);
    assert_eq!(refusal.check.class, CredentialCheckClass::Exhausted);
    let error = refusal.clone().into_daemon_error();
    match &error {
        crate::error::DaemonError::StructuredRpc { data, .. } => {
            assert_eq!(data["kind"], "provider_credential");
            assert_eq!(data["class"], "exhausted");
            assert_eq!(data["slot"], "openrouter");
        }
        other => panic!("expected typed refusal, got {other:?}"),
    }
    assert!(!error.to_string().contains(ENV_SECRET));

    // Still refused inside the TTL without another probe.
    f.clock.advance(599);
    assert!(f.vault.admit_launch(Slot::Openrouter).await.is_err());
    assert_eq!(f.probe.calls(), 1);

    // After TTL the next launch rechecks; the top-up admits it.
    f.clock.advance(2);
    assert!(f.vault.admit_launch(Slot::Openrouter).await.is_ok());
    assert_eq!(f.probe.calls(), 2);
    let check = f.vault.metadata(Slot::Openrouter).check.unwrap();
    assert_eq!(check.class, CredentialCheckClass::Valid);
    assert_eq!(check.credit_remaining, Some(20.0));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
#[tokio::test]
async fn invalid_check_is_keyed_to_the_checked_credential() {
    let f = fixture_with(
        &[("ANTHROPIC_API_KEY", ENV_SECRET)],
        ScriptedProbe::new([http(401, "")]),
    );
    assert!(f.vault.admit_launch(Slot::Anthropic).await.is_err());
    // Setting a new key invalidates the check; the stale refusal no longer
    // applies to the new credential.
    f.vault.set(Slot::Anthropic, VAULT_SECRET).unwrap();
    assert!(f.vault.metadata(Slot::Anthropic).check.is_none());
    assert!(f.vault.admission(Slot::Anthropic).is_ok());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
#[tokio::test]
async fn ttl_setting_is_live() {
    let f = fixture_with(
        &[("OPEN_ROUTER", ENV_SECRET)],
        ScriptedProbe::new([http(402, ""), http(402, "")]),
    );
    f.settings.check_ttl_secs.store(10, Ordering::SeqCst);
    assert!(f.vault.admit_launch(Slot::Openrouter).await.is_err());
    f.clock.advance(11);
    assert!(f.vault.admit_launch(Slot::Openrouter).await.is_err());
    assert_eq!(f.probe.calls(), 2);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lazy_check_is_single_flight_per_slot() {
    let mut probe = ScriptedProbe::new([http(200, "{}")]);
    probe.delay = Some(std::time::Duration::from_millis(200));
    let f = fixture_with(&[("OPENAI_API_KEY", ENV_SECRET)], probe);
    let launches = (0..8).map(|_| {
        let vault = f.vault.clone();
        tokio::spawn(async move { vault.admit_launch(Slot::Openai).await })
    });
    for launch in futures::future::join_all(launches).await {
        assert!(launch.unwrap().is_ok());
    }
    assert_eq!(f.probe.calls(), 1);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
#[tokio::test]
async fn generator_and_endpointless_slots_are_never_probed() {
    let f = fixture(&[("GROQ_API_KEY", ENV_SECRET)]);
    assert!(f.vault.admit_launch(Slot::Bedrock).await.is_ok());
    assert!(f.vault.admit_launch(Slot::Groq).await.is_ok());
    assert_eq!(f.probe.calls(), 0);
    let check = f.vault.check_now(Slot::Groq).await.unwrap();
    assert_eq!(check.detail_code, "no_check_endpoint");
    assert_eq!(f.probe.calls(), 0);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
#[tokio::test]
async fn check_now_bypasses_freshness_and_persists_for_vault_entries() {
    let f = fixture_with(&[], ScriptedProbe::new([http(200, "{}"), http(401, "")]));
    f.vault.set(Slot::Openai, VAULT_SECRET).unwrap();
    assert_eq!(
        f.vault.check_now(Slot::Openai).await.unwrap().class,
        CredentialCheckClass::Valid
    );
    assert_eq!(
        f.vault.check_now(Slot::Openai).await.unwrap().class,
        CredentialCheckClass::Invalid
    );
    let reopened = build(&f.dir, &f.settings, &f.env, &f.probe, &f.clock);
    assert_eq!(
        reopened.metadata(Slot::Openai).check.unwrap().class,
        CredentialCheckClass::Invalid
    );
    assert!(reopened.admission(Slot::Openai).is_err());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
#[tokio::test]
async fn lazy_env_check_never_creates_the_vault_on_disk() {
    let f = fixture_with(
        &[("OPEN_ROUTER", ENV_SECRET)],
        ScriptedProbe::new([http(200, "{}")]),
    );
    f.vault.admit_launch(Slot::Openrouter).await.unwrap();
    assert!(!f.dir.exists());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
#[test]
fn legacy_global_is_env_only_and_never_probes() {
    let vault = global();
    assert!(vault.dir().is_none());
    assert!(vault.settings().env_compat());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
#[test]
fn runtime_config_vault_settings_round_trip_through_update_field() {
    use rsi_common::provider_credentials::{
        SETTING_VAULT_CHECK_TTL_SECS, SETTING_VAULT_ENV_COMPAT,
    };
    let runtime = crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
    let json = runtime.to_json();
    assert_eq!(json[SETTING_VAULT_ENV_COMPAT], true);
    assert_eq!(json[SETTING_VAULT_CHECK_TTL_SECS], DEFAULT_CHECK_TTL_SECS);
    assert!(crate::config::is_persisted_runtime_config_field(
        SETTING_VAULT_ENV_COMPAT
    ));
    assert!(crate::config::is_persisted_runtime_config_field(
        SETTING_VAULT_CHECK_TTL_SECS
    ));

    assert_eq!(
        runtime.update_field(SETTING_VAULT_ENV_COMPAT, &serde_json::json!(false)),
        Ok(true)
    );
    assert_eq!(
        runtime.update_field(SETTING_VAULT_CHECK_TTL_SECS, &serde_json::json!(120)),
        Ok(true)
    );
    assert!(!runtime.vault_settings.env_compat());
    assert_eq!(runtime.vault_settings.check_ttl_secs(), 120);
    assert_eq!(
        runtime.persisted_field_value(SETTING_VAULT_ENV_COMPAT),
        Some(serde_json::json!(false))
    );
    assert!(
        runtime
            .update_field(SETTING_VAULT_CHECK_TTL_SECS, &serde_json::json!(0))
            .is_err()
    );
    assert!(
        runtime
            .update_field(SETTING_VAULT_ENV_COMPAT, &serde_json::json!("yes"))
            .is_err()
    );
    assert_eq!(runtime.vault_settings.check_ttl_secs(), 120);

    // A handle built on the shared settings observes the live toggle.
    let vault = VaultHandleBuilder::new(Arc::clone(&runtime.vault_settings))
        .env(|name| (name == "OPENAI_API_KEY").then(|| ENV_SECRET.to_string()))
        .open()
        .unwrap();
    assert!(vault.resolve(Slot::Openai).unwrap().is_none());
    runtime
        .update_field(SETTING_VAULT_ENV_COMPAT, &serde_json::json!(true))
        .unwrap();
    assert!(vault.resolve(Slot::Openai).unwrap().is_some());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
#[test]
fn persisted_vault_settings_survive_daemon_restart() {
    use rsi_common::provider_credentials::{
        SETTING_VAULT_CHECK_TTL_SECS, SETTING_VAULT_ENV_COMPAT,
    };
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("rsi.db");
    let store = crate::store::Store::open(&db).unwrap();
    let runtime = crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
    runtime
        .update_field(SETTING_VAULT_ENV_COMPAT, &serde_json::json!(false))
        .unwrap();
    runtime
        .update_field(SETTING_VAULT_CHECK_TTL_SECS, &serde_json::json!(900))
        .unwrap();
    for field in [SETTING_VAULT_ENV_COMPAT, SETTING_VAULT_CHECK_TTL_SECS] {
        crate::store::daemon_settings::persist_runtime_config_field(&store, &runtime, field)
            .unwrap();
    }
    drop(store);
    let reopened = crate::store::Store::open(&db).unwrap();
    let restarted = crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
    crate::store::daemon_settings::apply_persisted_runtime_config(&reopened, &restarted).unwrap();
    assert!(!restarted.vault_settings.env_compat());
    assert_eq!(restarted.vault_settings.check_ttl_secs(), 900);
}

// ---------------------------------------------------------------------------
// rev3: generation-bound checks, source env name, CLI exposure
// ---------------------------------------------------------------------------

/// Mock probe whose first call blocks until released, so a Set/Rotate/Clear
/// can land while a check of the old key is in flight.
#[derive(Default)]
struct GatedProbe {
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
    outcomes: Mutex<std::collections::VecDeque<ProbeOutcome>>,
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl CredentialProbe for GatedProbe {
    async fn probe(&self, _slot: Slot, _secret: &SecretString) -> ProbeOutcome {
        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if call == 0 {
            self.started.notify_one();
            self.release.notified().await;
        }
        self.outcomes
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(ProbeOutcome::Failed)
    }
}

fn gated_vault(root: &Path, probe: &Arc<GatedProbe>) -> VaultHandle {
    VaultHandleBuilder::new(Arc::new(VaultSettings::default()))
        .dir(root.join("vault"))
        .env(|_| None)
        .probe(Arc::clone(probe) as Arc<dyn CredentialProbe>)
        .open()
        .unwrap()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rotate_during_inflight_check_discards_stale_result_and_admits_new_key() {
    let root = tempfile::tempdir().unwrap();
    let probe = Arc::new(GatedProbe::default());
    probe.outcomes.lock().unwrap().extend([
        // Old key: exhausted, but it arrives after the rotation.
        http(402, ""),
        // New key: valid.
        http(
            200,
            r#"{"data":{"limit":null,"usage":0,"limit_remaining":null}}"#,
        ),
    ]);
    let vault = gated_vault(root.path(), &probe);
    vault.set(Slot::Openrouter, "sk-test-old-key-0010").unwrap();
    let before = vault.metadata(Slot::Openrouter).generation;

    let inflight = {
        let vault = vault.clone();
        tokio::spawn(async move { vault.admit_launch(Slot::Openrouter).await })
    };
    probe.started.notified().await;
    let new_fp = vault
        .rotate(Slot::Openrouter, "sk-test-new-key-0011")
        .unwrap();
    assert_eq!(vault.metadata(Slot::Openrouter).generation, before + 1);
    probe.release.notify_one();
    // The in-flight launch decided against the (new) current key: the old
    // key's exhausted result was never committed, so it is admitted.
    assert!(inflight.await.unwrap().is_ok());
    assert!(
        vault.metadata(Slot::Openrouter).check.is_none(),
        "stale check of the old key must be discarded"
    );

    // The next launch checks the new key and is admitted.
    assert!(vault.admit_launch(Slot::Openrouter).await.is_ok());
    let check = vault.metadata(Slot::Openrouter).check.unwrap();
    assert_eq!(check.class, CredentialCheckClass::Valid);
    assert_eq!(check.fingerprint.as_deref(), Some(new_fp.as_str()));
    assert_eq!(check.generation, before + 1);
    assert_eq!(probe.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clear_during_inflight_check_discards_result() {
    let root = tempfile::tempdir().unwrap();
    let probe = Arc::new(GatedProbe::default());
    probe.outcomes.lock().unwrap().push_back(http(401, ""));
    let vault = gated_vault(root.path(), &probe);
    vault
        .set(Slot::Anthropic, "sk-test-clear-race-0012")
        .unwrap();
    let inflight = {
        let vault = vault.clone();
        tokio::spawn(async move { vault.check_now(Slot::Anthropic).await })
    };
    probe.started.notified().await;
    vault.clear(Slot::Anthropic).unwrap();
    probe.release.notify_one();
    assert!(inflight.await.unwrap().is_none());
    let meta = vault.metadata(Slot::Anthropic);
    assert_eq!(meta.state, CredentialState::Cleared);
    assert!(meta.check.is_none());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
#[test]
fn every_mutation_bumps_the_slot_generation() {
    let f = fixture(&[("PIONEER_API_KEY", "sk-test-pioneer-fallback-0013")]);
    let generation = |f: &Fixture| f.vault.metadata(Slot::Pioneer).generation;
    assert_eq!(generation(&f), 0);
    f.vault.import_from_env().unwrap();
    assert_eq!(generation(&f), 1);
    f.vault
        .rotate(Slot::Pioneer, "sk-test-pioneer-rotated")
        .unwrap();
    assert_eq!(generation(&f), 2);
    f.vault.clear(Slot::Pioneer).unwrap();
    assert_eq!(generation(&f), 3);
    f.vault.set(Slot::Pioneer, "sk-test-pioneer-set").unwrap();
    assert_eq!(generation(&f), 4);
    // Durable across reopen.
    let reopened = build(&f.dir, &f.settings, &f.env, &f.probe, &f.clock);
    assert_eq!(reopened.metadata(Slot::Pioneer).generation, 4);
    // Other slots are untouched.
    assert_eq!(reopened.metadata(Slot::Openrouter).generation, 0);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
#[test]
fn cli_injection_is_reported_as_exposure_without_the_secret() {
    use rsi_common::provider_credentials::{CliExposureConsumer, CliExposureReason};
    let f = fixture(&[]);
    f.vault.set(Slot::Openrouter, VAULT_SECRET).unwrap();
    let meta = f.vault.metadata(Slot::Openrouter);
    assert_eq!(meta.cli_exposure, CliExposure::Always);
    assert!(meta.last_cli_exposure_at.is_none());
    assert_eq!(
        f.vault.metadata(Slot::Anthropic).cli_exposure,
        CliExposure::None
    );

    let mut cmd = tokio::process::Command::new("codex");
    f.vault.inject_cli_credential(
        &mut cmd,
        Slot::Openrouter,
        "OPEN_ROUTER",
        &SecretString::new(VAULT_SECRET.into()),
        CliExposureConsumer::SessionCodexCli,
        CliExposureReason::Route,
    );
    env_scrub::tests::assert_only_injected(&cmd, Some("OPEN_ROUTER"));
    let meta = f.vault.metadata(Slot::Openrouter);
    assert_eq!(meta.last_cli_exposure_at, Some(f.clock.now()));
    let json = serde_json::to_string(&f.vault.list()).unwrap();
    assert!(!json.contains(VAULT_SECRET));
    assert!(json.contains("\"cli_exposure\":\"always\""));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
#[test]
fn imported_entry_keeps_its_source_env_name() {
    let f = fixture(&[("PIONEER_API_KEY", "sk-test-pioneer-fallback-0014")]);
    f.vault.import_from_env().unwrap();
    // Env removed: the value now comes from the vault, but the name it was
    // imported from is kept for the Codex `env_key` mapping. (The Pioneer
    // credential-source assertions live in `rsid`:
    // `pioneer_credential_source_follows_the_vault_import_origin`.)
    f.env.lock().unwrap().clear();
    let resolved = f.vault.resolve(Slot::Pioneer).unwrap().unwrap();
    assert_eq!(resolved.source, CredentialSource::Vault);
    assert_eq!(resolved.env_var, Some("PIONEER_API_KEY"));
}
