//! Provider/reset episodes share the normal manager inbox and transport.
//! Per-work audit facts retain membership independently of the bounded notice.
use super::*;
use rsi_common::types::SessionProvider;

const WORK_EVENT: &str = "provider_window_work";
const RESUMED_EVENT: &str = "provider_window_wake_resumed";
const ITEM_LIMIT: usize = 32;
const EPISODE_BATCH: i64 = 32;

impl Store {
    /// Record a parked wake or Reserved launch once per provider/reset episode.
    /// Membership is append-only; inbox reads project its current bounded
    /// list without changing the immutable notice or queuing another wake.
    #[allow(clippy::too_many_arguments)]
    pub fn record_provider_window_held_work(
        &self,
        project: Uuid,
        provider: SessionProvider,
        mut reset: DateTime<Utc>,
        kind: &str,
        id: Uuid,
        name: &str,
        target: Uuid,
        earliest_retry_at: DateTime<Utc>,
    ) -> Result<Option<Uuid>> {
        let Some(config) = self.provider_window_notice_config(project)? else {
            return Ok(None);
        };
        let estimated = !self
            .load_provider_rate_limit_snapshots()?
            .iter()
            .any(|snapshot| {
                snapshot.provider == provider
                    && snapshot.windows.iter().any(|window| {
                        window.utilization >= 1.0
                            && window.resets_at_epoch == Some(reset.timestamp())
                    })
            });
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        // A missing reset is a moving five-minute recheck, not a new outage
        // every tick. Keep its first estimated reset until admission recovers.
        if estimated {
            let existing: Option<String> = self
                .conn
                .query_row(
                    "SELECT json_extract(h.state_json,'$.reset_at') FROM harness_manager_notices h
                 WHERE h.project_id=?1 AND h.kind='ledger_change' AND h.subject_version='hold'
                   AND h.subject_id LIKE 'provider_window:%'
                   AND json_extract(h.state_json,'$.provider')=?2
                   AND json_extract(h.state_json,'$.reset_is_estimate')=1
                   AND NOT EXISTS(SELECT 1 FROM harness_manager_notices r
                     WHERE r.project_id=h.project_id AND r.subject_id=h.subject_id
                       AND r.kind=h.kind AND r.subject_version='recovery')
                 ORDER BY h.sequence DESC LIMIT 1",
                    params![
                        project.to_string(),
                        serde_json::to_value(provider)?.as_str()
                    ],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(existing) = existing {
                reset = DateTime::parse_from_rfc3339(&existing)
                    .map_err(|_| refused("manager_invalid_stored_identity"))?
                    .with_timezone(&Utc);
            }
        }
        let subject = format!(
            "{PROVIDER_WINDOW_SUBJECT_PREFIX}{}:{}",
            serde_json::to_value(provider)?.as_str().unwrap_or_default(),
            reset.to_rfc3339_opts(SecondsFormat::Nanos, true)
        );
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM harness_manager_v2_events
             WHERE project_id=?1 AND kind=?2 AND record_key=?3
               AND json_extract(payload_json,'$.kind')=?4
               AND json_extract(payload_json,'$.id')=?5)",
            params![
                project.to_string(),
                WORK_EVENT,
                subject,
                kind,
                id.to_string()
            ],
            |row| row.get(0),
        )?;
        if exists {
            return Ok(None);
        }
        self.manager_v2_event(
            &config,
            None,
            WORK_EVENT,
            &subject,
            0,
            &json!({
                "kind": kind, "id": id, "name": name.chars().take(160).collect::<String>(),
                "target": target, "earliest_retry_at": earliest_retry_at,
            }),
        )?;
        let state = json!({
            "record_kind": "provider_window", "phase": "hold", "provider": provider,
            "reset_at": reset, "reset_is_estimate": estimated,
        });
        let job = self.insert_provider_window_notice(&config, &subject, "hold", &state)?;
        tx.commit()?;
        Ok(job)
    }

    /// Persist actual acceptance separately from `last_fired_at`, which also
    /// advances for a wake settled after a permanent refusal.
    pub fn record_provider_window_wake_resumed(&self, id: Uuid, target: Uuid) -> Result<()> {
        let Some(project) = self
            .get_session(target)?
            .and_then(|session| session.project_id)
        else {
            return Ok(());
        };
        let mut statement = self.conn.prepare(
            "SELECT DISTINCT e.project_id,e.record_key FROM harness_manager_v2_events e
             WHERE e.project_id=?4 AND e.kind=?1 AND json_extract(e.payload_json,'$.kind')='wake'
               AND json_extract(e.payload_json,'$.id')=?2
               AND NOT EXISTS(SELECT 1 FROM harness_manager_v2_events r
                 WHERE r.project_id=e.project_id AND r.kind=?3 AND r.record_key=e.record_key
                   AND json_extract(r.payload_json,'$.id')=?2)",
        )?;
        let episodes = statement
            .query_map(
                params![
                    WORK_EVENT,
                    id.to_string(),
                    RESUMED_EVENT,
                    project.to_string()
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        for (project, subject) in episodes {
            let project = Uuid::parse_str(&project)
                .map_err(|_| refused("manager_invalid_stored_identity"))?;
            if let Some(config) = self.provider_window_notice_config(project)? {
                self.manager_v2_event(
                    &config,
                    None,
                    RESUMED_EVENT,
                    &subject,
                    1,
                    &json!({"id":id}),
                )?;
            }
        }
        Ok(())
    }

    fn provider_window_notice_config(
        &self,
        project: Uuid,
    ) -> Result<Option<HarnessManagerConfigV1>> {
        let Some(config) = self.get_harness_manager_notice_config(project)? else {
            return Ok(None);
        };
        if config.current_session_id.is_none()
            || config.is_revoked()
            || self
                .get_harness_manager_policy(project)?
                .is_none_or(|policy| policy.revoked)
        {
            return Ok(None);
        }
        Ok(Some(config))
    }

    fn provider_window_snapshot(
        &self,
        project: Uuid,
        subject: &str,
        provider: SessionProvider,
        reset: DateTime<Utc>,
        recovery: bool,
    ) -> Result<Value> {
        // Membership is immutable. Read the real scheduler/attempt rows to
        // recover after a crash between delivery and notice reconciliation.
        let mut statement = self.conn.prepare(
            "SELECT e.payload_json,
               CASE json_extract(e.payload_json,'$.kind')
                 WHEN 'wake' THEN CASE
                   WHEN EXISTS(SELECT 1 FROM harness_manager_v2_events r
                     WHERE r.project_id=e.project_id AND r.kind='provider_window_wake_resumed'
                       AND r.record_key=e.record_key
                       AND json_extract(r.payload_json,'$.id')=json_extract(e.payload_json,'$.id')) THEN 'resumed'
                   WHEN j.id IS NULL OR j.enabled=0 THEN 'cancelled'
                   ELSE 'pending' END
                 ELSE CASE
                   WHEN a.started_at IS NOT NULL THEN 'resumed'
                   WHEN a.id IS NULL OR a.status NOT IN ('reserved','launching') THEN 'cancelled'
                   ELSE 'pending' END
               END
             FROM harness_manager_v2_events e
             LEFT JOIN scheduled_jobs j ON json_extract(e.payload_json,'$.kind')='wake'
               AND j.id=json_extract(e.payload_json,'$.id')
             LEFT JOIN topology_node_attempts a ON json_extract(e.payload_json,'$.kind')='topology_launch'
               AND a.id=json_extract(e.payload_json,'$.id')
             WHERE e.project_id=?1 AND e.kind=?2 AND e.record_key=?3
             ORDER BY e.sequence",
        )?;
        let mut items = Vec::new();
        let mut total = 0usize;
        let mut pending = 0usize;
        let mut resumed = 0usize;
        let mut cancelled = 0usize;
        let mut rows = statement.query(params![project.to_string(), WORK_EVENT, subject])?;
        while let Some(row) = rows.next()? {
            let status: String = row.get(1)?;
            total += 1;
            match status.as_str() {
                "resumed" => resumed += 1,
                "cancelled" => cancelled += 1,
                _ => pending += 1,
            }
            if items.len() < ITEM_LIMIT {
                let mut item: Value = serde_json::from_str(&row.get::<_, String>(0)?)?;
                if recovery {
                    item["status"] = json!(status);
                }
                items.push(item);
            }
        }
        Ok(json!({
            "record_kind": "provider_window", "provider": provider, "reset_at": reset,
            "phase": if recovery { "recovery" } else { "hold" },
            "items": items, "total_count": total, "omitted_count": total.saturating_sub(ITEM_LIMIT),
            "pending_count": pending, "resumed_count": resumed, "cancelled_count": cancelled,
            "detail": if recovery {
                format!("Provider window recovered: {resumed} work items resumed, {cancelled} cancelled; {} omitted from the list.", total.saturating_sub(ITEM_LIMIT))
            } else {
                format!("Provider plan window exhausted: {total} work items parked until {reset}; {} omitted from the list. Automatic retries spend no retry budget.", total.saturating_sub(ITEM_LIMIT))
            },
        }))
    }

    /// A hold notice is an immutable episode identity. Membership can grow as
    /// other scheduler lanes encounter the same outage, so materialize it at
    /// retrieval (and exact re-read), rather than rewriting `state_json`.
    pub(super) fn project_provider_window_notice(
        &self,
        project: Uuid,
        notice: &mut HarnessManagerNoticeV1,
    ) -> Result<()> {
        if notice.kind != "ledger_change"
            || !notice
                .subject_id
                .starts_with(PROVIDER_WINDOW_SUBJECT_PREFIX)
            || notice.subject_version != "hold"
        {
            return Ok(());
        }
        let provider = serde_json::from_value(notice.state["provider"].clone())?;
        let reset = serde_json::from_value(notice.state["reset_at"].clone())?;
        let mut state =
            self.provider_window_snapshot(project, &notice.subject_id, provider, reset, false)?;
        for field in ["reset_is_estimate", "carried_over"] {
            if let Some(value) = notice.state.get(field) {
                state[field] = value.clone();
            }
        }
        notice.state = state;
        Ok(())
    }

    /// Scheduler reconciliation publishes recovery once all parked work has
    /// actually resumed (or been cancelled). Reset alone is not proof of a
    /// successful continuation: another admission gate may still hold it.
    pub fn reconcile_provider_window_notices(&self, now: DateTime<Utc>) -> Result<Vec<Uuid>> {
        let mut statement = self.conn.prepare(
            "SELECT h.project_id,h.subject_id,h.state_json
             FROM harness_manager_notices h
             WHERE h.kind='ledger_change' AND h.subject_id LIKE 'provider_window:%'
               AND h.subject_version='hold'
               AND NOT EXISTS(SELECT 1 FROM harness_manager_notices r
                 WHERE r.project_id=h.project_id AND r.kind=h.kind
                   AND r.subject_id=h.subject_id AND r.subject_version='recovery')
             GROUP BY h.project_id,h.subject_id ORDER BY h.sequence LIMIT ?1",
        )?;
        let episodes = statement
            .query_map([EPISODE_BATCH], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        let mut jobs = Vec::new();
        for (project, subject, state) in episodes {
            let state: Value = serde_json::from_str(&state)?;
            let provider: SessionProvider = serde_json::from_value(state["provider"].clone())?;
            if self
                .provider_usage_window_hold_until(provider, now)?
                .is_some()
            {
                continue;
            }
            let project = Uuid::parse_str(&project)
                .map_err(|_| refused("manager_invalid_stored_identity"))?;
            let Some(config) = self.provider_window_notice_config(project)? else {
                continue;
            };
            let reset = serde_json::from_value(state["reset_at"].clone())?;
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let state = self.provider_window_snapshot(project, &subject, provider, reset, true)?;
            if state["pending_count"] == 0 {
                if let Some(job) =
                    self.insert_provider_window_notice(&config, &subject, "recovery", &state)?
                {
                    jobs.push(job);
                }
            }
            tx.commit()?;
        }
        Ok(jobs)
    }

    fn insert_provider_window_notice(
        &self,
        config: &HarnessManagerConfigV1,
        subject: &str,
        phase: &str,
        state: &Value,
    ) -> Result<Option<Uuid>> {
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM harness_manager_notices
             WHERE project_id=?1 AND kind='ledger_change' AND subject_id=?2 AND subject_version=?3)",
            params![config.project_id.to_string(), subject, phase],
            |row| row.get(0),
        )?;
        if exists {
            return Ok(None);
        }
        let state = serde_json::to_string(state)?;
        let (job, _, _) = self.manager_action_watch_identity(config);
        self.ensure_manager_action_watch(config, subject)?;
        let recorded_at = super::now();
        self.conn.execute(
            "INSERT INTO harness_manager_notices
             (id,job_id,project_id,manager_session_id,scope_version,epic_id,direction,
              source_session_id,recipient_session_id,kind,subject_id,subject_version,
              state_json,recorded_at,queued_at)
             VALUES(?1,?2,?3,?4,?5,NULL,'to_manager',NULL,?4,'ledger_change',?6,?7,?8,?9,?9)",
            params![
                Uuid::new_v4().to_string(),
                job.to_string(),
                config.project_id.to_string(),
                config.manager_session_id.to_string(),
                config.row_version,
                subject,
                phase,
                state,
                recorded_at
            ],
        )?;
        self.refresh_manager_notice_job(job)?;
        Ok(Some(job))
    }
}
