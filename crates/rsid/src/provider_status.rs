//! `AgentGetProviderStatus` (#1044): assemble secret-free provider health.
//!
//! The daemon calls provider APIs with its own credentials and returns only
//! numbers, timestamps and stable codes. The remote portion is cached per
//! provider for [`PROVIDER_STATUS_CACHE_TTL_SECS`] so agents cannot hammer a
//! provider API; the response is bounded by the closed provider name list.

use crate::store::provider_status::ProviderLaunchStats;
use crate::vault::VaultHandle;
use crate::vault::check::ProbeOutcome;
use crate::vault::secret::SecretString;
use crate::vault::slots::Slot;
use chrono::{DateTime, Utc};
use rsi_common::agent_provider_status::{
    AgentGetProviderStatusResultV1, PROVIDER_STATUS_CACHE_TTL_SECS, PROVIDER_STATUS_NAMES,
    ProviderCredentialCheckV1, ProviderCreditV1, ProviderStatusEntryV1,
};
use rsi_common::provider_credentials::CredentialCheckClass;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

const API_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_API_BODY_BYTES: usize = 64 * 1024;

/// The two OpenRouter account endpoints, behind a trait so tests never touch
/// the network.
#[async_trait::async_trait]
pub trait OpenRouterApi: Send + Sync {
    /// `GET {base}/api/v1/key`.
    async fn key(&self, secret: &SecretString) -> ProbeOutcome;
    /// `GET {base}/api/v1/credits`.
    async fn credits(&self, secret: &SecretString) -> ProbeOutcome;
}

/// Production client. The base URL is a field so a test can aim it at a mock.
#[derive(Clone, Debug)]
pub struct HttpOpenRouterApi {
    pub base: String,
}

impl Default for HttpOpenRouterApi {
    fn default() -> Self {
        Self {
            base: "https://openrouter.ai".into(),
        }
    }
}

impl HttpOpenRouterApi {
    async fn get(&self, path: &str, secret: &SecretString) -> ProbeOutcome {
        let Ok(http) = reqwest::Client::builder()
            .connect_timeout(API_TIMEOUT)
            .timeout(API_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
        else {
            return ProbeOutcome::Failed;
        };
        let response = match http
            .get(format!("{}{path}", self.base))
            .bearer_auth(secret.expose())
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) if error.is_timeout() => return ProbeOutcome::Timeout,
            Err(error) if error.is_connect() => return ProbeOutcome::Connect,
            Err(_) => return ProbeOutcome::Failed,
        };
        let status = response.status().as_u16();
        let mut body = Vec::new();
        let mut stream = futures::StreamExt::fuse(response.bytes_stream());
        while let Some(chunk) = futures::StreamExt::next(&mut stream).await {
            let Ok(chunk) = chunk else {
                return ProbeOutcome::Failed;
            };
            let room = MAX_API_BODY_BYTES.saturating_sub(body.len());
            body.extend_from_slice(&chunk[..chunk.len().min(room)]);
            if body.len() >= MAX_API_BODY_BYTES {
                break;
            }
        }
        ProbeOutcome::Http { status, body }
    }
}

#[async_trait::async_trait]
impl OpenRouterApi for HttpOpenRouterApi {
    async fn key(&self, secret: &SecretString) -> ProbeOutcome {
        self.get("/api/v1/key", secret).await
    }
    async fn credits(&self, secret: &SecretString) -> ProbeOutcome {
        self.get("/api/v1/credits", secret).await
    }
}

fn data_f64(body: &[u8], field: &str) -> Option<f64> {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()?
        .pointer(&format!("/data/{field}"))?
        .as_f64()
}

/// Parse `/api/v1/key`: `(limit, usage, limit_remaining)`.
#[must_use]
pub fn parse_openrouter_key(body: &[u8]) -> (Option<f64>, Option<f64>, Option<f64>) {
    (
        data_f64(body, "limit"),
        data_f64(body, "usage"),
        data_f64(body, "limit_remaining"),
    )
}

