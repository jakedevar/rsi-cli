//! Operator-local `RemoteGetProvidersV2` (hub #3796): each provider's plan
//! windows, overage posture and 1h/24h spend for the phone's usage dashboard,
//! labels and numbers only. The bounded rate-limit SQL lives in the
//! `rsid-remote-read` leaf. Spend does not fit the Remote read's tight SQL
//! budget (it sorts the sessions it touches), so this owner serves a cached
//! copy that a rate-limited background refresh keeps warm under the normal
//! Store lock, and never runs that query on the Remote path.

use super::{
    ProviderSpendRow, ProviderSpendView, ReadError, RemoteReadCompleted, RemoteReadLimiter, Result,
    providers_response, providers_source,
};
use crate::store::{ProviderSpend, Store};
use chrono::Utc;
use parking_lot::Mutex as StateLock;
use rsi_common::remote_manager_v2::{ProvidersRequestV2, ProvidersResponseV2};
use rsi_common::rpc::RpcRequest;
use rsid_core::store_lock::Mutex;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;

/// A cached spend figure is refreshed no sooner than this after the last start.
const REFRESH_AFTER: Duration = Duration::from_secs(30);
/// A cached figure older than this is withheld as `stale`.
const MAX_AGE: Duration = Duration::from_secs(300);

/// A tokenless JSON-RPC 2.0 call whose params are exactly `{}`.
pub(super) fn parse_providers(rpc: &RpcRequest) -> Result<ProvidersRequestV2> {
    if rpc.session_token.is_some() || rpc.jsonrpc != "2.0" {
        return Err(ReadError::InvalidSource);
    }
    serde_json::from_value(rpc.params.clone()).map_err(|_| ReadError::InvalidSource)
}

#[derive(Default)]
struct CacheState {
    /// The last good read and when it finished.
    value: Option<(Instant, ProviderSpend)>,
    /// When the last refresh started, success or not.
    started: Option<Instant>,
    refreshing: bool,
    /// The latest refresh failed.
    failed: bool,
}

/// The spend the providers read serves. One per daemon; a test owns its own.
#[derive(Default)]
pub(super) struct ProviderSpendCache(StateLock<CacheState>);

impl ProviderSpendCache {
    fn view_at(&self, now: Instant) -> ProviderSpendView {
        let state = self.0.lock();
        match &state.value {
            Some((at, spend)) if now.saturating_duration_since(*at) <= MAX_AGE => {
                ProviderSpendView::Available {
                    rows: spend
                        .models
                        .iter()
                        .map(|row| ProviderSpendRow {
                            provider: row.provider.clone(),
                            model: row.model.clone(),
                            cost_1h_usd: row.cost_1h_usd,
                            cost_24h_usd: row.cost_24h_usd,
                        })
                        .collect(),
                    truncated: spend.truncated,
                }
            }
            Some(_) => ProviderSpendView::Unavailable("stale"),
            None if state.failed => ProviderSpendView::Unavailable("unavailable"),
            None => ProviderSpendView::Unavailable("warming"),
        }
    }

    /// Claim the single refresh slot when one is due.
    fn begin_at(&self, now: Instant) -> bool {
        let mut state = self.0.lock();
        let due = state
            .started
            .is_none_or(|at| now.saturating_duration_since(at) >= REFRESH_AFTER);
        if state.refreshing || !due {
            return false;
        }
        state.refreshing = true;
        state.started = Some(now);
        true
    }

    fn finish(&self, result: Option<ProviderSpend>) {
        let mut state = self.0.lock();
        state.refreshing = false;
        match result {
            Some(spend) => {
                state.value = Some((Instant::now(), spend));
                state.failed = false;
            }
            None => state.failed = true,
        }
    }

    /// Start a background refresh when one is due. It takes the Store lock
    /// like any daemon task (waiting for it, never failing a Remote read) and
    /// at most one runs at a time, at most once per [`REFRESH_AFTER`].
    pub(super) fn refresh_if_due(
        self: &Arc<Self>,
        store: Arc<Mutex<Store>>,
    ) -> Option<JoinHandle<()>> {
        if !self.begin_at(Instant::now()) {
            return None;
        }
        let cache = Arc::clone(self);
        Some(tokio::spawn(async move {
            let guard = store.lock_owned().await;
            let result = tokio::task::spawn_blocking(move || guard.provider_spend(Utc::now()).ok())
                .await
                .ok()
                .flatten();
            cache.finish(result);
        }))
    }
}

