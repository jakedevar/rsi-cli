//! The enforceable OpenRouter spend ceiling (#3154, program #3135 M4).
//!
//! The rules (baseline, stop line, warn line, fail-closed) are the pure
//! functions in `rsi_common::openrouter_spend`. This module feeds them from the
//! daemon: the operator's recorded cap and baseline (`RuntimeConfig`), the last
//! probe of the account `total_usage` (`ProviderStatusService`) and the cost of
//! the invocations settled since that probe (the store), and applies the
//! verdicts where money is spent:
//!
//! * [`SpendEnforcer::admit_launch`]: launch admission, async, with a probe
//!   refreshed at most once a minute (`freeze_launch_identity`);
//! * [`check_launch_cached`]: the structural backstop at the provider-spawn
//!   chokepoint, which continuations and rotations reach too; synchronous, so
//!   it judges the last probe and fails closed when that is stale;
//! * [`SpendGuardedControl`]: the Harness loop's check before every model call;
//! * [`SpendEnforcer::run_budget_report`]: the per-run budget the benchmark
//!   runner (M3) reads through `AgentGetProviderStatus` (`run_budget`).
//!
//! The account figure OpenRouter reports is the truth; the settled-invocation
//! sum only covers the gap between probes, and over-counts rather than
//! under-counts (see `Store::openrouter_billable_spend_settled_since`).

use crate::config::RuntimeConfig;
use crate::error::{DaemonError, Result};
use crate::model_control::call_control::{
    AdmittedModelCall, ModelCallControl, ModelCallKind, ModelCallSettlement, ModelCallUsage,
};
use crate::provider_status::{ProviderStatusService, SpendProbe};
use crate::store::Store;
use crate::vault::VaultHandle;
use chrono::{DateTime, Utc};
use rsi_common::openrouter_spend::{
    OpenRouterSpendCapV1, PROBE_REFRESH_SECS, RunBudgetReportV1, SpendInputs, SpendRefusal,
    call_verdict, evaluate, launch_verdict, run_budget_report,
};
use std::sync::Arc;
use uuid::Uuid;

/// Whether a launch bills the OpenRouter account: the OpenRouter provider on
/// any route, or a Harness session whose route is the OpenRouter endpoint. A
/// Harness launch names the endpoint by `openai_base_url`; a continuation or
/// rotation rebuilds its config without it, so there the endpoint is resolved
/// the way the Harness loop resolves it (`resolve_provider`): by the model.
/// `model` is the effective model of the launch (`None` is the Harness default).
#[must_use]
pub fn launch_bills_openrouter(
    provider: rsi_common::types::SessionProvider,
    openai_base_url: Option<&str>,
    model: Option<&str>,
) -> bool {
    use rsi_common::types::SessionProvider;
    if provider != SessionProvider::Harness || openai_base_url.is_some() {
        return rsi_common::openrouter_spend::launch_is_openrouter_billed(
            provider,
            openai_base_url,
        );
    }
    // Mirrors the base-URL-less arm order of `harness::provider::resolve_provider`:
    // Bedrock vendors, Anthropic and OpenAI model prefixes, then the
    // OpenAI-compatible table, whose `openrouter` entry is the account.
    let model = model.unwrap_or(crate::session::harness::DEFAULT_HARNESS_MODEL);
    if crate::bedrock::bedrock_vendor(model).is_some()
        || model.starts_with("claude-")
        || model.starts_with("gpt-")
        || model.starts_with("o1-")
        || model.starts_with("o3-")
        || model.starts_with("o4-")
    {
        return false;
    }
    crate::session::harness::compatible_table::lookup(model)
        .is_some_and(|entry| entry.name == "openrouter")
}

/// How often the background loop looks at an armed cap (the probe itself is
/// refreshed at most every [`PROBE_REFRESH_SECS`]).
const LOOP_TICK_SECS: u64 = 30;

fn deny(refusal: &SpendRefusal) -> DaemonError {
    DaemonError::PolicyDenied(refusal.message())
}

/// Everything the ceiling reads and writes, bundled so production and tests
/// differ only in what they pass in.
#[derive(Clone)]
pub struct SpendEnforcer<'a> {
    pub service: &'a ProviderStatusService,
    pub vault: VaultHandle,
    pub config: Arc<RuntimeConfig>,
    pub store: Arc<rsid_core::store_lock::Mutex<Store>>,
}