/// Parse `/api/v1/credits`: `(total_credits, total_usage)`.
#[must_use]
pub fn parse_openrouter_credits(body: &[u8]) -> (Option<f64>, Option<f64>) {
    (
        data_f64(body, "total_credits"),
        data_f64(body, "total_usage"),
    )
}

/// The cached, network-derived portion of one provider's status.
#[derive(Clone, Debug, Default)]
struct LiveProbe {
    credit: Option<ProviderCreditV1>,
    reachable: Option<bool>,
    /// A probe request itself answered 402 (credit gone, key still known).
    saw_402_at: Option<DateTime<Utc>>,
}

pub struct ProviderStatusService {
    api: Arc<dyn OpenRouterApi>,
    ttl: Duration,
    cache: tokio::sync::Mutex<HashMap<&'static str, (Instant, LiveProbe)>>,
}

impl ProviderStatusService {
    #[must_use]
    pub fn new(api: Arc<dyn OpenRouterApi>, ttl: Duration) -> Self {
        Self {
            api,
            ttl,
            cache: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Process-wide production instance (real HTTP, 60 s TTL).
    pub fn global() -> &'static Self {
        static SERVICE: OnceLock<ProviderStatusService> = OnceLock::new();
        SERVICE.get_or_init(|| {
            Self::new(
                Arc::new(HttpOpenRouterApi::default()),
                Duration::from_secs(PROVIDER_STATUS_CACHE_TTL_SECS),
            )
        })
    }

    async fn openrouter_probe(&self, vault: &VaultHandle, now: DateTime<Utc>) -> LiveProbe {
        {
            let cache = self.cache.lock().await;
            if let Some((at, probe)) = cache.get("openrouter")
                && at.elapsed() < self.ttl
            {
                return probe.clone();
            }
        }
        let Ok(Some(resolved)) = vault.resolve(Slot::Openrouter) else {
            return LiveProbe::default();
        };
        let (key, credits) = tokio::join!(
            self.api.key(&resolved.secret),
            self.api.credits(&resolved.secret)
        );
        let mut probe = LiveProbe::default();
        let mut credit = ProviderCreditV1 {
            checked_at: now,
            ..ProviderCreditV1::default()
        };
        let mut any_http = false;
        let mut any_transport_failure = false;
        for (outcome, is_key) in [(&key, true), (&credits, false)] {
            match outcome {
                ProbeOutcome::Http { status, body } => {
                    any_http = true;
                    if *status == 402 {
                        probe.saw_402_at = Some(now);
                    } else if (200..300).contains(status) {
                        if is_key {
                            let (limit, usage, remaining) = parse_openrouter_key(body);
                            credit.key_limit = limit;
                            credit.key_usage = usage;
                            credit.key_limit_remaining = remaining;
                        } else {
                            let (total, used) = parse_openrouter_credits(body);
                            credit.total_credits = total;
                            credit.total_usage = used;
                            credit.account_remaining = total.zip(used).map(|(t, u)| t - u);
                        }
                    }
                }
                ProbeOutcome::Timeout | ProbeOutcome::Connect | ProbeOutcome::Failed => {
                    any_transport_failure = true;
                }
                ProbeOutcome::Unsupported => {}
            }
        }
        probe.reachable = if any_http {
            Some(true)
        } else if any_transport_failure {
            Some(false)
        } else {
            None
        };
        if credit.key_limit.is_some()
            || credit.key_usage.is_some()
            || credit.key_limit_remaining.is_some()
            || credit.total_credits.is_some()
        {
            probe.credit = Some(credit);
        }
        self.cache
            .lock()
            .await
            .insert("openrouter", (Instant::now(), probe.clone()));
        probe
    }

    /// Build the status for `filter` (or every provider). `stats` is keyed by
    /// the serde `SessionProvider` string.
    pub async fn report(
        &self,
        vault: &VaultHandle,
        stats: &HashMap<String, ProviderLaunchStats>,
        filter: Option<&str>,
        now: DateTime<Utc>,
    ) -> AgentGetProviderStatusResultV1 {
        let mut providers = Vec::new();
        for name in PROVIDER_STATUS_NAMES {
            if filter.is_some_and(|wanted| wanted != name) {
                continue;
            }
            providers.push(self.entry(vault, stats, name, now).await);
        }
        AgentGetProviderStatusResultV1 {
            generated_at: now,
            cache_ttl_secs: self.ttl.as_secs(),
            providers,
        }
    }

    async fn entry(
        &self,
        vault: &VaultHandle,
        stats: &HashMap<String, ProviderLaunchStats>,
        name: &'static str,
        now: DateTime<Utc>,
    ) -> ProviderStatusEntryV1 {
        let slot = slot_for_name(name);
        let launch_stats = session_provider_key(name)
            .and_then(|key| stats.get(key))
            .cloned()
            .unwrap_or_default();
        let live = match slot {
            Some(Slot::Openrouter) => self.openrouter_probe(vault, now).await,
            // Free `GET /v1/models` checks with their own TTL. Bedrock's
            // check is a paid one-token call, so it is never triggered here.
            Some(Slot::Anthropic | Slot::Openai) => {
                if let Some(slot) = slot {
                    vault.ensure_fresh_check(slot).await;
                }
                LiveProbe::default()
            }
            _ => LiveProbe::default(),
        };
        let metadata = slot.map(|slot| vault.metadata(slot));
        let check = metadata.as_ref().and_then(|meta| meta.check.clone());
        let configured = slot.map(|slot| vault.resolvable(slot));
        let (launch_admission, refusal_detail) = match (slot, configured) {
            (Some(_), Some(false)) => ("refused", Some("credential_not_configured".to_string())),
            (Some(slot), _) => match vault.admission(slot) {
                Ok(()) => ("open", None),
                Err(refusal) => (
                    "refused",
                    Some(format!(
                        "{}:{}",
                        match refusal.check.class {
                            CredentialCheckClass::Invalid => "invalid",
                            CredentialCheckClass::Exhausted => "exhausted",
                            CredentialCheckClass::Valid => "valid",
                            CredentialCheckClass::Unknown => "unknown",
                        },
                        refusal.check.detail_code
                    )),
                ),
            },
            (None, _) => ("open", None),
        };
        let reachable = live.reachable.or_else(|| {
            check
                .as_ref()
                .and_then(|check| match check.detail_code.as_str() {
                    "timeout" | "connect_error" | "request_failed" => Some(false),
                    "no_check_endpoint" => None,
                    _ => Some(true),
                })
        });
        let max_time = |a: Option<DateTime<Utc>>, b: Option<DateTime<Utc>>| match (a, b) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        let exhausted_check_at = check
            .as_ref()
            .filter(|check| check.class == CredentialCheckClass::Exhausted)
            .map(|check| check.at);
        let last_402_at = max_time(
            max_time(launch_stats.last_credit_error_at, live.saw_402_at),
            exhausted_check_at,
        );
        let failure_rate_24h = (launch_stats.launches > 0)
            .then(|| f64::from(launch_stats.failed) / f64::from(launch_stats.launches));
        ProviderStatusEntryV1 {
            provider: name.to_string(),
            configured,
            credential_slot: slot.map(|slot| slot.to_string()),
            reachable,
            launch_admission: launch_admission.to_string(),
            refusal_detail,
            credit: live.credit,
            credential_check: check.map(|check| ProviderCredentialCheckV1 {
                class: check.class,
                detail_code: check.detail_code,
                http_status: check.http_status,
                checked_at: check.at,
            }),
            last_402_at,
            last_429_at: launch_stats.last_rate_limit_at,
            launches_24h: launch_stats.launches,
            failed_launches_24h: launch_stats.failed,
            failure_rate_24h,
        }
    }
}

fn slot_for_name(name: &str) -> Option<Slot> {
    match name {
        "openrouter" => Some(Slot::Openrouter),
        "bedrock" => Some(Slot::Bedrock),
        "pioneer" => Some(Slot::Pioneer),
        "anthropic" => Some(Slot::Anthropic),
        "openai" => Some(Slot::Openai),
        _ => None,
    }
}

/// The serde `SessionProvider` string behind a session-provider name.
fn session_provider_key(name: &str) -> Option<&'static str> {
    Some(match name {
        "claude" => "Claude",
        "codex" => "Codex",
        "pioneer" => "Pioneer",
        "openrouter" => "OpenRouter",
        "bedrock" => "Bedrock",
        "local" => "Local",
        "antigravity" => "Antigravity",
        "codex_app_server" => "CodexAppServer",
        "harness" => "Harness",
        _ => return None,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::vault::{VaultHandleBuilder, VaultSettings};
    use std::sync::atomic::{AtomicUsize, Ordering};

    const FAKE_KEY: &str = "sk-or-test-status-key-0001";

    struct StubApi {
        key: ProbeOutcome,
        credits: ProbeOutcome,
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl OpenRouterApi for StubApi {
        async fn key(&self, secret: &SecretString) -> ProbeOutcome {
            assert_eq!(secret.expose(), FAKE_KEY);
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.key.clone()
        }
        async fn credits(&self, _secret: &SecretString) -> ProbeOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.credits.clone()
        }
    }

    fn http(status: u16, body: &str) -> ProbeOutcome {
        ProbeOutcome::Http {
            status,
            body: body.as_bytes().to_vec(),
        }
    }

    fn vault(with_key: bool) -> VaultHandle {
        VaultHandleBuilder::new(Arc::new(VaultSettings::default()))
            .env(move |name| (with_key && name == "OPEN_ROUTER").then(|| FAKE_KEY.to_string()))
            .open()
            .unwrap()
    }

    fn stub(key: ProbeOutcome, credits: ProbeOutcome) -> Arc<StubApi> {
        Arc::new(StubApi {
            key,
            credits,
            calls: AtomicUsize::new(0),
        })
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn parsers_read_openrouter_key_and_credit_bodies() {
        let key = br#"{"data":{"limit":10.0,"usage":2.5,"limit_remaining":7.5}}"#;
        assert_eq!(
            parse_openrouter_key(key),
            (Some(10.0), Some(2.5), Some(7.5))
        );
        let unlimited = br#"{"data":{"limit":null,"usage":1.0,"limit_remaining":null}}"#;
        assert_eq!(parse_openrouter_key(unlimited), (None, Some(1.0), None));
        let credits = br#"{"data":{"total_credits":50.0,"total_usage":12.25}}"#;
        assert_eq!(parse_openrouter_credits(credits), (Some(50.0), Some(12.25)));
        assert_eq!(parse_openrouter_credits(b"not json"), (None, None));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn openrouter_reports_credit_and_never_the_key() {
        let api = stub(
            http(
                200,
                r#"{"data":{"limit":10.0,"usage":2.5,"limit_remaining":7.5}}"#,
            ),
            http(
                200,
                r#"{"data":{"total_credits":50.0,"total_usage":12.25}}"#,
            ),
        );
        let service = ProviderStatusService::new(api.clone(), Duration::from_secs(60));
        let now = Utc::now();
        let mut stats = HashMap::new();
        stats.insert(
            "OpenRouter".to_string(),
            ProviderLaunchStats {
                launches: 4,
                failed: 1,
                last_credit_error_at: Some(now - chrono::Duration::hours(3)),
                last_rate_limit_at: Some(now - chrono::Duration::hours(1)),
            },
        );
        let result = service
            .report(&vault(true), &stats, Some("openrouter"), now)
            .await;
        assert_eq!(result.providers.len(), 1);
        let entry = &result.providers[0];
        assert_eq!(entry.configured, Some(true));
        assert_eq!(entry.reachable, Some(true));
        assert_eq!(entry.launch_admission, "open");
        let credit = entry.credit.as_ref().unwrap();
        assert_eq!(credit.key_limit_remaining, Some(7.5));
        assert_eq!(credit.account_remaining, Some(50.0 - 12.25));
        assert_eq!(entry.last_402_at, stats["OpenRouter"].last_credit_error_at);
        assert_eq!(entry.last_429_at, stats["OpenRouter"].last_rate_limit_at);
        assert_eq!((entry.launches_24h, entry.failed_launches_24h), (4, 1));
        assert_eq!(entry.failure_rate_24h, Some(0.25));
        let json = serde_json::to_string(&result).unwrap();
        assert!(
            !json.contains(FAKE_KEY),
            "the response must never carry a key"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn remote_portion_is_cached_within_ttl_and_refetched_after() {
        let api = stub(
            http(
                200,
                r#"{"data":{"limit":null,"usage":0.0,"limit_remaining":null}}"#,
            ),
            http(200, r#"{"data":{"total_credits":5.0,"total_usage":1.0}}"#),
        );
        let vault = vault(true);
        let stats = HashMap::new();
        let cached = ProviderStatusService::new(api.clone(), Duration::from_secs(3600));
        for _ in 0..3 {
            cached
                .report(&vault, &stats, Some("openrouter"), Utc::now())
                .await;
        }
        assert_eq!(
            api.calls.load(Ordering::SeqCst),
            2,
            "one key + one credits call"
        );
        let uncached = ProviderStatusService::new(api.clone(), Duration::ZERO);
        uncached
            .report(&vault, &stats, Some("openrouter"), Utc::now())
            .await;
        uncached
            .report(&vault, &stats, Some("openrouter"), Utc::now())
            .await;
        assert_eq!(api.calls.load(Ordering::SeqCst), 2 + 4);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn payment_required_probe_sets_last_402_and_unreachable_is_reported() {
        let now = Utc::now();
        let exhausted = ProviderStatusService::new(
            stub(http(402, "{}"), http(402, "{}")),
            Duration::from_secs(60),
        );
        let result = exhausted
            .report(&vault(true), &HashMap::new(), Some("openrouter"), now)
            .await;
        let entry = &result.providers[0];
        assert_eq!(entry.last_402_at, Some(now));
        assert_eq!(entry.reachable, Some(true));
        assert!(entry.credit.is_none());

        let down = ProviderStatusService::new(
            stub(ProbeOutcome::Timeout, ProbeOutcome::Connect),
            Duration::from_secs(60),
        );
        let result = down
            .report(&vault(true), &HashMap::new(), Some("openrouter"), now)
            .await;
        assert_eq!(result.providers[0].reachable, Some(false));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn unconfigured_provider_makes_no_remote_call_and_refuses_launch() {
        let api = stub(http(200, "{}"), http(200, "{}"));
        let service = ProviderStatusService::new(api.clone(), Duration::from_secs(60));
        let result = service
            .report(
                &vault(false),
                &HashMap::new(),
                Some("openrouter"),
                Utc::now(),
            )
            .await;
        let entry = &result.providers[0];
        assert_eq!(entry.configured, Some(false));
        assert_eq!(entry.launch_admission, "refused");
        assert_eq!(
            entry.refusal_detail.as_deref(),
            Some("credential_not_configured")
        );
        assert_eq!(api.calls.load(Ordering::SeqCst), 0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn exhausted_credential_reports_a_refused_launch() {
        let vault = vault(true);
        let fingerprint = vault
            .resolve(Slot::Openrouter)
            .unwrap()
            .unwrap()
            .secret
            .fingerprint();
        assert!(vault.mark_exhausted(Slot::Openrouter, &fingerprint, 402));
        let service = ProviderStatusService::new(
            stub(http(200, "{}"), http(200, "{}")),
            Duration::from_secs(60),
        );
        let result = service
            .report(&vault, &HashMap::new(), Some("openrouter"), Utc::now())
            .await;
        let entry = &result.providers[0];
        assert_eq!(entry.launch_admission, "refused");
        assert_eq!(
            entry.refusal_detail.as_deref(),
            Some("exhausted:live_credit_exhausted")
        );
        assert!(entry.last_402_at.is_some());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn unfiltered_report_is_bounded_and_slotless_providers_report_no_credential() {
        let service = ProviderStatusService::new(
            stub(http(200, "{}"), http(200, "{}")),
            Duration::from_secs(60),
        );
        let result = service
            .report(&vault(false), &HashMap::new(), None, Utc::now())
            .await;
        assert_eq!(result.providers.len(), PROVIDER_STATUS_NAMES.len());
        let claude = result
            .providers
            .iter()
            .find(|p| p.provider == "claude")
            .unwrap();
        assert_eq!(claude.configured, None);
        assert_eq!(claude.launch_admission, "open");
    }
}
