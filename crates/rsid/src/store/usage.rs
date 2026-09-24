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
            "WITH linked_session_rollup AS (
                SELECT
                    s.id AS session_id,
                    mi.model AS model,
                    substr(COALESCE(mi.created_at, s.created_at), 1, 10) AS day,
                    CASE WHEN mi.id = s.model_invocation_id THEN 1 ELSE 0 END AS chat_count,
                    COALESCE(mi.estimated_cost_usd, 0) AS cost_usd,
                    COALESCE(mi.input_tokens, 0) AS input_tokens,
                    COALESCE(mi.output_tokens, 0) AS output_tokens,
                    COALESCE(mi.cache_creation_tokens, 0) AS cache_creation_tokens,
                    COALESCE(mi.cache_read_tokens, 0) AS cache_read_tokens,
                    COALESCE(mi.wall_time_ms, 0) AS work_time_ms
                FROM sessions s
                JOIN model_invocations mi
                  ON mi.session_id = s.id
                 AND mi.admission_status = 'admitted'
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
                    COALESCE(s.cost_usd, 0) AS cost_usd,
                    COALESCE(s.total_input_tokens, 0) AS input_tokens,
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
                    COALESCE(estimated_cost_usd, 0) AS cost_usd,
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
            )"
        );
        let totals_sql = format!(
            "{rollup_cte}
             SELECT COALESCE(SUM(chat_count), 0), COALESCE(SUM(cost_usd), 0), COALESCE(SUM(input_tokens), 0), \
                    COALESCE(SUM(output_tokens), 0), COALESCE(SUM(cache_creation_tokens), 0), \
                    COALESCE(SUM(cache_read_tokens), 0), COALESCE(SUM(work_time_ms), 0)
             FROM usage_rollup"
        );
        let (
            lifetime_chats,
            total_cost_usd,
            total_input_tokens,
            total_output_tokens,
            total_cache_creation_tokens,
            total_cache_read_tokens,
            total_work_time_ms,
        ): (i64, f64, i64, i64, i64, i64, i64) =
            self.conn
                .query_row(&totals_sql, params![project_id], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                })?;

        let model_sql = format!(
            "{rollup_cte}
             SELECT model, COALESCE(SUM(chat_count), 0), COALESCE(SUM(cost_usd), 0), COALESCE(SUM(input_tokens), 0), \
                    COALESCE(SUM(output_tokens), 0), COALESCE(SUM(cache_creation_tokens), 0), \
                    COALESCE(SUM(cache_read_tokens), 0), COALESCE(SUM(work_time_ms), 0) \
             FROM usage_rollup WHERE model IS NOT NULL GROUP BY model ORDER BY model ASC"
        );
        let mut model_stmt = self.conn.prepare(&model_sql)?;
        let per_model = model_stmt
            .query_map(params![project_id], |row| {
                Ok(ModelUsage {
                    model: row.get(0)?,
                    chats: row.get::<_, i64>(1)? as u64,
                    cost_usd: row.get(2)?,
                    input_tokens: row.get::<_, i64>(3)? as u64,
                    output_tokens: row.get::<_, i64>(4)? as u64,
                    cache_creation_tokens: row.get::<_, i64>(5)? as u64,
                    cache_read_tokens: row.get::<_, i64>(6)? as u64,
                    work_time_ms: row.get::<_, i64>(7)? as u64,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        // `substr(created_at, 1, 10)` rather than `date(created_at)`: RFC3339
        // with nanosecond fractions ("...T12:00:00.123456789+00:00") is not
        // guaranteed to round-trip through SQLite's date/time parser, but the
        // `YYYY-MM-DD` prefix is always exactly the first 10 bytes.
        let day_sql = format!(
            "{rollup_cte}
             SELECT day, COALESCE(SUM(cost_usd), 0), COALESCE(SUM(input_tokens), 0), \
                    COALESCE(SUM(output_tokens), 0), COALESCE(SUM(cache_creation_tokens), 0), \
                    COALESCE(SUM(cache_read_tokens), 0)
             FROM usage_rollup GROUP BY day ORDER BY day ASC"
        );
        let mut day_stmt = self.conn.prepare(&day_sql)?;
        let timeline = day_stmt
            .query_map(params![project_id], |row| {
                Ok(UsageBucket {
                    day: row.get(0)?,
                    cost_usd: row.get(1)?,
                    input_tokens: row.get::<_, i64>(2)? as u64,
                    output_tokens: row.get::<_, i64>(3)? as u64,
                    cache_creation_tokens: row.get::<_, i64>(4)? as u64,
                    cache_read_tokens: row.get::<_, i64>(5)? as u64,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(UsageStats {
            lifetime_chats: lifetime_chats as u64,
            total_cost_usd,
            total_input_tokens: total_input_tokens as u64,
            total_output_tokens: total_output_tokens as u64,
            total_cache_creation_tokens: total_cache_creation_tokens as u64,
            total_cache_read_tokens: total_cache_read_tokens as u64,
            total_work_time_ms: total_work_time_ms as u64,
            per_model,
            timeline,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    #[test]
    fn usage_stats_aggregates_and_excludes_per_d4() {
        let store = Store::open_in_memory().unwrap();
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
    #[test]
    fn usage_stats_empty_db_coalesces_to_zero() {
        let store = Store::open_in_memory().unwrap();
        let stats = store.usage_stats(None).unwrap();
        assert_eq!(stats.lifetime_chats, 0);
        assert_eq!(stats.total_cost_usd, 0.0);
        assert_eq!(stats.total_input_tokens, 0);
        assert!(stats.per_model.is_empty());
        assert!(stats.timeline.is_empty());
    }

    #[test]
    fn usage_stats_rolls_linked_ledger_rows_and_background_invocations() {
        let store = Store::open_in_memory().unwrap();
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

    #[test]
    fn usage_stats_attributes_linked_session_tokens_to_invocation_model() {
        let store = Store::open_in_memory().unwrap();

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
}
