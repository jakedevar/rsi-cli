//! Lifetime usage aggregate for the Settings -> Stats category (T8).
//!
//! Read-only rollup across session-linked and background model invocations.
//! Every input column already exists (plan F-001..F-006) — no schema change,
//! no writes, no new index. Three prepared reads per call (totals+count,
//! per-model GROUP BY, per-day GROUP BY), mirroring `load_model_segments`
//! (`store/metrics.rs`) and the `COALESCE(SUM(x), 0)` empty-set precedent
//! (`store/offload.rs`).

use super::Store;
use crate::error::Result;
use rsi_common::types::{ModelUsage, UsageBucket, UsageStats};
use rusqlite::params;
use std::collections::BTreeMap;

impl Store {
    /// Aggregate lifetime usage stats across sessions, optionally scoped to a
    /// single project (D1 — tab-scoped filter passes `Session.project_id` as
    /// a string). Read-only; no writes, no schema change (D2).
    ///
    /// The `(?1 IS NULL OR project_id = ?1)` predicate lets one prepared
    /// statement serve both the filtered and unfiltered case — binding
    /// `None` binds SQL NULL, which short-circuits the `OR` to true for
    /// every row.
    ///
    /// # Errors
    /// Returns an error if the underlying SQLite query fails (e.g. a locked
    /// or corrupted database).
    pub fn usage_stats(&self, project_id: Option<&str>) -> Result<UsageStats> {
        let session_where =
            "s.status != 'Deleted' AND s.is_eval = 0 AND s.session_kind NOT IN ('Group', 'Epic')";
        let rollup_cte = format!(
            "WITH ids(id) AS (
                SELECT s.id FROM sessions s
                WHERE {session_where}
                  AND s.model_invocation_id IS NOT NULL
                  AND (?1 IS NULL OR s.project_id = ?1)
            ),
            linked_session_rollup AS (
                SELECT
                    s.id AS session_id,
                    mi.model AS model,
                    substr(COALESCE(mi.created_at, s.created_at), 1, 10) AS day,
                    CASE WHEN mi.id = s.model_invocation_id THEN 1 ELSE 0 END AS chat_count,
                    COALESCE({linked_cost}, 0) AS cost_usd,
                    COALESCE(mi.input_tokens, 0) AS input_tokens,
                    COALESCE(mi.output_tokens, 0) AS output_tokens,
                    COALESCE(mi.cache_creation_tokens, 0) AS cache_creation_tokens,
                    COALESCE(mi.cache_read_tokens, 0) AS cache_read_tokens,
                    COALESCE(mi.wall_time_ms, 0) AS work_time_ms
                FROM sessions s
                JOIN model_invocations mi
                  ON mi.session_id = s.id
                 AND mi.admission_status = 'admitted'
                JOIN ({cost_increment}) ci ON ci.invocation_id = mi.id
                WHERE {session_where}
                  AND s.model_invocation_id IS NOT NULL
                  AND (?1 IS NULL OR s.project_id = ?1)
            ),
            legacy_session_rollup AS (
                SELECT
                    s.id AS session_id,
                    s.model AS model,
                    substr(s.created_at, 1, 10) AS day,
                    1 AS chat_count,
                    COALESCE({legacy_cost}, 0) AS cost_usd,
                    COALESCE(s.total_prompt_tokens, s.total_input_tokens, 0) AS input_tokens,
                    COALESCE(s.total_output_tokens, 0) AS output_tokens,
                    COALESCE(s.total_cache_creation_tokens, 0) AS cache_creation_tokens,
                    COALESCE(s.total_cache_read_tokens, 0) AS cache_read_tokens,
                    COALESCE(s.work_time_ms, 0) AS work_time_ms
                FROM sessions s
                WHERE {session_where}
                  AND s.model_invocation_id IS NULL
                  AND (?1 IS NULL OR s.project_id = ?1)
            ),
            background_rollup AS (
                SELECT
                    NULL AS session_id,
                    model,
                    substr(created_at, 1, 10) AS day,
                    0 AS chat_count,
                    COALESCE({background_cost}, 0) AS cost_usd,
                    COALESCE(input_tokens, 0) AS input_tokens,
                    COALESCE(output_tokens, 0) AS output_tokens,
                    COALESCE(cache_creation_tokens, 0) AS cache_creation_tokens,
                    COALESCE(cache_read_tokens, 0) AS cache_read_tokens,
                    COALESCE(wall_time_ms, 0) AS work_time_ms
                FROM model_invocations
                WHERE admission_status = 'admitted'
                  AND session_id IS NULL
                  AND (?1 IS NULL OR project_id = ?1)
            ),
            usage_rollup AS (
                SELECT * FROM linked_session_rollup
                UNION ALL
                SELECT * FROM legacy_session_rollup
                UNION ALL
                SELECT * FROM background_rollup
            )",
            // Own cost per invocation, not the session running total (#2593).
            // The predecessor window sorts only the counted sessions `ids`
            // names (the same set `linked_session_rollup` joins), not every
            // admitted invocation of the store (#330, hub #2972).
            cost_increment = crate::model_control::cost::cost_increment_for_ids_sql!(),
            linked_cost = "CASE WHEN rsi_billing_mode(mi.provider,mi.model,(SELECT json_group_object(key,value) FROM daemon_settings WHERE key IN ('claude_billing_mode','codex_billing_mode','codexappserver_billing_mode','pioneer_billing_mode','antigravity_billing_mode','billing_login.claude','billing_login.codex')))='subscription' THEN 0.0 ELSE ci.cost_increment_usd END",
            legacy_cost = "CASE WHEN rsi_billing_mode(s.provider,s.model,(SELECT json_group_object(key,value) FROM daemon_settings WHERE key IN ('claude_billing_mode','codex_billing_mode','codexappserver_billing_mode','pioneer_billing_mode','antigravity_billing_mode','billing_login.claude','billing_login.codex')))='subscription' THEN 0.0 ELSE s.cost_usd END",
            background_cost = "CASE WHEN rsi_billing_mode(provider,model,(SELECT json_group_object(key,value) FROM daemon_settings WHERE key IN ('claude_billing_mode','codex_billing_mode','codexappserver_billing_mode','pioneer_billing_mode','antigravity_billing_mode','billing_login.claude','billing_login.codex')))='subscription' THEN 0.0 ELSE estimated_cost_usd END"
        );
        // One pass over the rollup (#333, hub #2998): the cost window under it
        // is the expensive part, and the three aggregates (totals, per model,
        // per day) used to evaluate it once each. Group by (model, day) once
        // and fold the three views from those rows. `substr(created_at, 1, 10)`
        // rather than `date(created_at)`: RFC3339 with nanosecond fractions
        // ("...T12:00:00.123456789+00:00") is not guaranteed to round-trip
        // through SQLite's date/time parser, but the `YYYY-MM-DD` prefix is
        // always exactly the first 10 bytes.
        let sql = format!(
            "{rollup_cte}
             SELECT model, day, COALESCE(SUM(chat_count), 0), COALESCE(SUM(cost_usd), 0), \
                    COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0), \
                    COALESCE(SUM(cache_creation_tokens), 0), COALESCE(SUM(cache_read_tokens), 0), \
                    COALESCE(SUM(work_time_ms), 0)
             FROM usage_rollup GROUP BY model, day"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![project_id], |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)? as u64,
                    row.get::<_, f64>(3)?,
                    row.get::<_, i64>(4)? as u64,
                    row.get::<_, i64>(5)? as u64,
                    row.get::<_, i64>(6)? as u64,
                    row.get::<_, i64>(7)? as u64,
                    row.get::<_, i64>(8)? as u64,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut stats = UsageStats::default();
        let mut per_model: BTreeMap<String, ModelUsage> = BTreeMap::new();
        let mut timeline: BTreeMap<String, UsageBucket> = BTreeMap::new();
        for (model, day, chats, cost, input, output, creation, read, work) in rows {
            stats.lifetime_chats += chats;
            stats.total_cost_usd += cost;
            stats.total_input_tokens += input;
            stats.total_output_tokens += output;
            stats.total_cache_creation_tokens += creation;
            stats.total_cache_read_tokens += read;
            stats.total_work_time_ms += work;
            if let Some(model) = model {
                let usage = per_model
                    .entry(model.clone())
                    .or_insert_with(|| ModelUsage {
                        model,
                        ..ModelUsage::default()
                    });
                usage.chats += chats;
                usage.cost_usd += cost;
                usage.input_tokens += input;
                usage.output_tokens += output;
                usage.cache_creation_tokens += creation;
                usage.cache_read_tokens += read;
                usage.work_time_ms += work;
            }
            let bucket = timeline.entry(day.clone()).or_insert_with(|| UsageBucket {
                day,
                ..UsageBucket::default()
            });
            bucket.cost_usd += cost;
            bucket.input_tokens += input;
            bucket.output_tokens += output;
            bucket.cache_creation_tokens += creation;
            bucket.cache_read_tokens += read;
        }
        stats.per_model = per_model.into_values().collect();
        stats.timeline = timeline.into_values().collect();
        Ok(stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Dollar-accounting fixtures use API logins. Subscription coverage lives
    // in provider_billing::tests and never treats CLI list prices as bills.
    fn metered_store() -> Store {
        let store = Store::open_in_memory().unwrap();
        store
            .set_daemon_setting("claude_billing_mode", "metered_api")
            .unwrap();
        store
            .set_daemon_setting("codex_billing_mode", "metered_api")
            .unwrap();
        store
    }

    use crate::model_control::{InvocationCompletion, ModelAdmissionRequest, registry};
    use chrono::TimeZone;
    use rsi_common::model_control::{
        InvocationOwner, ModelInvocationPurpose, ModelTier, ModelUsageConfidence,
    };
    use rsi_common::types::{Session, SessionKind, SessionProvider, SessionStatus};
    use std::path::PathBuf;
    use uuid::Uuid;

    /// Minimal valid `Session` fixture; callers override analytics fields.
    fn base_session() -> Session {
        Session {
            context_fill_pct: None,
            id: Uuid::new_v4(),
            provider: SessionProvider::Claude,
            claude_session_id: None,
            query: "hello".to_string(),
            title: None,
            agent_role: None,
            epic_spawn_ordinal: None,
            description: None,
            short_summary: None,
            pending_question: None,
            pending_archive: false,
            working_dir: PathBuf::from("/tmp/test"),
            git_branch: None,
            status: SessionStatus::Completed,
            project_id: None,
            pinned_at: None,
            testing_needed_at: None,
            rotation_disabled_at: None,
            session_kind: SessionKind::Standard,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            cost_usd: None,
            duration_ms: None,
            num_turns: None,
            model: None,
            input_tokens: None,
            output_tokens: None,
            context_window: None,
            resolved_context_budget: None,
            total_input_tokens: None,
            total_prompt_tokens: None,
            total_output_tokens: None,
            total_cache_creation_tokens: None,
            total_cache_read_tokens: None,
            stop_reason: None,
            continued_from: None,
            context_usage_confidence: rsi_common::types::ContextUsageConfidence::Missing,
            daemon_input_tokens: None,
            daemon_output_tokens: None,
            handoff_filepath: None,
            active_task: None,
            group_id: None,
            pipeline_artifact: None,
            workflow_id: None,
            workflow_id_override: None,
            rotation_depth: 0,
            retry_attempt: None,
            max_retries: None,
            effort: None,
            issue_identifier: None,
            issue_url: None,
            issue_tracker_id: None,
            scheduled_job_id: None,
            rating: None,
            harness_version_hash: None,
            test_passed: None,
            clippy_passed: None,
            turn_count: None,
            retry_count: None,
            approval_wait_ms: None,
            work_time_ms: None,
            approval_started_at: None,
            sandbox_kind: None,
            sandbox_root: None,
            sandbox_branch: None,
            sandbox_cleanup_state: None,
            tag: String::new(),
            tags: Vec::new(),
            parent_id: None,
            lead_session_id: None,
            is_eval: false,
            capability_class: None,
            topology_node_id: None,
            topology_iteration: 0,
            provider_cli_version: None,
            provider_capabilities: Vec::new(),
            thinking_tokens: None,
            service_tier: None,
            cache_creation_1h_tokens: None,
            cache_creation_5m_tokens: None,
            permission_denial_count: None,
            subagent_stats_json: None,
            queued_turn_count: None,
            terminal_reason: None,
        }
    }

    /// Seeds sessions across >=2 models, >=2 projects, a NULL-cost row, a
    /// Group container, a Deleted row, an is_eval row, and rows on >=2
    /// distinct `created_at` days. Asserts: SUM NULL-skips, project filter
    /// narrows, D4-excluded rows are absent from `lifetime_chats`, per-model
    /// GROUP BY sums, and day buckets split correctly.
    /// satisfies: F-004, F-005, F-006, F-015
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn usage_stats_aggregates_and_excludes_per_d4() {
        let store = metered_store();
        let project_a = Uuid::new_v4();
        let project_b = Uuid::new_v4();
        let day1 = chrono::Utc.with_ymd_and_hms(2026, 7, 1, 12, 0, 0).unwrap();
        let day2 = chrono::Utc.with_ymd_and_hms(2026, 7, 2, 12, 0, 0).unwrap();

        // Row 1: counted, project A, model X, day1.
        let mut s1 = base_session();
        s1.project_id = Some(project_a);
        s1.model = Some("model-x".to_string());
        s1.cost_usd = Some(1.5);
        s1.total_input_tokens = Some(100);
        s1.total_output_tokens = Some(10);
        s1.total_cache_creation_tokens = Some(1);
        s1.total_cache_read_tokens = Some(2);
        s1.work_time_ms = Some(1000);
        s1.created_at = day1;
        store.insert_session(&s1).unwrap();

        // Row 2: counted, project B, model Y, NULL cost_usd (SUM must skip
        // it, not treat as zero-and-still-summed-wrong), day2.
        let mut s2 = base_session();
        s2.project_id = Some(project_b);
        s2.model = Some("model-y".to_string());
        s2.cost_usd = None;
        s2.total_input_tokens = Some(200);
        s2.created_at = day2;
        store.insert_session(&s2).unwrap();

        // Row 3: counted, project A, model X again (GROUP BY must sum both).
        let mut s3 = base_session();
        s3.project_id = Some(project_a);
        s3.model = Some("model-x".to_string());
        s3.cost_usd = Some(2.5);
        s3.total_input_tokens = Some(50);
        s3.created_at = day1;
        store.insert_session(&s3).unwrap();

        // Row 4: EXCLUDED — container kind Group contributes no chats.
        let mut s4 = base_session();
        s4.session_kind = SessionKind::Group;
        s4.cost_usd = Some(999.0);
        store.insert_session(&s4).unwrap();

        // Row 5: EXCLUDED — logically deleted.
        let mut s5 = base_session();
        s5.status = SessionStatus::Deleted;
        s5.cost_usd = Some(999.0);
        store.insert_session(&s5).unwrap();

        // Row 6: EXCLUDED — eval row.
        let mut s6 = base_session();
        s6.is_eval = true;
        s6.cost_usd = Some(999.0);
        store.insert_session(&s6).unwrap();

        // Unfiltered totals: 3 counted rows (s1, s2, s3).
        let stats = store.usage_stats(None).unwrap();
        assert_eq!(stats.lifetime_chats, 3, "Group/Deleted/is_eval excluded");
        assert_eq!(stats.total_cost_usd, 4.0, "SUM must NULL-skip s2.cost_usd");
        assert_eq!(stats.total_input_tokens, 350);

        // Per-model GROUP BY: model-x sums s1+s3.
        assert_eq!(stats.per_model.len(), 2);
        let model_x = stats
            .per_model
            .iter()
            .find(|m| m.model == "model-x")
            .expect("model-x present");
        assert_eq!(model_x.chats, 2);
        assert_eq!(model_x.cost_usd, 4.0);
        assert_eq!(model_x.input_tokens, 150);

        // Day buckets: day1 (s1+s3) and day2 (s2) are distinct.
        assert_eq!(stats.timeline.len(), 2);
        let bucket1 = stats
            .timeline
            .iter()
            .find(|b| b.day == "2026-07-01")
            .expect("day1 bucket present");
        assert_eq!(bucket1.cost_usd, 4.0);
        let bucket2 = stats
            .timeline
            .iter()
            .find(|b| b.day == "2026-07-02")
            .expect("day2 bucket present");
        assert_eq!(bucket2.input_tokens, 200);

        // Project filter narrows to project A only (s1 + s3).
        let project_a_str = project_a.to_string();
        let filtered = store.usage_stats(Some(&project_a_str)).unwrap();
        assert_eq!(filtered.lifetime_chats, 2);
        assert_eq!(filtered.total_cost_usd, 4.0);
        assert!(filtered.lifetime_chats <= stats.lifetime_chats);
    }

    /// `COALESCE(SUM(x), 0)` on an empty table returns 0, not NULL/error.
    /// satisfies: F-006
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn usage_stats_empty_db_coalesces_to_zero() {
        let store = metered_store();
        let stats = store.usage_stats(None).unwrap();
        assert_eq!(stats.lifetime_chats, 0);
        assert_eq!(stats.total_cost_usd, 0.0);
        assert_eq!(stats.total_input_tokens, 0);
        assert!(stats.per_model.is_empty());
        assert!(stats.timeline.is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn usage_stats_rolls_linked_ledger_rows_and_background_invocations() {
        let store = metered_store();
        let project_id = Uuid::new_v4();

        let mut session = base_session();
        session.project_id = Some(project_id);
        session.model = Some("model-z".to_string());
        session.cost_usd = Some(99.0);
        session.total_input_tokens = Some(999);
        session.total_output_tokens = Some(999);
        session.total_cache_creation_tokens = Some(99);
        session.total_cache_read_tokens = Some(99);
        session.work_time_ms = Some(9999);
        store.insert_session(&session).unwrap();

        let session_registry = registry::lookup(ModelInvocationPurpose::SessionContinueResume)
            .copied()
            .expect("session registry");
        let session_request = ModelAdmissionRequest {
            purpose: ModelInvocationPurpose::SessionContinueResume,
            provider: Some("Codex".to_string()),
            model: Some("model-z".to_string()),
            backend: Some("Codex".to_string()),
            effort: None,
            trigger: "test".to_string(),
            owner: InvocationOwner {
                session_id: Some(session.id),
                project_id: Some(project_id),
                ..Default::default()
            },
            dedup_key: Some("usage-session".to_string()),
            request_fingerprint: Some("sha256:usage-session".to_string()),
            parent_invocation_id: None,
            retry_of_invocation_id: None,
            expected_usage: Some(crate::model_control::ExpectedUsage {
                input_tokens: 120,
                output_tokens: 40,
                cache_creation_tokens: 5,
                cache_read_tokens: 7,
                reasoning_tokens: 0,
                embedding_input_count: 0,
                wall_time_ms: 500,
            }),
            baseline_input_tokens: 0,
            baseline_output_tokens: 0,
            baseline_cache_creation_tokens: 0,
            baseline_cache_read_tokens: 0,
            baseline_reasoning_tokens: 0,
            baseline_embedding_input_count: 0,
            baseline_wall_time_ms: 0,
        };
        let session_invocation_id = match store
            .admit_model_invocation(
                Uuid::new_v4(),
                session_registry,
                ModelTier::Premium,
                &session_request,
            )
            .unwrap()
        {
            crate::store::StoreAdmissionOutcome::Admitted(id) => id,
            other => panic!("unexpected session outcome: {other:?}"),
        };
        store
            .complete_model_invocation(
                session_invocation_id,
                &InvocationCompletion {
                    input_tokens: Some(120),
                    output_tokens: Some(40),
                    cache_creation_tokens: Some(5),
                    cache_read_tokens: Some(7),
                    wall_time_ms: Some(500),
                    estimated_cost_usd: Some(1.25),
                    confidence: Some(ModelUsageConfidence::Measured),
                    ..InvocationCompletion::default()
                },
            )
            .unwrap();
        store
            .set_session_model_invocation(session.id, Some(session_invocation_id))
            .unwrap();

        let background_registry = registry::lookup(ModelInvocationPurpose::TextGenerateRpc)
            .copied()
            .expect("background registry");
        let background_request = ModelAdmissionRequest {
            purpose: ModelInvocationPurpose::TextGenerateRpc,
            provider: Some("Codex".to_string()),
            model: Some("model-bg".to_string()),
            backend: Some("Codex".to_string()),
            effort: None,
            trigger: "test".to_string(),
            owner: InvocationOwner {
                project_id: Some(project_id),
                operator: Some("rpc".to_string()),
                ..Default::default()
            },
            dedup_key: Some("usage-background".to_string()),
            request_fingerprint: Some("sha256:usage-background".to_string()),
            parent_invocation_id: None,
            retry_of_invocation_id: None,
            expected_usage: None,
            baseline_input_tokens: 0,
            baseline_output_tokens: 0,
            baseline_cache_creation_tokens: 0,
            baseline_cache_read_tokens: 0,
            baseline_reasoning_tokens: 0,
            baseline_embedding_input_count: 0,
            baseline_wall_time_ms: 0,
        };
        let background_invocation_id = match store
            .admit_model_invocation(
                Uuid::new_v4(),
                background_registry,
                ModelTier::Premium,
                &background_request,
            )
            .unwrap()
        {
            crate::store::StoreAdmissionOutcome::Admitted(id) => id,
            other => panic!("unexpected background outcome: {other:?}"),
        };
        store
            .complete_model_invocation(
                background_invocation_id,
                &InvocationCompletion {
                    input_tokens: Some(30),
                    output_tokens: Some(10),
                    estimated_cost_usd: Some(0.5),
                    confidence: Some(ModelUsageConfidence::Measured),
                    ..InvocationCompletion::default()
                },
            )
            .unwrap();

        let stats = store.usage_stats(Some(&project_id.to_string())).unwrap();
        assert_eq!(stats.lifetime_chats, 1);
        assert_eq!(stats.total_cost_usd, 1.75);
        assert_eq!(stats.total_input_tokens, 150);
        assert_eq!(stats.total_output_tokens, 50);
        assert_eq!(stats.total_cache_creation_tokens, 5);
        assert_eq!(stats.total_cache_read_tokens, 7);

        let session_model = stats
            .per_model
            .iter()
            .find(|usage| usage.model == "model-z")
            .expect("session model present");
        assert_eq!(session_model.chats, 1);
        assert_eq!(session_model.input_tokens, 120);

        let background_model = stats
            .per_model
            .iter()
            .find(|usage| usage.model == "model-bg")
            .expect("background model present");
        assert_eq!(background_model.chats, 0);
        assert_eq!(background_model.input_tokens, 30);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn usage_stats_attributes_linked_session_tokens_to_invocation_model() {
        let store = metered_store();

        let mut session = base_session();
        session.model = Some("session-model".to_string());
        store.insert_session(&session).unwrap();

        let session_registry = registry::lookup(ModelInvocationPurpose::SessionContinueResume)
            .copied()
            .expect("session registry");
        let request = ModelAdmissionRequest {
            purpose: ModelInvocationPurpose::SessionContinueResume,
            provider: Some("Codex".to_string()),
            model: Some("ledger-model".to_string()),
            backend: Some("Codex".to_string()),
            effort: None,
            trigger: "test".to_string(),
            owner: InvocationOwner {
                session_id: Some(session.id),
                ..Default::default()
            },
            dedup_key: Some("usage-ledger-model".to_string()),
            request_fingerprint: Some("sha256:usage-ledger-model".to_string()),
            parent_invocation_id: None,
            retry_of_invocation_id: None,
            expected_usage: None,
            baseline_input_tokens: 0,
            baseline_output_tokens: 0,
            baseline_cache_creation_tokens: 0,
            baseline_cache_read_tokens: 0,
            baseline_reasoning_tokens: 0,
            baseline_embedding_input_count: 0,
            baseline_wall_time_ms: 0,
        };
        let invocation_id = match store
            .admit_model_invocation(
                Uuid::new_v4(),
                session_registry,
                ModelTier::Premium,
                &request,
            )
            .unwrap()
        {
            crate::store::StoreAdmissionOutcome::Admitted(id) => id,
            other => panic!("unexpected outcome: {other:?}"),
        };
        store
            .complete_model_invocation(
                invocation_id,
                &InvocationCompletion {
                    input_tokens: Some(42),
                    output_tokens: Some(7),
                    confidence: Some(ModelUsageConfidence::Measured),
                    ..InvocationCompletion::default()
                },
            )
            .unwrap();
        store
            .set_session_model_invocation(session.id, Some(invocation_id))
            .unwrap();

        let stats = store.usage_stats(None).unwrap();
        assert!(
            stats
                .per_model
                .iter()
                .all(|usage| usage.model != "session-model")
        );
        let ledger_model = stats
            .per_model
            .iter()
            .find(|usage| usage.model == "ledger-model")
            .expect("ledger model present");
        assert_eq!(ledger_model.input_tokens, 42);
    }

    /// A session of project `project` with `rows` cumulative resume rows (a
    /// restart every seventh row) that `usage_stats` counts as a linked session.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    fn seed_cumulative_session(
        store: &Store,
        project: Uuid,
        tag: &str,
        rows: usize,
        status: SessionStatus,
    ) {
        let mut session = base_session();
        session.project_id = Some(project);
        session.status = status;
        store.insert_session(&session).unwrap();
        for row in 0..rows {
            let cost = if row % 7 == 6 {
                0.05
            } else {
                0.1 * (row + 1) as f64
            };
            store
                .conn
                .execute(
                    "INSERT INTO model_invocations(id, purpose, invocation_kind, foreground,
                                                   paid_risk, admission_status, status, provider,
                                                   model, trigger_source, session_id, project_id,
                                                   estimated_cost_usd, created_at)
                     VALUES(?1, 'session.continue.resume', 'session_lifecycle', 'foreground',
                            'paid_capable', 'admitted', 'completed', 'Claude', 'claude-opus-test',
                            'test', ?2, ?3, ?4, ?5)",
                    params![
                        format!("{tag}-{row}"),
                        session.id.to_string(),
                        project.to_string(),
                        cost,
                        format!("2026-10-14T10:{:02}:00.000000000Z", row % 50)
                    ],
                )
                .unwrap();
        }
        store
            .conn
            .execute(
                "UPDATE sessions SET model_invocation_id = ?1 WHERE id = ?2",
                params![format!("{tag}-0"), session.id.to_string()],
            )
            .unwrap();
    }

    /// The pre-#330 result: the counted sessions' own invocation costs read
    /// through the UNRESTRICTED derived table, summed in SQL.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    fn unrestricted_usage_cost(store: &Store, project: Option<&str>) -> f64 {
        store
            .conn
            .query_row(
                &format!(
                    "SELECT COALESCE(SUM(ci.cost_increment_usd), 0)
                     FROM sessions s
                     JOIN model_invocations mi ON mi.session_id = s.id
                      AND mi.admission_status = 'admitted'
                     JOIN ({}) ci ON ci.invocation_id = mi.id
                     WHERE s.status != 'Deleted' AND s.is_eval = 0
                       AND s.session_kind NOT IN ('Group', 'Epic')
                       AND s.model_invocation_id IS NOT NULL
                       AND (?1 IS NULL OR s.project_id = ?1)",
                    crate::model_control::cost::COST_INCREMENT_SQL
                ),
                params![project],
                |row| row.get(0),
            )
            .unwrap()
    }

    /// #330 (hub #2972): `usage_stats` sorts the predecessor window of the
    /// counted sessions only. Its cost equals the unrestricted window's, for the
    /// whole store and for one project, and the project-scoped call's VM steps
    /// (not wall clock) do not grow with another project's invocations.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn usage_stats_cost_equals_the_unrestricted_window_and_ignores_other_projects_330() {
        use crate::sandbox::startup_restore_tests::vm_ops_during;

        let project = Uuid::new_v4();
        let project_text = project.to_string();
        let measure = |other_rows: usize| {
            let mut store = metered_store();
            let other = Uuid::new_v4();
            seed_cumulative_session(&store, project, "p1", 9, SessionStatus::Completed);
            seed_cumulative_session(&store, project, "p2", 4, SessionStatus::Completed);
            // Deleted sessions are not counted and must not change the cost.
            seed_cumulative_session(&store, project, "pd", 5, SessionStatus::Deleted);
            for session in 0..20 {
                seed_cumulative_session(
                    &store,
                    other,
                    &format!("q{session}"),
                    other_rows,
                    SessionStatus::Completed,
                );
            }
            for filter in [None, Some(project_text.as_str())] {
                let got = store.usage_stats(filter).unwrap();
                let want = unrestricted_usage_cost(&store, filter);
                assert!(want > 0.0, "the fixture must price something: {want}");
                assert!(
                    (got.total_cost_usd - want).abs() < 1e-9,
                    "{filter:?}: {} vs {want}",
                    got.total_cost_usd
                );
                let by_day: f64 = got.timeline.iter().map(|bucket| bucket.cost_usd).sum();
                assert!(
                    (by_day - want).abs() < 1e-9,
                    "{filter:?}: {by_day} vs {want}"
                );
            }
            let (_, scoped_ops) =
                vm_ops_during(&mut store, |store| store.usage_stats(Some(&project_text)));
            let (_, full_ops) = vm_ops_during(&mut store, |store| {
                unrestricted_usage_cost(store, Some(&project_text))
            });
            (scoped_ops, full_ops)
        };
        let (scoped_small, full_small) = measure(2);
        let (scoped_large, full_large) = measure(30);
        eprintln!(
            "330 usage_stats vm ops: scoped {scoped_small} -> {scoped_large}, \
             unrestricted {full_small} -> {full_large}"
        );
        assert!(
            scoped_large <= scoped_small + scoped_small / 20,
            "usage_stats(project) must not grow with another project's invocations: \
             {scoped_small} -> {scoped_large}"
        );
        assert!(
            full_large > full_small * 2,
            "the probe must see the unrestricted window grow: {full_small} -> {full_large}"
        );
    }