pub(super) fn spend_cache() -> &'static Arc<ProviderSpendCache> {
    static CACHE: OnceLock<Arc<ProviderSpendCache>> = OnceLock::new();
    CACHE.get_or_init(Arc::default)
}

/// Why the latest plan-window refresh of `provider` failed, if it did.
fn stale_reason(provider: &str) -> Option<String> {
    (provider == "Codex")
        .then(|| crate::codex_rate_limits::tracker().stale_reason())
        .flatten()
        .map(str::to_owned)
}

pub(super) fn spawn_providers_read(
    limiter: &RemoteReadLimiter,
    store: Arc<Mutex<Store>>,
    cache: &Arc<ProviderSpendCache>,
    _request: ProvidersRequestV2,
) -> Result<JoinHandle<RemoteReadCompleted<ProvidersResponseV2>>> {
    let served = Arc::clone(cache);
    let refresh_store = Arc::clone(&store);
    limiter.spawn_staged_store(
        store,
        |_| Ok(()),
        |conn, (), _| providers_source(conn),
        move |(), source, _| {
            let response = providers_response(
                &source,
                &served.view_at(Instant::now()),
                stale_reason,
                Utc::now(),
            );
            // Only a read that got its windows warms the cache, and only after
            // it has released the Store lock; the figure arrives for the next
            // read.
            drop(served.refresh_if_due(refresh_store));
            Ok(response)
        },
    )
}

#[cfg(all(
    test,
    unix,
    any(not(feature = "test-shard-mode"), feature = "test-shard-store-04")
))]
mod tests {
    use super::*;
    use crate::store::Store;
    use chrono::SecondsFormat;
    use rsi_common::rpc::{ProviderRateLimitSnapshot, ProviderRateLimitWindow};
    use rsi_common::types::SessionProvider;
    use serde_json::json;
    use uuid::Uuid;

    fn limiter() -> RemoteReadLimiter {
        let generous = Duration::from_secs(30);
        RemoteReadLimiter::with_deadline(generous, generous)
    }

    fn spend(models: &[(&str, &str, f64, f64)], truncated: bool) -> ProviderSpend {
        ProviderSpend {
            models: models
                .iter()
                .map(
                    |(provider, model, h1, h24)| crate::store::ProviderModelSpend {
                        provider: (*provider).into(),
                        model: (*model).into(),
                        cost_1h_usd: *h1,
                        cost_24h_usd: *h24,
                    },
                )
                .collect(),
            truncated,
        }
    }

