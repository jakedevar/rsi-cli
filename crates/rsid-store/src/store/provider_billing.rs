//! Read-time billing classification. Recorded estimates are never rewritten.
use super::Store;
use crate::error::Result;
use rsi_common::provider_billing::BillingMode;
use rsi_common::types::Session;

/// The scalar view and the aggregate use exactly the same billing policy.
pub(crate) fn resolve_settings(
    provider: &str,
    model: Option<&str>,
    settings: &std::collections::BTreeMap<String, String>,
) -> BillingMode {
    let name = provider.to_ascii_lowercase();
    let login_name = if name == "codexappserver" {
        "codex"
    } else {
        &name
    };
    rsi_common::provider_billing::resolve(
        provider,
        model,
        settings
            .get(&format!("{name}_billing_mode"))
            .map(String::as_str),
        settings
            .get(&format!("billing_login.{login_name}"))
            .map(String::as_str),
    )
}

/// Fold the canonical invocation increments in one SQL aggregate step per row.
/// Returning one compact tuple avoids re-evaluating billing for each SUM and
/// keeps the manager's existing per-member VM instruction ceiling (#2494).
pub(crate) struct BilledUsage;
#[derive(Default)]
pub(crate) struct BilledUsageState {
    count: i64,
    cost: f64,
    missing: i64,
    subscriptions: i64,
    settings: std::collections::BTreeMap<String, String>,
}
impl rusqlite::functions::Aggregate<BilledUsageState, String> for BilledUsage {
    fn init(
        &self,
        _ctx: &mut rusqlite::functions::Context<'_>,
    ) -> rusqlite::Result<BilledUsageState> {
        Ok(BilledUsageState::default())
    }
    fn step(
        &self,
        ctx: &mut rusqlite::functions::Context<'_>,
        state: &mut BilledUsageState,
    ) -> rusqlite::Result<()> {
        if state.count == 0 {
            let settings: String = ctx.get(3)?;
            state.settings = serde_json::from_str(&settings)
                .map_err(|e| rusqlite::Error::UserFunctionError(Box::new(e)))?;
        }
        state.count += 1;
        let provider: Option<String> = ctx.get(0)?;
        let model: Option<String> = ctx.get(1)?;
        if !resolve_settings(
            provider.as_deref().unwrap_or("Unknown"),
            model.as_deref(),
            &state.settings,
        )
        .is_metered()
        {
            state.subscriptions += 1;
        } else if let Some(cost) = ctx.get::<Option<f64>>(2)? {
            if cost.is_finite() && cost >= 0.0 && (state.cost + cost).is_finite() {
                state.cost += cost;
            } else {
                state.missing += 1;
            }
        } else {
            state.missing += 1;
        }
        Ok(())
    }
    fn finalize(
        &self,
        _ctx: &mut rusqlite::functions::Context<'_>,
        state: Option<BilledUsageState>,
    ) -> rusqlite::Result<String> {
        let state = state.unwrap_or_default();
        Ok(
            serde_json::json!([state.count, state.cost, state.missing, state.subscriptions])
                .to_string(),
        )
    }
}

