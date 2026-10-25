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
    AgentGetProviderStatusResultV1, PROVIDER_AUTH_INVALID_DETAIL, PROVIDER_STATUS_CACHE_TTL_SECS,
    PROVIDER_STATUS_NAMES, ProviderAuthEpisodeV1, ProviderCredentialCheckV1, ProviderCreditV1,
    ProviderStatusEntryV1,
};
use rsi_common::provider_credentials::CredentialCheckClass;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

const API_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_API_BODY_BYTES: usize = 64 * 1024;
/// The model catalog is megabytes; the same bound as discovery's own fetch.
const MAX_CATALOG_BODY_BYTES: usize = 8 * 1024 * 1024;
const CATALOG_TIMEOUT: Duration = Duration::from_secs(10);
/// A failed account-usage read is retried no sooner than this.
const USAGE_RETRY_AFTER: Duration = Duration::from_secs(15);
/// How long a catalog fetch's prices are served before the daemon fetches the
/// catalog again for a price read.
const PRICE_TTL: chrono::Duration = chrono::Duration::hours(1);
/// A model the fetched catalog did not list is looked for again no sooner.
const PRICE_RETRY: chrono::Duration = chrono::Duration::seconds(60);

/// The last successful read of the account `total_usage` (#3154): the figure
/// the spend ceiling is judged against, with the time it was read.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpendProbe {
    pub total_usage: f64,
    pub at: DateTime<Utc>,
}

/// The two OpenRouter account endpoints, behind a trait so tests never touch
/// the network.
#[async_trait::async_trait]
pub trait OpenRouterApi: Send + Sync {
    /// `GET {base}/api/v1/key`.
    async fn key(&self, secret: &SecretString) -> ProbeOutcome;
    /// `GET {base}/api/v1/credits`.
    async fn credits(&self, secret: &SecretString) -> ProbeOutcome;
    /// `GET {base}/api/v1/models` (#3154: the catalog's per-token prices). The
    /// body limit is the catalog's, not the small account-endpoint limit. A
    /// stub that does not serve the catalog keeps this default.
    async fn models(&self, _secret: &SecretString) -> ProbeOutcome {
        ProbeOutcome::Unsupported
    }
}

/// Production client. The base URL is a field so a test can aim it at a mock.
#[derive(Clone, Debug)]
pub struct HttpOpenRouterApi {
    pub base: String,
    /// Offline daemon (#130): every read returns [`ProbeOutcome::Offline`]
    /// without a request.
    pub offline: bool,
}

impl Default for HttpOpenRouterApi {
    fn default() -> Self {
        Self {
            base: "https://openrouter.ai".into(),
            offline: crate::offline::is_offline(),
        }
    }
}

impl HttpOpenRouterApi {
    async fn get(&self, path: &str, secret: &SecretString) -> ProbeOutcome {
        self.get_limited(path, secret, MAX_API_BODY_BYTES, API_TIMEOUT)
            .await
    }