    /// One counted session of `rows` cumulative resume rows on `day`, priced
    /// 0.1, 0.2, ... (so the session's own spend is `0.1 * rows`), with 10 input
    /// tokens, 3 output tokens and 5 ms of wall time per row.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    fn seed_priced_session(store: &Store, project: Uuid, model: &str, day: &str, rows: usize) {
        store
            .set_daemon_setting("claude_billing_mode", "metered_api")
            .unwrap();
        let mut session = base_session();
        session.project_id = Some(project);
        store.insert_session(&session).unwrap();
        let tag = Uuid::new_v4();
        for row in 0..rows {
            store
                .conn
                .execute(
                    "INSERT INTO model_invocations(id, purpose, invocation_kind, foreground,
                                                   paid_risk, admission_status, status, provider,
                                                   model, trigger_source, session_id, project_id,
                                                   estimated_cost_usd, input_tokens, output_tokens,
                                                   wall_time_ms, created_at)
                     VALUES(?1, 'session.continue.resume', 'session_lifecycle', 'foreground',
                            'paid_capable', 'admitted', 'completed', 'Claude', ?2,
                            'test', ?3, ?4, ?5, 10, 3, 5, ?6)",
                    params![
                        format!("{tag}-{row}"),
                        model,
                        session.id.to_string(),
                        project.to_string(),
                        0.1 * (row + 1) as f64,
                        format!("{day}T10:{:02}:00.000000000Z", row % 50)
                    ],
                )
                .unwrap();
        }
        store
            .conn
            .execute(
                "UPDATE sessions SET model_invocation_id = ?1 WHERE id = ?2",
                params![format!("{tag}-0"), session.id.to_string()],
            )
            .unwrap();
    }

    /// What the seeded sessions must add up to, worked out from the seed
    /// parameters alone: `(model, day, rows)` per session.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    fn assert_usage_equals_seeded(stats: &UsageStats, seeded: &[(&str, &str, usize)]) {
        use std::collections::BTreeMap;
        let mut models: BTreeMap<&str, (u64, f64, u64)> = BTreeMap::new();
        let mut days: BTreeMap<&str, (f64, u64)> = BTreeMap::new();
        for (model, day, rows) in seeded {
            let entry = models.entry(model).or_default();
            entry.0 += 1;
            entry.1 += 0.1 * *rows as f64;
            entry.2 += 10 * *rows as u64;
            let entry = days.entry(day).or_default();
            entry.0 += 0.1 * *rows as f64;
            entry.1 += 10 * *rows as u64;
        }
        let rows: u64 = seeded.iter().map(|(_, _, rows)| *rows as u64).sum();
        assert_eq!(stats.lifetime_chats, seeded.len() as u64);
        assert!((stats.total_cost_usd - models.values().map(|m| m.1).sum::<f64>()).abs() < 1e-9);
        assert_eq!(stats.total_input_tokens, 10 * rows);
        assert_eq!(stats.total_output_tokens, 3 * rows);
        assert_eq!(stats.total_work_time_ms, 5 * rows);
        assert_eq!(
            stats
                .per_model
                .iter()
                .map(|m| m.model.as_str())
                .collect::<Vec<_>>(),
            models.keys().copied().collect::<Vec<_>>(),
            "per-model rows are sorted by model"
        );
        for usage in &stats.per_model {
            let want = models[usage.model.as_str()];
            assert_eq!(usage.chats, want.0, "{}", usage.model);
            assert!((usage.cost_usd - want.1).abs() < 1e-9, "{}", usage.model);
            assert_eq!(usage.input_tokens, want.2, "{}", usage.model);
        }
        assert_eq!(
            stats
                .timeline
                .iter()
                .map(|b| b.day.as_str())
                .collect::<Vec<_>>(),
            days.keys().copied().collect::<Vec<_>>(),
            "timeline buckets are sorted by day"
        );
        for bucket in &stats.timeline {
            let want = days[bucket.day.as_str()];
            assert!((bucket.cost_usd - want.0).abs() < 1e-9, "{}", bucket.day);
            assert_eq!(bucket.input_tokens, want.1, "{}", bucket.day);
        }
    }

    /// #333 (hub #2998): the totals, the per-model rows and the timeline all come
    /// from ONE evaluation of the rollup, and equal what the seeded sessions add
    /// up to after inserts, an update and a restart of the database file.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn usage_stats_single_pass_equals_the_seeded_totals_across_updates_and_restart_333() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.db");
        let project = Uuid::new_v4();
        let mut seeded = vec![
            ("model-a", "2026-10-14", 9),
            ("model-a", "2026-10-15", 4),
            ("model-b", "2026-10-14", 6),
        ];
        let store = Store::open_test_fixture(&path).unwrap();
        for (model, day, rows) in &seeded {
            seed_priced_session(&store, project, model, day, *rows);
        }
        assert_usage_equals_seeded(&store.usage_stats(None).unwrap(), &seeded);

        // Insert: a later session on a new day and a new model.
        seed_priced_session(&store, project, "model-c", "2026-10-16", 5);
        seeded.push(("model-c", "2026-10-16", 5));
        assert_usage_equals_seeded(&store.usage_stats(None).unwrap(), &seeded);

        // Update: the newest row of one session settles at a higher cumulative
        // figure, so the session's own spend grows by exactly the difference.
        let before = store.usage_stats(None).unwrap().total_cost_usd;
        store
            .conn
            .execute(
                "UPDATE model_invocations SET estimated_cost_usd = estimated_cost_usd + 0.25
                 WHERE model = 'model-b' AND created_at = (
                    SELECT max(created_at) FROM model_invocations WHERE model = 'model-b')",
                [],
            )
            .unwrap();
        let after = store.usage_stats(None).unwrap();
        assert!(
            (after.total_cost_usd - before - 0.25).abs() < 1e-9,
            "{before} -> {}",
            after.total_cost_usd
        );
        let b = after
            .per_model
            .iter()
            .find(|m| m.model == "model-b")
            .unwrap();
        assert!((b.cost_usd - (0.6 + 0.25)).abs() < 1e-9, "{}", b.cost_usd);
        store
            .conn
            .execute(
                "UPDATE model_invocations SET estimated_cost_usd = estimated_cost_usd - 0.25
                 WHERE model = 'model-b' AND created_at = (
                    SELECT max(created_at) FROM model_invocations WHERE model = 'model-b')",
                [],
            )
            .unwrap();

        // A project filter that names no session adds up to nothing.
        let none = store
            .usage_stats(Some(&Uuid::new_v4().to_string()))
            .unwrap();
        assert_eq!(
            (
                none.lifetime_chats,
                none.per_model.len(),
                none.timeline.len()
            ),
            (0, 0, 0)
        );

        // Restart: the reopened file yields the same aggregate.
        drop(store);
        let reopened = Store::open_test_fixture(&path).unwrap();
        assert_usage_equals_seeded(&reopened.usage_stats(None).unwrap(), &seeded);
        assert_usage_equals_seeded(
            &reopened.usage_stats(Some(&project.to_string())).unwrap(),
            &seeded,
        );
    }

    /// #333: `usage_stats` evaluates the cost window once, not once per view.
    /// Its VM steps (not wall clock) stay within twice one evaluation of the
    /// same rollup's grand total, where the three-view version needed about three.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn usage_stats_evaluates_the_rollup_once_333() {
        use crate::sandbox::startup_restore_tests::vm_ops_during;

        let mut store = metered_store();
        let project = Uuid::new_v4();
        for session in 0..40 {
            seed_priced_session(&store, project, "model-a", "2026-10-14", 25 + session % 3);
        }
        let (stats, ops) = vm_ops_during(&mut store, |store| store.usage_stats(None).unwrap());
        assert_eq!(stats.lifetime_chats, 40);
        let (_, one_pass) = vm_ops_during(&mut store, |store| {
            store
                .conn
                .query_row(
                    &format!(
                        "WITH ids(id) AS (SELECT id FROM sessions WHERE model_invocation_id IS NOT NULL)
                         SELECT COALESCE(SUM(ci.cost_increment_usd), 0)
                         FROM sessions s
                         JOIN model_invocations mi ON mi.session_id = s.id
                         JOIN ({}) ci ON ci.invocation_id = mi.id
                         WHERE s.model_invocation_id IS NOT NULL",
                        crate::model_control::cost::cost_increment_for_ids_sql!()
                    ),
                    [],
                    |row| row.get::<_, f64>(0),
                )
                .unwrap()
        });
        assert!(
            ops < one_pass * 2,
            "usage_stats ({ops}) must cost about one evaluation of the rollup ({one_pass}), not one per view"
        );
    }
}