impl Store {
    /// Record only the CLI's sanitized, closed login method. Key construction
    /// stays behind the store boundary; no credential material is accepted.
    pub fn record_provider_billing_login(
        &self,
        provider: &str,
        method: rsi_common::provider_login::ProviderLoginMethod,
    ) -> Result<()> {
        let key = match provider {
            "claude" => "billing_login.claude",
            "codex" => "billing_login.codex",
            _ => return Ok(()),
        };
        if matches!(
            method,
            rsi_common::provider_login::ProviderLoginMethod::Unknown
                | rsi_common::provider_login::ProviderLoginMethod::None
        ) {
            return Ok(());
        }
        self.set_daemon_setting(key, method.as_str())
    }
    pub fn billing_modes(&self) -> Result<std::collections::BTreeMap<String, String>> {
        rsi_common::provider_billing::BILLING_PROVIDERS
            .iter()
            .map(|p| {
                Ok((
                    p.to_string(),
                    self.provider_billing_mode(p, None)?.as_str().to_string(),
                ))
            })
            .collect()
    }
    pub fn session_billing_mode(&self, session: &Session) -> Result<BillingMode> {
        self.provider_billing_mode(&format!("{:?}", session.provider), session.model.as_deref())
    }
    pub fn provider_billing_mode(
        &self,
        provider: &str,
        model: Option<&str>,
    ) -> Result<BillingMode> {
        let name = provider.to_ascii_lowercase();
        let configured = self.get_daemon_setting(&format!("{name}_billing_mode"))?;
        let login_name = if name == "codexappserver" {
            "codex"
        } else {
            &name
        };
        let login = self.get_daemon_setting(&format!("billing_login.{login_name}"))?;
        Ok(rsi_common::provider_billing::resolve(
            provider,
            model,
            configured.as_deref(),
            login.as_deref(),
        ))
    }
    /// Dollar value suitable for billed totals. A missing subscription figure
    /// is a known zero contribution, never unknown metered spend.
    pub fn session_metered_spend(&self, session: &Session) -> Result<Option<f64>> {
        Ok(if self.session_billing_mode(session)?.is_metered() {
            session.cost_usd
        } else {
            Some(0.0)
        })
    }
}