    async fn get_limited(
        &self,
        path: &str,
        secret: &SecretString,
        max_body_bytes: usize,
        timeout: Duration,
    ) -> ProbeOutcome {
        if self.offline {
            return ProbeOutcome::Offline;
        }
        let Ok(http) = reqwest::Client::builder()
            .connect_timeout(API_TIMEOUT)
            .timeout(timeout)
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
            let room = max_body_bytes.saturating_sub(body.len());
            body.extend_from_slice(&chunk[..chunk.len().min(room)]);
            if body.len() >= max_body_bytes {
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
    async fn models(&self, secret: &SecretString) -> ProbeOutcome {
        self.get_limited(
            "/api/v1/models",
            secret,
            MAX_CATALOG_BODY_BYTES,
            CATALOG_TIMEOUT,
        )
        .await
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
    /// Latest successful account-usage read (#3154), kept apart from the
    /// status cache so a failed probe never erases it: it ages instead, and an
    /// armed cap fails closed once it is stale.
    usage: parking_lot::Mutex<Option<SpendProbe>>,
    /// When the spend ceiling last tried to read the account usage, success or
    /// not: a failing provider is retried every [`USAGE_RETRY_AFTER`], not on
    /// every model call of every running session.
    usage_attempted: parking_lot::Mutex<Option<Instant>>,
    /// Single-flight guard of the catalog price fetch.
    price_fetch: tokio::sync::Mutex<()>,
    /// When this service last fetched the catalog for a price read.
    prices_fetched: parking_lot::Mutex<Option<DateTime<Utc>>>,
}

impl ProviderStatusService {
    #[must_use]
    pub fn new(api: Arc<dyn OpenRouterApi>, ttl: Duration) -> Self {
        Self {
            api,
            ttl,
            cache: tokio::sync::Mutex::new(HashMap::new()),
            usage: parking_lot::Mutex::new(None),
            usage_attempted: parking_lot::Mutex::new(None),
            price_fetch: tokio::sync::Mutex::new(()),
            prices_fetched: parking_lot::Mutex::new(None),
        }
    }

    /// The latest successful account-usage read, `None` before the first.
    #[must_use]
    pub fn usage_probe(&self) -> Option<SpendProbe> {
        *self.usage.lock()
    }

    /// Record an account-usage read. Keeps the newer of two reads.
    pub fn record_usage_probe(&self, total_usage: f64, at: DateTime<Utc>) {
        let mut usage = self.usage.lock();
        if usage.is_none_or(|current| current.at <= at) {
            *usage = Some(SpendProbe { total_usage, at });
        }
    }

    /// Test seam: overwrite the recorded usage read, however old.
    #[cfg(test)]
    pub(crate) fn set_usage_probe_for_test(&self, probe: Option<SpendProbe>) {
        *self.usage.lock() = probe;
        *self.usage_attempted.lock() = None;
    }

    /// The account usage no older than `max_age`, probing the provider when
    /// the last read is older (bypassing the 60 s status cache). `force` always
    /// probes: arming a cap takes a fresh baseline. On a failed probe the
    /// previous read is returned as it is, aging toward stale; `None` before
    /// any read succeeded.
    pub async fn refresh_usage(
        &self,
        vault: &VaultHandle,
        max_age: chrono::Duration,
        force: bool,
    ) -> Option<SpendProbe> {
        let now = Utc::now();
        if !force {
            if let Some(probe) = self.usage_probe()
                && now - probe.at < max_age
            {
                return Some(probe);
            }
            if self
                .usage_attempted
                .lock()
                .is_some_and(|at| at.elapsed() < USAGE_RETRY_AFTER)
            {
                return self.usage_probe();
            }
        }
        *self.usage_attempted.lock() = Some(Instant::now());
        self.openrouter_probe_with(vault, now, true).await;
        self.usage_probe()
    }

    /// The prices of `models` from the daemon's own OpenRouter discovery,
    /// fetching the catalog first when the last fetch is older than an hour.
    /// Returns `(prices, unknown model ids)`; a failed fetch leaves whatever an
    /// earlier fetch discovered.
    pub async fn model_prices(
        &self,
        vault: &VaultHandle,
        models: &[String],
    ) -> (
        Vec<rsi_common::openrouter_spend::OpenRouterModelPriceV1>,
        Vec<String>,
    ) {
        // This service's own last fetch decides, not the process-wide table
        // (the model picker also fills it, from a narrower query): the catalog
        // is fetched when this service never did or its copy is over an hour
        // old, and again, at most once a minute, for a model it does not list.
        let _flight = self.price_fetch.lock().await;
        let age = (*self.prices_fetched.lock()).map(|at| Utc::now() - at);
        let missing = models.iter().any(|model| {
            crate::model_control::discovered_price(
                model.strip_prefix("openrouter/").unwrap_or(model),
            )
            .is_none()
        });
        let due = match age {
            None => true,
            Some(age) => age >= PRICE_TTL || (missing && age >= PRICE_RETRY),
        };
        if due && let Ok(Some(resolved)) = vault.resolve(Slot::Openrouter) {
            *self.prices_fetched.lock() = Some(Utc::now());
            if let ProbeOutcome::Http { status, body } = self.api.models(&resolved.secret).await
                && (200..300).contains(&status)
            {
                let _ = crate::openrouter::publish_catalog_prices(&body);
            }
        }
        let mut prices = Vec::new();
        let mut unknown = Vec::new();
        for model in models {
            let id = model.strip_prefix("openrouter/").unwrap_or(model);
            match crate::model_control::discovered_price(id) {
                Some(found) => prices.push(rsi_common::openrouter_spend::OpenRouterModelPriceV1 {
                    model: model.clone(),
                    prompt_usd_per_mtok: found.price.input_per_mtok,
                    completion_usd_per_mtok: found.price.output_per_mtok,
                    input_cache_read_usd_per_mtok: found.price.cache_read_per_mtok,
                    fetched_at: found.fetched_at,
                }),
                None => unknown.push(model.clone()),
            }
        }
        (prices, unknown)
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
        self.openrouter_probe_with(vault, now, false).await
    }

    /// The OpenRouter probe; `force` skips the status cache (#3154: the spend
    /// ceiling reads the account usage itself, never a cached copy).
    async fn openrouter_probe_with(
        &self,
        vault: &VaultHandle,
        now: DateTime<Utc>,
        force: bool,
    ) -> LiveProbe {
        if !force {
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
                            if let Some(used) = used.filter(|used| used.is_finite()) {
                                self.record_usage_probe(used, now);
                            }
                            credit.account_remaining = total.zip(used).map(|(t, u)| t - u);
                        }
                    }
                }
                ProbeOutcome::Timeout | ProbeOutcome::Connect | ProbeOutcome::Failed => {
                    any_transport_failure = true;
                }
                ProbeOutcome::Unsupported | ProbeOutcome::Offline => {}
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
            provider_profile: rsi_common::provider_profile::ProviderProfile::All,
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
        // A provider that rejected its credential at startup stays refused
        // until a later launch gets past startup (#1610), a logged-in CLI
        // reading taken after the last failure ends it, or, with no such
        // reading, one probe launch is admitted after a bounded interval
        // (hub #2957; the login reading is folded in by
        // `apply_provider_login`).
        let auth_episode = launch_stats
            .auth_episode_started_at
            .zip(launch_stats.last_auth_failure_at)
            .map(|(started, last)| ProviderAuthEpisodeV1::from_failures(started, last, now));
        let (launch_admission, refusal_detail) =
            if launch_admission == "open" && auth_episode.as_ref().is_some_and(|e| e.refuses()) {
                ("refused", Some(PROVIDER_AUTH_INVALID_DETAIL.to_string()))
            } else {
                (launch_admission, refusal_detail)
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
            last_auth_failure_at: launch_stats.last_auth_failure_at,
            auth_episode,
            launches_24h: launch_stats.launches,
            failed_launches_24h: launch_stats.failed,
            failure_rate_24h,
            models_unavailable: Vec::new(),
            login: None,
            contract: None,
            spend_cap: None,
            model_prices: Vec::new(),
            model_prices_unknown: Vec::new(),
            run_budget: None,
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
                ..ProviderLaunchStats::default()
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
    async fn auth_rejection_refuses_codex_admission_until_the_episode_ends() {
        let service = ProviderStatusService::new(
            stub(http(200, "{}"), http(200, "{}")),
            Duration::from_secs(60),
        );
        let now = Utc::now();
        let failed_at = now - chrono::Duration::minutes(3);
        let mut stats = HashMap::new();
        stats.insert(
            "Codex".to_string(),
            ProviderLaunchStats {
                launches: 2,
                failed: 2,
                last_auth_failure_at: Some(failed_at),
                auth_episode_started_at: Some(failed_at),
                ..ProviderLaunchStats::default()
            },
        );
        let result = service
            .report(&vault(true), &stats, Some("codex"), now)
            .await;
        let entry = &result.providers[0];
        assert_eq!(entry.launch_admission, "refused");
        assert_eq!(entry.refusal_detail.as_deref(), Some("auth_invalid"));
        assert_eq!(entry.last_auth_failure_at, Some(failed_at));

        stats.get_mut("Codex").unwrap().auth_episode_started_at = None;
        let result = service
            .report(&vault(true), &stats, Some("codex"), now)
            .await;
        let entry = &result.providers[0];
        assert_eq!(entry.launch_admission, "open");
        assert_eq!(entry.refusal_detail, None);
        assert_eq!(entry.last_auth_failure_at, Some(failed_at));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    fn codex_stats(failed_at: DateTime<Utc>) -> HashMap<String, ProviderLaunchStats> {
        HashMap::from([(
            "Codex".to_string(),
            ProviderLaunchStats {
                launches: 1,
                failed: 1,
                last_auth_failure_at: Some(failed_at),
                auth_episode_started_at: Some(failed_at),
                ..ProviderLaunchStats::default()
            },
        )])
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    fn codex_login(
        state: rsi_common::provider_login::ProviderLoginState,
        checked_at: DateTime<Utc>,
    ) -> Vec<(String, rsi_common::provider_login::ProviderLoginV1)> {
        vec![(
            "codex".to_string(),
            rsi_common::provider_login::ProviderLoginV1 {
                state,
                checked_at: Some(checked_at),
                ..Default::default()
            },
        )]
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn a_logged_in_check_after_the_auth_failure_reopens_codex_admission() {
        use rsi_common::agent_provider_status::ProviderAuthEpisodeStateV1;
        use rsi_common::provider_login::ProviderLoginState;
        let service = ProviderStatusService::new(
            stub(http(200, "{}"), http(200, "{}")),
            Duration::from_secs(60),
        );
        let now = Utc::now();
        let failed_at = now - chrono::Duration::minutes(20);
        let stats = codex_stats(failed_at);
        let mut result = service
            .report(&vault(true), &stats, Some("codex"), now)
            .await;
        let entry = &result.providers[0];
        // No successful launch and no login reading yet: still refused, and
        // the episode names its start, last failure and next probe.
        assert_eq!(entry.launch_admission, "refused");
        let episode = entry.auth_episode.as_ref().unwrap();
        assert_eq!(episode.state, ProviderAuthEpisodeStateV1::AwaitingProbe);
        assert_eq!(
            (episode.started_at, episode.last_failure_at),
            (failed_at, failed_at)
        );
        assert_eq!(
            episode.next_probe_at,
            Some(failed_at + chrono::Duration::hours(1))
        );

        // A later logged-in check ends the refusal with no launch in between.
        result.apply_provider_login(codex_login(
            ProviderLoginState::LoggedIn,
            now - chrono::Duration::seconds(5),
        ));
        let entry = &result.providers[0];
        assert_eq!(entry.launch_admission, "open");
        assert_eq!(entry.refusal_detail, None);
        let episode = entry.auth_episode.as_ref().unwrap();
        assert_eq!(episode.state, ProviderAuthEpisodeStateV1::ClearedByLogin);
        assert_eq!(episode.last_failure_at, failed_at);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn a_still_failing_login_keeps_codex_refused() {
        use rsi_common::agent_provider_status::ProviderAuthEpisodeStateV1;
        use rsi_common::provider_login::ProviderLoginState;
        let service = ProviderStatusService::new(
            stub(http(200, "{}"), http(200, "{}")),
            Duration::from_secs(60),
        );
        let now = Utc::now();
        let failed_at = now - chrono::Duration::minutes(20);
        let stats = codex_stats(failed_at);
        // Logged out: re-login needed, with no timed probe.
        let mut result = service
            .report(&vault(true), &stats, Some("codex"), now)
            .await;
        result.apply_provider_login(codex_login(ProviderLoginState::LoggedOut, now));
        let entry = &result.providers[0];
        assert_eq!(entry.launch_admission, "refused");
        assert_eq!(entry.refusal_detail.as_deref(), Some("auth_invalid"));
        let episode = entry.auth_episode.as_ref().unwrap();
        assert_eq!(episode.state, ProviderAuthEpisodeStateV1::ReloginNeeded);
        assert_eq!(episode.next_probe_at, None);

        // A logged-in reading older than the failure proves nothing.
        let mut result = service
            .report(&vault(true), &stats, Some("codex"), now)
            .await;
        result.apply_provider_login(codex_login(
            ProviderLoginState::LoggedIn,
            failed_at - chrono::Duration::minutes(1),
        ));
        let entry = &result.providers[0];
        assert_eq!(entry.launch_admission, "refused");
        assert_eq!(
            entry.auth_episode.as_ref().unwrap().state,
            ProviderAuthEpisodeStateV1::AwaitingProbe
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn one_probe_is_admitted_after_the_bound_with_no_login_reading() {
        use rsi_common::agent_provider_status::ProviderAuthEpisodeStateV1;
        let service = ProviderStatusService::new(
            stub(http(200, "{}"), http(200, "{}")),
            Duration::from_secs(60),
        );
        let now = Utc::now();
        let failed_at = now - chrono::Duration::minutes(61);
        let result = service
            .report(&vault(true), &codex_stats(failed_at), Some("codex"), now)
            .await;
        let entry = &result.providers[0];
        assert_eq!(entry.launch_admission, "open");
        assert_eq!(entry.refusal_detail, None);
        let episode = entry.auth_episode.as_ref().unwrap();
        assert_eq!(episode.state, ProviderAuthEpisodeStateV1::ProbeAdmitted);
        assert_eq!(episode.next_probe_at, None);
        assert_eq!(entry.last_auth_failure_at, Some(failed_at));
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

    /// #130: offline, the OpenRouter key and credit reads send no request.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn an_offline_openrouter_status_read_sends_no_request() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::any())
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let api = HttpOpenRouterApi {
            base: server.uri(),
            offline: true,
        };
        let secret = SecretString::new("sk-test-offline".into());
        assert_eq!(api.key(&secret).await, ProbeOutcome::Offline);
        assert_eq!(api.credits(&secret).await, ProbeOutcome::Offline);
        server.verify().await;
    }
}