impl SpendEnforcer<'static> {
    /// The daemon's own: the process-wide status service and vault.
    #[must_use]
    pub fn production(
        config: Arc<RuntimeConfig>,
        store: Arc<rsid_core::store_lock::Mutex<Store>>,
    ) -> Self {
        Self {
            service: ProviderStatusService::global(),
            vault: crate::vault::global(),
            config,
            store,
        }
    }
}

impl SpendEnforcer<'_> {
    /// The live state of the cap: the last probed usage plus the invocations
    /// settled since that probe. `refresh` re-probes the account when the last
    /// read is older than a minute; without it the last read is used as it is
    /// (it may be stale, and then the state says so).
    pub async fn state(&self, refresh: bool) -> OpenRouterSpendCapV1 {
        let cfg = self.config.openrouter_spend_config();
        let now = Utc::now();
        if cfg.cap_usd <= 0.0 {
            return evaluate(&SpendInputs {
                cap_usd: 0.0,
                warn_pct: cfg.warn_pct,
                baseline: cfg.baseline,
                probe: None,
                since_probe_usd: 0.0,
                now,
            });
        }
        let probe = if refresh {
            self.service
                .refresh_usage(
                    &self.vault,
                    chrono::Duration::seconds(PROBE_REFRESH_SECS),
                    false,
                )
                .await
        } else {
            self.service.usage_probe()
        };
        let (probe, since) = match probe {
            Some(probe) => {
                let settled = self
                    .store
                    .lock()
                    .await
                    .openrouter_billable_spend_settled_since(probe.at);
                match settled {
                    Ok(since) => (Some(probe), since),
                    // The spend since the probe cannot be read: the state is
                    // unknown, so it fails closed like a missing probe.
                    Err(error) => {
                        tracing::warn!(%error, "OpenRouter spend read failed; failing closed");
                        (None, 0.0)
                    }
                }
            }
            None => (None, 0.0),
        };
        evaluate(&SpendInputs {
            cap_usd: cfg.cap_usd,
            warn_pct: cfg.warn_pct,
            baseline: cfg.baseline,
            probe: probe.map(|p| (p.total_usage, p.at)),
            since_probe_usd: since,
            now,
        })
    }

    /// Launch admission for an OpenRouter-billed launch.
    ///
    /// # Errors
    /// `PolicyDenied("openrouter_spend_cap_reached: ...")` at the line minus the
    /// per-launch reserve, `openrouter_spend_cap_probe_stale` while an armed cap
    /// cannot be judged.
    pub async fn admit_launch(&self) -> Result<()> {
        launch_verdict(&self.state(true).await).map_err(|refusal| deny(&refusal))
    }

    /// The check before one model call of a running session.
    ///
    /// # Errors
    /// As [`Self::admit_launch`], without the launch reserve.
    pub async fn admit_call(&self) -> Result<()> {
        self.call_refusal()
            .await
            .map_or(Ok(()), |refusal| Err(deny(&refusal)))
    }

    /// The refusal for one more model call, if any, with its typed code.
    pub async fn call_refusal(&self) -> Option<SpendRefusal> {
        call_verdict(&self.state(true).await).err()
    }

    /// A fresh probe of the account usage, for arming: the baseline the stop
    /// line is measured from. Never served from a cache.
    ///
    /// # Errors
    /// `openrouter_spend_cap_probe_failed` when the account usage cannot be
    /// read, so a cap is never armed without a baseline.
    pub async fn fresh_baseline(&self) -> Result<SpendProbe> {
        self.service
            .refresh_usage(&self.vault, chrono::Duration::zero(), true)
            .await
            .filter(|probe| (Utc::now() - probe.at).num_seconds() < 120)
            .ok_or_else(|| {
                DaemonError::InvalidParam(
                    "openrouter_spend_cap_probe_failed: the OpenRouter account usage could not \
                     be read, so the cap was not armed; check the OpenRouter credential and retry"
                        .into(),
                )
            })
    }

    /// The verdict for a benchmark run with `run_budget_usd` that has
    /// launched `run_sessions` so far (empty for a run about to start): the
    /// run's spend against its own budget, and what is left of that against
    /// the cap's remainder. Refused (`admitted: false`) when the run spent its
    /// budget, what is left is larger than the cap's remainder, the cap is
    /// reached or it cannot be judged. With no cap armed any positive budget
    /// is admitted. This is what `AgentGetProviderStatus` returns as
    /// `run_budget` when the request names a `run_budget_usd`.
    ///
    /// # Errors
    /// A store read failure.
    pub async fn run_budget_report(
        &self,
        run_budget_usd: f64,
        run_sessions: &[Uuid],
    ) -> Result<RunBudgetReportV1> {
        let spent = self
            .store
            .lock()
            .await
            .sessions_invocation_spend(run_sessions)?;
        let cap = self.state(true).await;
        Ok(run_budget_report(&cap, run_budget_usd, spent))
    }
}