impl Store {
    /// Bounded indexed invocation reads; account utilization is kept separate
    /// from token attribution because providers publish no per-role quota.
    pub fn plan_usage_by_role(
        &self,
        now: chrono::DateTime<chrono::Utc>,
        scope: &super::fleet::FleetScope,
    ) -> Result<Vec<rsi_common::fleet::PlanRoleUsageV1>> {
        use rsi_common::fleet::PlanRoleUsageV1;
        use rusqlite::params;
        use std::collections::BTreeMap;
        let mut result = Vec::new();
        for snapshot in self.load_provider_rate_limit_snapshots()? {
            let provider = format!("{:?}", snapshot.provider);
            if self.provider_billing_mode(&provider, None)?.is_metered() {
                continue;
            }
            for window in snapshot.windows {
                let seconds = match window.window_key.as_str() {
                    "five_hour" => 5 * 3600,
                    "seven_day" => 7 * 86400,
                    _ => continue,
                };
                let end = window
                    .resets_at_epoch
                    .and_then(|t| chrono::DateTime::from_timestamp(t, 0));
                let start = end.unwrap_or(now) - chrono::Duration::seconds(seconds);
                let until = end.unwrap_or(now).min(now);
                let mut stmt = self.conn.prepare(
                    "SELECT i.project_id,i.session_id,COALESCE(i.input_tokens,0),COALESCE(i.output_tokens,0),
                        COALESCE(i.cache_read_tokens,0),COALESCE(i.cache_creation_tokens,0),
                        CASE WHEN lower(COALESCE(s.agent_role,'')) LIKE '%manager%'
                             OR s.id IN (SELECT seat_session_id FROM global_manager_grants)
                             OR s.id IN (SELECT manager_session_id FROM harness_manager_scopes) THEN 'manager_seat'
                          WHEN p.lead_session_id=s.id THEN 'epic_lead'
                          WHEN lower(COALESCE(s.agent_role,'')) LIKE '%review%' THEN 'reviewer'
                          WHEN s.id IN (SELECT session_id FROM topology_node_attempts WHERE session_id IS NOT NULL) THEN 'topology_node'
                          ELSE 'worker' END,
                        CASE WHEN i.trigger_source LIKE 'satellite%'
                             OR s.id IN (SELECT target_session_id FROM satellite_inbound_attempts WHERE state='delivered')
                             THEN 'satellite' ELSE 'local' END
                     FROM (SELECT * FROM model_invocations WHERE created_at>=?1 AND created_at<=?2
                           AND lower(provider)=lower(?3) AND admission_status='admitted'
                           ORDER BY created_at DESC,id LIMIT 50001) i
                     LEFT JOIN sessions s ON s.id=i.session_id LEFT JOIN sessions p ON p.id=s.parent_id")?;
                let mut rows = stmt.query(params![
                    start.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                    until.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                    provider
                ])?;
                let mut groups: BTreeMap<(String, String), PlanRoleUsageV1> = BTreeMap::new();
                let mut count = 0;
                let mut incomplete = false;
                while let Some(row) = rows.next()? {
                    if count == 50000 {
                        incomplete = true;
                        break;
                    }
                    count += 1;
                    let project: Option<String> = row.get(0)?;
                    let session: Option<String> = row.get(1)?;
                    if !scope.admits(
                        project
                            .as_deref()
                            .and_then(|s| uuid::Uuid::parse_str(s).ok()),
                        session
                            .as_deref()
                            .and_then(|s| uuid::Uuid::parse_str(s).ok()),
                    ) {
                        continue;
                    }
                    let role: String = row.get(6)?;
                    let origin: String = row.get(7)?;
                    let usage = groups
                        .entry((role.clone(), origin.clone()))
                        .or_insert_with(|| PlanRoleUsageV1 {
                            provider: provider.clone(),
                            window_key: window.window_key.clone(),
                            role,
                            origin,
                            ..Default::default()
                        });
                    usage.input += row.get::<_, u64>(2)?;
                    usage.output += row.get::<_, u64>(3)?;
                    usage.cache_read += row.get::<_, u64>(4)?;
                    usage.cache_write += row.get::<_, u64>(5)?;
                }
                for mut usage in groups.into_values() {
                    usage.incomplete = incomplete;
                    result.push(usage);
                }
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::fleet::FleetScope;
    use chrono::{SecondsFormat, Utc};
    use rsi_common::global_dashboard::DashboardWindow;
    use rsi_common::types::{SessionProvider, SessionStatus};
    use rusqlite::params;
    use uuid::Uuid;
    fn session(store: &Store, provider: SessionProvider, amount: Option<f64>) -> Session {
        let mut session =
            crate::test_support::test_session(Uuid::new_v4(), "/tmp/billing-test".into());
        session.provider = provider;
        session.status = SessionStatus::Completed;
        session.cost_usd = amount;
        session.total_prompt_tokens = Some(100);
        session.total_output_tokens = Some(20);
        store.insert_session(&session).unwrap();
        session
    }
    fn invocation(store: &Store, session: &Session, amount: Option<f64>) {
        store.conn.execute("INSERT INTO model_invocations(id,session_id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,trigger_source,provider,model,created_at,input_tokens,output_tokens,cache_read_tokens,cache_creation_tokens,estimated_cost_usd)
            VALUES(?1,?2,'session.launch.fresh','session_lifecycle','foreground','paid_capable','admitted','completed','test',?3,'test-model',?4,100,20,5,3,?5)",params![Uuid::new_v4().to_string(),session.id.to_string(),format!("{:?}",session.provider),Utc::now().to_rfc3339_opts(SecondsFormat::Nanos,true),amount]).unwrap();
    }
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn subscription_estimates_keep_tokens_and_only_metered_dollars_are_aggregated() {
        let store = Store::open_in_memory().unwrap();
        let claude = session(&store, SessionProvider::Claude, Some(2500.0));
        let codex = session(&store, SessionProvider::Codex, None);
        let api = session(&store, SessionProvider::OpenRouter, Some(2.0));
        let cloud = session(&store, SessionProvider::Bedrock, Some(3.0));
        // Legacy sessions and invocation reads share exactly the same rule.
        for (s, amount) in [
            (&claude, Some(2500.0)),
            (&codex, None),
            (&api, Some(2.0)),
            (&cloud, Some(3.0)),
        ] {
            invocation(&store, s, amount);
        }
        let stats = store.usage_stats(None).unwrap();
        assert_eq!(stats.total_cost_usd, 5.0);
        assert_eq!(stats.total_input_tokens, 400);
        let fleet = store.fleet_overview(Utc::now()).unwrap();
        assert_eq!(fleet.totals[2].cost, 5.0);
        assert_eq!(fleet.totals[2].input, 400);
        assert_eq!(fleet.totals[2].unknown_usage, 0);
        let dashboard = store
            .global_dashboard(Utc::now(), DashboardWindow::Day, None)
            .unwrap();
        assert_eq!(dashboard.totals.usage.estimated_cost_usd, 5.0);
        assert_eq!(
            store.get_session(claude.id).unwrap().unwrap().cost_usd,
            Some(2500.0)
        );
        // The override reclassifies history on its next read without changing telemetry.
        store
            .set_daemon_setting("claude_billing_mode", "metered_api")
            .unwrap();
        assert_eq!(store.usage_stats(None).unwrap().total_cost_usd, 2505.0);
    }
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn login_detection_and_cloud_routes_share_the_sql_classification() {
        let store = Store::open_in_memory().unwrap();
        store
            .set_daemon_setting("billing_login.claude", "api_key")
            .unwrap();
        for (provider, model, want) in [
            ("Claude", None, BillingMode::MeteredApi),
            ("Codex", None, BillingMode::Subscription),
            ("OpenRouter", None, BillingMode::MeteredApi),
            (
                "Claude",
                Some("us.anthropic.claude-sonnet-4-20250514-v1:0"),
                BillingMode::Cloud,
            ),
        ] {
            assert_eq!(store.provider_billing_mode(provider, model).unwrap(), want);
            let sql = concat!(
                "SELECT ",
                "rsi_billing_mode(?1,?2,(SELECT json_group_object(key,value) FROM daemon_settings WHERE key IN ('claude_billing_mode','codex_billing_mode','codexappserver_billing_mode','pioneer_billing_mode','antigravity_billing_mode','billing_login.claude','billing_login.codex')))"
            );
            let got: String = store
                .conn
                .query_row(sql, params![provider, model], |r| r.get(0))
                .unwrap();
            assert_eq!(got, want.as_str());
        }
    }
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn plan_windows_attribute_recorded_tokens_by_role_without_dollar_estimates() {
        let store = Store::open_in_memory().unwrap();
        let mut worker =
            crate::test_support::test_session(Uuid::new_v4(), "/tmp/billing-role".into());
        worker.provider = SessionProvider::Claude;
        worker.agent_role = Some("reviewer".into());
        store.insert_session(&worker).unwrap();
        invocation(&store, &worker, Some(500.0));
        let now = Utc::now();
        store
            .upsert_provider_rate_limit_snapshot(
                &rsi_common::rpc::ProviderRateLimitSnapshot {
                    provider: SessionProvider::Claude,
                    status: Some("allowed".into()),
                    rate_limit_type: None,
                    overage_status: None,
                    is_using_overage: false,
                    observed_at: now,
                    stale_reason: None,
                    windows: vec![rsi_common::rpc::ProviderRateLimitWindow {
                        window_key: "five_hour".into(),
                        utilization: 0.4,
                        resets_at_epoch: Some(now.timestamp() + 3600),
                    }],
                },
                Some(worker.id),
            )
            .unwrap();
        let roles = store.plan_usage_by_role(now, &FleetScope::All).unwrap();
        assert_eq!(roles.len(), 1);
        assert_eq!(
            (&roles[0].role, roles[0].input, roles[0].output),
            (&"reviewer".to_string(), 100, 20)
        );
        assert_eq!(roles[0].origin, "local");
        assert_eq!(
            store.fleet_overview(now).unwrap().plan_windows[0].windows[0].utilization,
            0.4
        );
    }
}