    async fn read(store: &Arc<Mutex<Store>>, cache: &Arc<ProviderSpendCache>) -> String {
        let completed =
            spawn_providers_read(&limiter(), Arc::clone(store), cache, ProvidersRequestV2 {})
                .unwrap()
                .await
                .unwrap();
        let mut wire = Vec::new();
        completed
            .send_json_line(&mut wire, |result| result.unwrap())
            .await
            .unwrap();
        String::from_utf8(wire).unwrap()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn providers_params_are_closed_and_operator_only() {
        let method = rsi_common::remote_manager_v2::METHOD_PROVIDERS;
        assert!(crate::remote_read::is_operator_read_method(method));
        assert_eq!(
            parse_providers(&RpcRequest::new(method, json!({}))).unwrap(),
            ProvidersRequestV2 {}
        );
        assert!(parse_providers(&RpcRequest::new(method, json!({"provider": "x"}))).is_err());
        let mut tokened = RpcRequest::new(method, json!({}));
        tokened.session_token = Some("test-token".into());
        assert!(parse_providers(&tokened).is_err());
        let mut wrong_version = RpcRequest::new(method, json!({}));
        wrong_version.jsonrpc = "1.0".into();
        assert!(parse_providers(&wrong_version).is_err());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn spend_cache_warms_expires_and_refreshes_at_most_once_per_interval() {
        let cache = ProviderSpendCache::default();
        let start = Instant::now();
        assert_eq!(
            cache.view_at(start),
            ProviderSpendView::Unavailable("warming")
        );
        assert!(cache.begin_at(start));
        // One refresh at a time, and none sooner than the interval.
        assert!(!cache.begin_at(start + Duration::from_secs(1)));
        cache.finish(None);
        assert_eq!(
            cache.view_at(start),
            ProviderSpendView::Unavailable("unavailable")
        );
        assert!(!cache.begin_at(start + Duration::from_secs(5)));
        assert!(cache.begin_at(start + REFRESH_AFTER));
        cache.finish(Some(spend(&[("Claude", "opus", 1.0, 2.0)], false)));
        let finished = Instant::now();
        assert!(matches!(
            cache.view_at(finished),
            ProviderSpendView::Available {
                truncated: false,
                ..
            }
        ));
        assert_eq!(
            cache.view_at(finished + MAX_AGE + Duration::from_secs(1)),
            ProviderSpendView::Unavailable("stale")
        );
        // A failed refresh keeps serving the last good figure until it ages out.
        assert!(cache.begin_at(start + REFRESH_AFTER * 3));
        cache.finish(None);
        assert!(matches!(
            cache.view_at(finished),
            ProviderSpendView::Available { .. }
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[tokio::test]
    async fn providers_read_the_live_schema_warm_then_serve_spend_and_leak_nothing() {
        let store = Store::open_in_memory().unwrap();
        // The daemon's normal busy timeout, which the shared Remote store
        // budget requires before it lends the connection.
        store.conn.busy_timeout(Duration::from_secs(10)).unwrap();
        let now = Utc::now();
        store
            .upsert_provider_rate_limit_snapshot(
                &ProviderRateLimitSnapshot {
                    provider: SessionProvider::Claude,
                    status: Some("allowed".into()),
                    rate_limit_type: Some("five_hour".into()),
                    overage_status: Some("rejected".into()),
                    is_using_overage: false,
                    observed_at: now,
                    windows: vec![ProviderRateLimitWindow {
                        window_key: "five_hour".into(),
                        utilization: 0.42,
                        resets_at_epoch: Some(now.timestamp() + 3600),
                    }],
                    stale_reason: None,
                },
                None,
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO model_invocations
                   (id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,
                    trigger_source,provider,model,created_at,estimated_cost_usd)
                 VALUES (?1,'model.call','cli','foreground','paid','admitted','completed','test',
                         'Claude','claude-opus',?2,2.5)",
                rusqlite::params![
                    Uuid::new_v4().to_string(),
                    (now - chrono::Duration::minutes(10))
                        .to_rfc3339_opts(SecondsFormat::Nanos, true)
                ],
            )
            .unwrap();
        let store = Arc::new(Mutex::new(store));
        let cache = Arc::new(ProviderSpendCache::default());

        // First read: windows are served, spend is still warming.
        let text = read(&store, &cache).await;
        let first: ProvidersResponseV2 = serde_json::from_str(text.trim_end()).unwrap();
        let claude = &first.providers[0];
        assert_eq!(claude.provider, "Claude");
        assert_eq!(claude.windows[0].key, "five_hour");
        assert_eq!(claude.windows[0].utilization, 0.42);
        assert!(claude.windows[0].resets_at.is_some());
        assert!(claude.spend.is_none());
        assert_eq!(claude.spend_reason.as_deref(), Some("warming"));
        let codex = &first.providers[1];
        assert_eq!(codex.windows_reason.as_deref(), Some("no_observation"));

        // The refresh the first read started finishes: spend is served by the
        // next read. (The slot is taken, so no second refresh starts.)
        assert!(cache.refresh_if_due(Arc::clone(&store)).is_none());
        for _ in 0..200 {
            if matches!(
                cache.view_at(Instant::now()),
                ProviderSpendView::Available { .. }
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let text = read(&store, &cache).await;
        let second: ProvidersResponseV2 = serde_json::from_str(text.trim_end()).unwrap();
        let claude = &second.providers[0];
        let spend = claude.spend.as_ref().expect("spend after the refresh");
        assert_eq!((spend.cost_1h_usd, spend.cost_24h_usd), (2.5, 2.5));
        assert_eq!(claude.models[0].model, "claude-opus");
        assert!(claude.spend_reason.is_none());
        for secret in ["observed_session", "account", "token", "email", "/home/"] {
            assert!(!text.contains(secret), "providers leak {secret}: {text}");
        }
    }

    /// The rate-limit read is a bounded scan of one small table; the invalidation
    /// mark is a primary-key lookup per row, never a second scan.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn provider_windows_read_scans_one_table_and_looks_up_the_mark_by_key() {
        let store = Store::open_in_memory().unwrap();
        let mut statement = store
            .conn
            .prepare(&format!(
                "EXPLAIN QUERY PLAN {}",
                super::super::PROVIDER_WINDOWS_SQL
            ))
            .unwrap();
        let plan: Vec<String> = statement
            .query_map([256_i64], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            plan.iter()
                .any(|line| line.starts_with("SCAN provider_rate_limit_windows")),
            "{plan:?}"
        );
        assert!(
            plan.iter()
                .all(|line| !line.contains("SCAN daemon_settings")),
            "{plan:?}"
        );
    }
}