/// The synchronous launch check at the provider-spawn chokepoint: the cap
/// judged from the last probe alone (the async admission already refreshed it
/// and counted the settled invocations). A cap that is off admits; an armed
/// cap with a stale probe refuses.
///
/// # Errors
/// As [`SpendEnforcer::admit_launch`].
pub(crate) fn check_launch_cached(
    config: &RuntimeConfig,
    service: &ProviderStatusService,
    now: DateTime<Utc>,
) -> Result<()> {
    let cfg = config.openrouter_spend_config();
    if cfg.cap_usd <= 0.0 {
        return Ok(());
    }
    let cap = evaluate(&SpendInputs {
        cap_usd: cfg.cap_usd,
        warn_pct: cfg.warn_pct,
        baseline: cfg.baseline,
        probe: service.usage_probe().map(|p| (p.total_usage, p.at)),
        since_probe_usd: 0.0,
        now,
    });
    launch_verdict(&cap).map_err(|refusal| deny(&refusal))
}

/// A model-call control that checks the spend ceiling before every call. The
/// first call of a session uses the launch's pre-admitted permit; a refusal
/// settles it (`fail_pending`) so the invocation row carries the typed class.
pub struct SpendGuardedControl {
    inner: Arc<dyn ModelCallControl>,
    enforcer: SpendEnforcer<'static>,
}

impl SpendGuardedControl {
    #[must_use]
    pub fn new(inner: Arc<dyn ModelCallControl>, enforcer: SpendEnforcer<'static>) -> Self {
        Self { inner, enforcer }
    }
}

#[async_trait::async_trait]
impl ModelCallControl for SpendGuardedControl {
    async fn admit(
        &self,
        kind: ModelCallKind,
        dedup_hint: &str,
        fingerprint_hint: &str,
        retry_of_invocation_id: Option<Uuid>,
    ) -> Result<AdmittedModelCall> {
        if let Some(refusal) = self.enforcer.call_refusal().await {
            // The invocation row carries the refusal's own class: a stale
            // probe is not a reached cap.
            let _ = self.inner.fail_pending(refusal.code).await;
            return Err(deny(&refusal));
        }
        self.inner
            .admit(kind, dedup_hint, fingerprint_hint, retry_of_invocation_id)
            .await
    }

    async fn complete(&self, call: ModelCallSettlement, usage: ModelCallUsage) -> Result<()> {
        self.inner.complete(call, usage).await
    }

    async fn fail(&self, call: ModelCallSettlement, error_class: &str) -> Result<()> {
        self.inner.fail(call, error_class).await
    }

    async fn fail_pending(&self, error_class: &str) -> Result<()> {
        self.inner.fail_pending(error_class).await
    }
}

/// Take the first spend probe at daemon start, before sessions are restored,
/// so a restored OpenRouter session is judged against a current figure instead
/// of being refused for want of one. A no-op while no cap is armed; bounded by
/// the probe's own timeouts.
pub async fn prime_probe() {
    let _ = ProviderStatusService::global()
        .refresh_usage(&crate::vault::global(), chrono::Duration::zero(), true)
        .await;
}

/// The daemon's background loop: while a cap is armed, keep the probe fresh so
/// the synchronous launch backstop and the warn notice judge a current figure.
/// Idle (one config read per tick) while no cap is armed.
pub async fn run_spend_probe_loop(
    config: Arc<RuntimeConfig>,
    store: Arc<rsid_core::store_lock::Mutex<Store>>,
) {
    let enforcer = SpendEnforcer::production(config, store);
    loop {
        if enforcer.config.openrouter_spend_config().cap_usd > 0.0 {
            let _ = enforcer.state(true).await;
        }
        tokio::time::sleep(std::time::Duration::from_secs(LOOP_TICK_SECS)).await;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::provider_status::OpenRouterApi;
    use crate::vault::check::ProbeOutcome;
    use crate::vault::secret::SecretString;
    use crate::vault::{VaultHandleBuilder, VaultSettings};
    use std::sync::atomic::{AtomicUsize, Ordering};

    const KEY: &str = "sk-or-test-spend-key-0001";

    /// A mock OpenRouter account whose `total_usage` the test moves.
    struct MockAccount {
        usage_micro: std::sync::atomic::AtomicI64,
        fail: std::sync::atomic::AtomicBool,
        credit_calls: AtomicUsize,
    }

    impl MockAccount {
        fn new(usage: f64) -> Arc<Self> {
            Arc::new(Self {
                usage_micro: std::sync::atomic::AtomicI64::new((usage * 1e6) as i64),
                fail: std::sync::atomic::AtomicBool::new(false),
                credit_calls: AtomicUsize::new(0),
            })
        }
        fn set_usage(&self, usage: f64) {
            self.usage_micro
                .store((usage * 1e6) as i64, Ordering::SeqCst);
        }
    }

    #[async_trait::async_trait]
    impl OpenRouterApi for MockAccount {
        async fn key(&self, _secret: &SecretString) -> ProbeOutcome {
            ProbeOutcome::Http {
                status: 200,
                body: br#"{"data":{"limit":1000.0,"usage":1.0,"limit_remaining":999.0}}"#.to_vec(),
            }
        }
        async fn credits(&self, _secret: &SecretString) -> ProbeOutcome {
            self.credit_calls.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                return ProbeOutcome::Timeout;
            }
            let usage = self.usage_micro.load(Ordering::SeqCst) as f64 / 1e6;
            ProbeOutcome::Http {
                status: 200,
                body: format!(r#"{{"data":{{"total_credits":615.0,"total_usage":{usage}}}}}"#)
                    .into_bytes(),
            }
        }
    }

    fn vault() -> VaultHandle {
        VaultHandleBuilder::new(Arc::new(VaultSettings::default()))
            .env(|name| (name == "OPEN_ROUTER").then(|| KEY.to_string()))
            .open()
            .unwrap()
    }

    fn enforcer(account: &Arc<MockAccount>) -> SpendEnforcer<'static> {
        // The service must outlive the enforcer; a leaked one per test is fine.
        let service: &'static ProviderStatusService = Box::leak(Box::new(
            ProviderStatusService::new(account.clone(), std::time::Duration::from_secs(60)),
        ));
        SpendEnforcer {
            service,
            vault: vault(),
            config: RuntimeConfig::from_config(&crate::config::Config::from_env()),
            store: Arc::new(rsid_core::store_lock::Mutex::new(
                Store::open_in_memory().unwrap(),
            )),
        }
    }

    async fn arm(e: &SpendEnforcer<'_>, cap: f64) {
        let probe = e.fresh_baseline().await.unwrap();
        e.config
            .arm_openrouter_spend_cap(cap, probe.total_usage, probe.at);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn arming_records_the_fresh_probe_as_the_baseline() {
        let account = MockAccount::new(514.968);
        let e = enforcer(&account);
        assert_eq!(e.state(true).await.state.as_str(), "off");
        arm(&e, 100.0).await;
        let cfg = e.config.openrouter_spend_config();
        let (baseline, armed_at) = cfg.baseline.unwrap();
        assert!((baseline - 514.968).abs() < 1e-6);
        assert!((Utc::now() - armed_at).num_seconds() < 60);
        let state = e.state(false).await;
        assert_eq!(state.state.as_str(), "ok");
        assert!((state.stop_line_usd.unwrap() - 614.968).abs() < 1e-6);
        assert!(state.spent_usd.unwrap() < 1e-6);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn a_launch_is_refused_at_the_line_minus_the_reserve() {
        let account = MockAccount::new(514.968);
        let e = enforcer(&account);
        arm(&e, 100.0).await;
        assert!(e.admit_launch().await.is_ok());
        // $99.60 spent: inside the $0.50 launch reserve.
        account.set_usage(514.968 + 99.6);
        e.service.record_usage_probe(514.968 + 99.6, Utc::now());
        let error = e.admit_launch().await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains(rsi_common::openrouter_spend::SPEND_CAP_REACHED_CODE),
            "{error}"
        );
        // A running session's next call is still admitted (no reserve) ...
        assert!(e.admit_call().await.is_ok());
        // ... until the line itself.
        account.set_usage(514.968 + 100.0);
        e.service.record_usage_probe(514.968 + 100.0, Utc::now());
        assert!(e.admit_call().await.is_err());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn the_probe_is_refreshed_at_most_once_a_minute() {
        let account = MockAccount::new(514.968);
        let e = enforcer(&account);
        arm(&e, 100.0).await;
        let after_arming = account.credit_calls.load(Ordering::SeqCst);
        for _ in 0..5 {
            e.admit_call().await.unwrap();
        }
        assert_eq!(account.credit_calls.load(Ordering::SeqCst), after_arming);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn a_stale_probe_fails_closed_for_launches_and_calls() {
        let account = MockAccount::new(514.968);
        let e = enforcer(&account);
        arm(&e, 100.0).await;
        // The last good read is 10 minutes old and every re-probe now fails.
        e.service.set_usage_probe_for_test(Some(SpendProbe {
            total_usage: 514.968,
            at: Utc::now() - chrono::Duration::minutes(10),
        }));
        account.fail.store(true, Ordering::SeqCst);
        for result in [e.admit_launch().await, e.admit_call().await] {
            let error = result.unwrap_err().to_string();
            assert!(
                error.contains(rsi_common::openrouter_spend::SPEND_CAP_PROBE_STALE_CODE),
                "{error}"
            );
        }
        // The synchronous backstop fails closed on the same stale figure.
        assert!(check_launch_cached(&e.config, e.service, Utc::now()).is_err());
        // A failing provider is retried on a short backoff, not on every call.
        let attempts = account.credit_calls.load(Ordering::SeqCst);
        for _ in 0..4 {
            assert!(e.admit_call().await.is_err());
        }
        assert_eq!(account.credit_calls.load(Ordering::SeqCst), attempts);
        // Recovery: the account answers again (the backoff elapsed).
        account.fail.store(false, Ordering::SeqCst);
        e.service.set_usage_probe_for_test(Some(SpendProbe {
            total_usage: 514.968,
            at: Utc::now() - chrono::Duration::minutes(10),
        }));
        assert!(e.admit_launch().await.is_ok());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn no_probe_ever_taken_fails_closed_when_armed() {
        let account = MockAccount::new(10.0);
        let e = enforcer(&account);
        e.config.arm_openrouter_spend_cap(5.0, 10.0, Utc::now());
        account.fail.store(true, Ordering::SeqCst);
        assert!(e.admit_launch().await.is_err());
        assert!(check_launch_cached(&e.config, e.service, Utc::now()).is_err());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn arming_refuses_without_a_baseline() {
        let account = MockAccount::new(10.0);
        account.fail.store(true, Ordering::SeqCst);
        let e = enforcer(&account);
        let error = e.fresh_baseline().await.unwrap_err().to_string();
        assert!(
            error.contains("openrouter_spend_cap_probe_failed"),
            "{error}"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn an_off_cap_admits_without_probing() {
        let account = MockAccount::new(9999.0);
        let e = enforcer(&account);
        assert!(e.admit_launch().await.is_ok());
        assert!(e.admit_call().await.is_ok());
        assert_eq!(account.credit_calls.load(Ordering::SeqCst), 0);
        assert!(check_launch_cached(&e.config, e.service, Utc::now()).is_ok());
    }

    /// Seed one settled invocation: `completed` is when it settled, `cost` its
    /// recorded `estimated_cost_usd` (a launch/resume row stores the session's
    /// running total).
    fn seed_invocation(
        store: &Store,
        id: &str,
        purpose: &str,
        provider: &str,
        session: Option<&str>,
        cost: f64,
        completed: DateTime<Utc>,
    ) {
        let completed = completed.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        store
            .conn
            .execute(
                "INSERT INTO model_invocations(id, purpose, invocation_kind, foreground,
                                               paid_risk, admission_status, status, provider,
                                               model, trigger_source, session_id,
                                               estimated_cost_usd, created_at, completed_at)
                 VALUES(?1, ?2, 'session_lifecycle', 'foreground', 'paid_capable', 'admitted',
                        'completed', ?3, 'vendor/model', 'test', ?4, ?5, ?6, ?6)",
                rusqlite::params![id, purpose, provider, session, cost, completed],
            )
            .unwrap();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn the_live_estimate_adds_the_calls_settled_since_the_probe() {
        let account = MockAccount::new(514.968);
        let e = enforcer(&account);
        arm(&e, 100.0).await;
        let probe = e.service.usage_probe().unwrap();
        {
            let store = e.store.lock().await;
            // Settled BEFORE the probe: already in the account figure.
            seed_invocation(
                &store,
                "before",
                "model.call",
                "Harness",
                None,
                50.0,
                probe.at - chrono::Duration::seconds(30),
            );
            let after = probe.at + chrono::Duration::seconds(1);
            // A session's cumulative launch row (running totals 2.0 then 5.0)
            // adds 5.0 in total, never 7.0, and a plain call adds its own 4.0.
            seed_invocation(
                &store,
                "c1",
                "session.launch.fresh",
                "Harness",
                Some("s1"),
                2.0,
                after,
            );
            seed_invocation(
                &store,
                "c2",
                "session.continue.resume",
                "Harness",
                Some("s1"),
                5.0,
                after + chrono::Duration::seconds(1),
            );
            seed_invocation(&store, "m1", "model.call", "OpenRouter", None, 4.0, after);
            // Not OpenRouter-billable: Claude CLI spend is not on this account.
            seed_invocation(&store, "other", "model.call", "Claude", None, 99.0, after);
        }
        let state = e.state(false).await;
        assert!((state.spent_usd.unwrap() - 9.0).abs() < 1e-6, "{state:?}");
        assert!((state.remaining_usd.unwrap() - 91.0).abs() < 1e-6);
        // A line crossed only by settled calls (no new probe) still stops the
        // next model call.
        {
            let store = e.store.lock().await;
            seed_invocation(
                &store,
                "big",
                "model.call",
                "Harness",
                None,
                95.0,
                probe.at + chrono::Duration::seconds(2),
            );
        }
        assert!(e.admit_call().await.is_err());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn the_session_spend_of_a_run_is_read_for_its_budget() {
        let account = MockAccount::new(1.0);
        let e = enforcer(&account);
        let now = Utc::now();
        let (a, b) = (Uuid::new_v4().to_string(), Uuid::new_v4().to_string());
        {
            let store = e.store.lock().await;
            seed_invocation(
                &store,
                "r1",
                "session.launch.fresh",
                "Harness",
                Some(&a),
                1.5,
                now,
            );
            seed_invocation(
                &store,
                "r2",
                "session.continue.resume",
                "Harness",
                Some(&a),
                2.0,
                now + chrono::Duration::seconds(1),
            );
            seed_invocation(
                &store,
                "r3",
                "session.launch.fresh",
                "Harness",
                Some(&b),
                7.0,
                now,
            );
        }
        let run = [Uuid::parse_str(&a).unwrap()];
        let report = e.run_budget_report(2.0, &run).await.unwrap();
        assert!((report.status.spent_usd - 2.0).abs() < 1e-9, "{report:?}");
        assert!(
            report.status.exhausted && !report.admitted,
            "the run reached its budget: no more tasks"
        );
        assert_eq!(
            report.refusal_code.as_deref(),
            Some(rsi_common::openrouter_spend::RUN_BUDGET_EXHAUSTED_CODE)
        );
        let report = e.run_budget_report(3.0, &run).await.unwrap();
        assert!(report.admitted && !report.status.exhausted);
        assert!((report.status.remaining_usd - 1.0).abs() < 1e-9);
        let none = e.run_budget_report(3.0, &[]).await.unwrap();
        assert_eq!(none.status.spent_usd, 0.0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn a_session_is_stopped_at_its_next_model_call_once_the_line_is_crossed() {
        use crate::model_control::call_control::NoopModelCallControl;
        let account = MockAccount::new(514.968);
        let e = enforcer(&account);
        arm(&e, 100.0).await;
        let guarded = SpendGuardedControl::new(Arc::new(NoopModelCallControl), e.clone());
        for turn in 0..3 {
            guarded
                .admit(ModelCallKind::Primary, &format!("turn:{turn}"), "fp", None)
                .await
                .unwrap_or_else(|error| panic!("turn {turn} is under the line: {error}"));
        }
        // Mid-session the account crosses the stop line.
        e.service.record_usage_probe(514.968 + 100.01, Utc::now());
        let error = match guarded
            .admit(ModelCallKind::Primary, "turn:3", "fp", None)
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("the call past the line was admitted"),
        };
        let text = error.to_string();
        assert!(
            text.contains(rsi_common::openrouter_spend::SPEND_CAP_REACHED_CODE),
            "{text}"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn run_budget_is_refused_above_the_remainder() {
        let account = MockAccount::new(514.968);
        let e = enforcer(&account);
        arm(&e, 100.0).await;
        account.set_usage(514.968 + 90.0);
        e.service.record_usage_probe(514.968 + 90.0, Utc::now());
        assert!(e.run_budget_report(8.0, &[]).await.unwrap().admitted);
        let refused = e.run_budget_report(11.0, &[]).await.unwrap();
        assert!(!refused.admitted);
        assert_eq!(
            refused.refusal_code.as_deref(),
            Some(rsi_common::openrouter_spend::RUN_BUDGET_EXCEEDS_REMAINDER_CODE)
        );
    }

    /// Records the class each refused call settled its pending invocation with.
    struct RecordingControl {
        failed: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl ModelCallControl for RecordingControl {
        async fn admit(
            &self,
            kind: ModelCallKind,
            dedup_hint: &str,
            fingerprint_hint: &str,
            retry_of_invocation_id: Option<Uuid>,
        ) -> Result<AdmittedModelCall> {
            crate::model_control::call_control::NoopModelCallControl
                .admit(kind, dedup_hint, fingerprint_hint, retry_of_invocation_id)
                .await
        }
        async fn complete(&self, _call: ModelCallSettlement, _usage: ModelCallUsage) -> Result<()> {
            Ok(())
        }
        async fn fail(&self, _call: ModelCallSettlement, _error_class: &str) -> Result<()> {
            Ok(())
        }
        async fn fail_pending(&self, error_class: &str) -> Result<()> {
            self.failed.lock().unwrap().push(error_class.to_string());
            Ok(())
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn a_refused_call_settles_with_its_own_class() {
        let account = MockAccount::new(514.968);
        let e = enforcer(&account);
        arm(&e, 100.0).await;
        let recording = Arc::new(RecordingControl {
            failed: std::sync::Mutex::new(Vec::new()),
        });
        let guarded = SpendGuardedControl::new(recording.clone(), e.clone());
        // A stale probe is not a reached cap.
        e.service.set_usage_probe_for_test(Some(SpendProbe {
            total_usage: 514.968,
            at: Utc::now() - chrono::Duration::minutes(10),
        }));
        account.fail.store(true, Ordering::SeqCst);
        assert!(
            guarded
                .admit(ModelCallKind::Primary, "turn:0", "fp", None)
                .await
                .is_err()
        );
        // A crossed line is.
        account.fail.store(false, Ordering::SeqCst);
        e.service.record_usage_probe(514.968 + 100.01, Utc::now());
        assert!(
            guarded
                .admit(ModelCallKind::Primary, "turn:1", "fp", None)
                .await
                .is_err()
        );
        assert_eq!(
            *recording.failed.lock().unwrap(),
            vec![
                rsi_common::openrouter_spend::SPEND_CAP_PROBE_STALE_CODE.to_string(),
                rsi_common::openrouter_spend::SPEND_CAP_REACHED_CODE.to_string(),
            ]
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn a_harness_launch_without_an_endpoint_is_billed_by_its_model_route() {
        use rsi_common::types::SessionProvider::{Claude, Harness, OpenRouter};
        let openrouter_url = Some("https://openrouter.ai/api/v1");
        assert!(launch_bills_openrouter(OpenRouter, None, None));
        assert!(launch_bills_openrouter(
            Harness,
            openrouter_url,
            Some("x/y")
        ));
        assert!(!launch_bills_openrouter(
            Harness,
            Some("https://api.example.test/v1"),
            Some("openrouter/x")
        ));
        // A continuation carries no endpoint: the model resolves the route the
        // way the Harness loop does.
        assert!(launch_bills_openrouter(
            Harness,
            None,
            Some("openrouter/vendor-model")
        ));
        // The compatible table's first match wins, as in `resolve_provider`:
        // a name an earlier entry contains routes there, not to OpenRouter.
        assert!(!launch_bills_openrouter(
            Harness,
            None,
            Some("openrouter/deepseek/deepseek-v4.1-flash")
        ));
        assert!(!launch_bills_openrouter(
            Harness,
            None,
            Some("claude-sonnet-5")
        ));
        assert!(!launch_bills_openrouter(Harness, None, Some("gpt-5")));
        assert!(!launch_bills_openrouter(Harness, None, None));
        assert!(!launch_bills_openrouter(Claude, None, Some("openrouter/x")));
    }
}
