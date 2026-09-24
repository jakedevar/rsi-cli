//! Durable journal and absent-root custody adoption for Epic R R-a1.
use super::Store;
use super::sandbox_custody::{CustodyCause, transition_terminal_root_tx};
use crate::error::{DaemonError, Result};
use chrono::{SecondsFormat, Utc};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::{Value, json};
use uuid::Uuid;

fn timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn sql_generation(value: u64) -> Result<i64> {
    i64::try_from(value)
        .map_err(|_| DaemonError::Store("custody generation exceeds SQLite INTEGER".into()))
}

fn sql_count(value: i64) -> Result<usize> {
    usize::try_from(value)
        .map_err(|_| DaemonError::Store("sandbox reclaim row count is invalid".into()))
}

// RSI-RELEASED-MIGRATION-BEGIN: v131-sandbox-reclaim-journal-schema
#[allow(clippy::too_many_lines)]
pub(crate) fn apply_sandbox_reclaim_journal_migration(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE sandbox_reclaim_runs (
            run_id TEXT PRIMARY KEY,
            trigger TEXT NOT NULL CHECK(trigger IN ('startup','pressure','periodic','operator','manager')),
            dry_run INTEGER NOT NULL CHECK(dry_run IN (0,1)),
            requested_max INTEGER NOT NULL CHECK(requested_max > 0),
            manager_operation_id TEXT,
            started_at TEXT NOT NULL,
            finished_at TEXT,
            stop_reason TEXT,
            free_bytes_before INTEGER CHECK(free_bytes_before IS NULL OR free_bytes_before >= 0),
            free_bytes_after INTEGER CHECK(free_bytes_after IS NULL OR free_bytes_after >= 0),
            adopted_count INTEGER NOT NULL DEFAULT 0 CHECK(adopted_count >= 0),
            reclaimed_count INTEGER NOT NULL DEFAULT 0 CHECK(reclaimed_count >= 0),
            retained_count INTEGER NOT NULL DEFAULT 0 CHECK(retained_count >= 0),
            reclaimed_bytes INTEGER NOT NULL DEFAULT 0 CHECK(reclaimed_bytes >= 0),
            row_version INTEGER NOT NULL DEFAULT 1 CHECK(row_version > 0),
            CHECK(finished_at IS NULL OR finished_at >= started_at)
        );
        CREATE TABLE sandbox_reclaim_items (
            run_id TEXT NOT NULL REFERENCES sandbox_reclaim_runs(run_id) ON DELETE RESTRICT,
            custody_id TEXT NOT NULL REFERENCES sandbox_custody_roots(custody_id) ON DELETE RESTRICT,
            generation INTEGER NOT NULL CHECK(generation > 0),
            owner_session_id TEXT REFERENCES sessions(id) ON DELETE RESTRICT,
            owner_status TEXT,
            owner_updated_at TEXT,
            class TEXT NOT NULL CHECK(class IN ('clean','dirty_preserve','absent_adopted','retained')),
            effectful INTEGER NOT NULL CHECK(effectful IN (0,1)),
            sandbox_root TEXT NOT NULL,
            sandbox_branch TEXT NOT NULL,
            branch_oid TEXT,
            preserve_ref TEXT,
            preserve_oid TEXT,
            ignored_summary_json TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(ignored_summary_json)),
            bytes_estimate INTEGER NOT NULL DEFAULT 0 CHECK(bytes_estimate >= 0),
            phase TEXT NOT NULL CHECK(phase IN ('intent','target_pending','quarantined','preserved','removed','settled','retained','refused','recovery_required')),
            reason_code TEXT,
            row_version INTEGER NOT NULL DEFAULT 1 CHECK(row_version > 0),
            updated_at TEXT NOT NULL,
            PRIMARY KEY(run_id,custody_id)
        );
        CREATE TABLE sandbox_reclaim_recreations (
            recreation_id TEXT PRIMARY KEY CHECK(rsi_uuid_is_canonical(recreation_id)),
            session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE RESTRICT
                CHECK(rsi_uuid_is_canonical(session_id)),
            old_custody_id TEXT NOT NULL REFERENCES sandbox_custody_roots(custody_id) ON DELETE RESTRICT
                CHECK(rsi_uuid_is_canonical(old_custody_id)),
            new_allocation_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(new_allocation_id)),
            new_root TEXT NOT NULL UNIQUE,
            new_custody_id TEXT REFERENCES sandbox_custody_roots(custody_id) ON DELETE RESTRICT
                CHECK(new_custody_id IS NULL OR rsi_uuid_is_canonical(new_custody_id)),
            branch TEXT NOT NULL,
            recorded_oid TEXT NOT NULL
                CHECK(length(recorded_oid)=40 AND recorded_oid=lower(recorded_oid) AND recorded_oid NOT GLOB '*[^0-9a-f]*'),
            checkout_oid TEXT
                CHECK(checkout_oid IS NULL OR (length(checkout_oid)=40 AND checkout_oid=lower(checkout_oid) AND checkout_oid NOT GLOB '*[^0-9a-f]*')),
            preserve_ref TEXT,
            preserve_restored INTEGER NOT NULL CHECK(preserve_restored IN (0,1)),
            restore_skip_reason TEXT,
            phase TEXT NOT NULL CHECK(phase IN ('intent','worktree_added','linked','abandoned','refused')),
            reason_code TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            CHECK((phase='linked') = (new_custody_id IS NOT NULL)),
            CHECK(phase NOT IN ('worktree_added','linked') OR checkout_oid IS NOT NULL)
        );
        CREATE INDEX sandbox_reclaim_runs_open ON sandbox_reclaim_runs(started_at,run_id) WHERE finished_at IS NULL;
        CREATE UNIQUE INDEX sandbox_reclaim_runs_manager_operation_unique
            ON sandbox_reclaim_runs(manager_operation_id) WHERE manager_operation_id IS NOT NULL;
        CREATE INDEX sandbox_reclaim_items_phase ON sandbox_reclaim_items(phase,updated_at,run_id);
        CREATE UNIQUE INDEX sandbox_reclaim_items_one_effectful_per_generation
            ON sandbox_reclaim_items(custody_id,generation) WHERE effectful=1;
        CREATE UNIQUE INDEX sandbox_reclaim_recreations_one_open_per_session
            ON sandbox_reclaim_recreations(session_id) WHERE phase IN ('intent','worktree_added');
        CREATE TRIGGER sandbox_reclaim_runs_identity_immutable BEFORE UPDATE ON sandbox_reclaim_runs
          WHEN NEW.run_id!=OLD.run_id OR NEW.trigger!=OLD.trigger OR NEW.dry_run!=OLD.dry_run OR NEW.requested_max!=OLD.requested_max OR NEW.manager_operation_id IS NOT OLD.manager_operation_id OR NEW.started_at!=OLD.started_at
          BEGIN SELECT RAISE(ABORT,'sandbox reclaim run identity is immutable'); END;
        CREATE TRIGGER sandbox_reclaim_runs_no_delete BEFORE DELETE ON sandbox_reclaim_runs
          BEGIN SELECT RAISE(ABORT,'sandbox reclaim runs are retained'); END;
        CREATE TRIGGER sandbox_reclaim_items_identity_immutable BEFORE UPDATE ON sandbox_reclaim_items
          WHEN NEW.run_id!=OLD.run_id OR NEW.custody_id!=OLD.custody_id OR NEW.generation!=OLD.generation OR NEW.owner_session_id IS NOT OLD.owner_session_id OR NEW.owner_status IS NOT OLD.owner_status OR NEW.owner_updated_at IS NOT OLD.owner_updated_at OR NEW.class!=OLD.class OR NEW.effectful!=OLD.effectful OR NEW.sandbox_root!=OLD.sandbox_root OR NEW.sandbox_branch!=OLD.sandbox_branch OR NEW.branch_oid IS NOT OLD.branch_oid
          BEGIN SELECT RAISE(ABORT,'sandbox reclaim item identity is immutable'); END;
        CREATE TRIGGER sandbox_reclaim_items_phase_forward BEFORE UPDATE OF phase ON sandbox_reclaim_items
          WHEN NOT (
            NEW.phase=OLD.phase OR
            (OLD.phase='intent' AND NEW.phase IN ('target_pending','quarantined','settled','retained','refused','recovery_required')) OR
            (OLD.phase='target_pending' AND NEW.phase IN ('quarantined','retained','refused','recovery_required')) OR
            (OLD.phase='quarantined' AND NEW.phase IN ('preserved','removed','retained','recovery_required')) OR
            (OLD.phase='preserved' AND NEW.phase IN ('removed','retained','recovery_required')) OR
            (OLD.phase='removed' AND NEW.phase IN ('settled','recovery_required'))
          ) BEGIN SELECT RAISE(ABORT,'sandbox reclaim item phase must move forward'); END;
        CREATE TRIGGER sandbox_reclaim_items_no_delete BEFORE DELETE ON sandbox_reclaim_items
          BEGIN SELECT RAISE(ABORT,'sandbox reclaim items are retained'); END;
        CREATE TRIGGER sandbox_reclaim_recreations_identity_immutable BEFORE UPDATE ON sandbox_reclaim_recreations
          WHEN NEW.recreation_id!=OLD.recreation_id OR NEW.session_id!=OLD.session_id OR
            NEW.old_custody_id!=OLD.old_custody_id OR NEW.new_allocation_id!=OLD.new_allocation_id OR
            NEW.new_root!=OLD.new_root OR NEW.branch!=OLD.branch OR NEW.recorded_oid!=OLD.recorded_oid OR
            NEW.preserve_ref IS NOT OLD.preserve_ref OR NEW.created_at!=OLD.created_at
          BEGIN SELECT RAISE(ABORT,'sandbox reclaim recreation identity is immutable'); END;
        CREATE TRIGGER sandbox_reclaim_recreations_phase_forward BEFORE UPDATE OF phase ON sandbox_reclaim_recreations
          WHEN NOT (
            NEW.phase=OLD.phase OR
            (OLD.phase='intent' AND NEW.phase='worktree_added') OR
            (OLD.phase IN ('intent','worktree_added') AND NEW.phase IN ('abandoned','refused')) OR
            (OLD.phase='worktree_added' AND NEW.phase='linked')
          ) BEGIN SELECT RAISE(ABORT,'sandbox reclaim recreation phase must move forward'); END;
        CREATE TRIGGER sandbox_reclaim_recreations_no_delete BEFORE DELETE ON sandbox_reclaim_recreations
          BEGIN SELECT RAISE(ABORT,'sandbox reclaim recreations are retained'); END;"
    )?;
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: v131-sandbox-reclaim-journal-schema

#[derive(Debug, Clone)]
pub(crate) struct AbsentRootCandidate {
    pub custody_id: Uuid,
    pub generation: u64,
    pub state: String,
    pub owner_session_id: Option<Uuid>,
    pub owner_status: Option<String>,
    pub owner_updated_at: Option<String>,
    pub sandbox_root: String,
    pub sandbox_branch: String,
    pub allocation_id: Option<String>,
    pub canonical_repo_dir: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdoptionOutcome {
    Adopted,
    Retained(&'static str),
}

impl Store {
    pub(crate) fn sandbox_reclaim_run_result(&self, run_id: Uuid) -> Result<Value> {
        let run = self
            .conn
            .query_row(
                "SELECT trigger,dry_run,requested_max,started_at,finished_at,stop_reason,
                        adopted_count,reclaimed_count,retained_count,reclaimed_bytes
                   FROM sandbox_reclaim_runs WHERE run_id=?1",
                [run_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, bool>(1)?,
                        row.get::<_, u32>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, u32>(6)?,
                        row.get::<_, u32>(7)?,
                        row.get::<_, u32>(8)?,
                        row.get::<_, u64>(9)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| DaemonError::Store("sandbox reclaim run disappeared".into()))?;
        let mut statement = self.conn.prepare(
            "SELECT custody_id,generation,class,phase,reason_code,sandbox_root,sandbox_branch,branch_oid
               FROM sandbox_reclaim_items WHERE run_id=?1 ORDER BY custody_id",
        )?;
        let rows = statement.query_map([run_id.to_string()], |row| {
            Ok(json!({
                "custody_id": row.get::<_, String>(0)?,
                "generation": row.get::<_, u64>(1)?,
                "class": row.get::<_, String>(2)?,
                "phase": row.get::<_, String>(3)?,
                "reason_code": row.get::<_, Option<String>>(4)?,
                "sandbox_root": row.get::<_, String>(5)?,
                "sandbox_branch": row.get::<_, String>(6)?,
                "branch_oid": row.get::<_, Option<String>>(7)?,
            }))
        })?;
        let items = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(json!({
            "run": {
                "run_id": run_id,
                "trigger": run.0,
                "dry_run": run.1,
                "requested_max": run.2,
                "started_at": run.3,
                "finished_at": run.4,
                "stop_reason": run.5,
                "adopted_count": run.6,
                "reclaimed_count": run.7,
                "retained_count": run.8,
                "reclaimed_bytes": run.9,
            },
            "items": items,
        }))
    }

    pub(crate) fn absent_root_candidates(
        &self,
        max_count: usize,
    ) -> Result<Vec<AbsentRootCandidate>> {
        let mut stmt = self.conn.prepare(
            "SELECT r.custody_id,r.generation,r.state,r.owner_session_id,s.status,s.updated_at,
                    r.sandbox_root,r.sandbox_branch,r.allocation_id,r.canonical_repo_dir
               FROM sandbox_custody_roots r LEFT JOIN sessions s ON s.id=r.owner_session_id
              WHERE (r.state='live' AND r.validation_state='verified' AND r.validated_generation=r.generation)
                 OR (r.state='quarantined' AND r.validation_error_code='root_missing')
              ORDER BY r.custody_id LIMIT ?1")?;
        let limit = i64::try_from(max_count).unwrap_or(i64::MAX);
        let rows = stmt.query_map([limit], |row| {
            let id: String = row.get(0)?;
            let owner: Option<String> = row.get(3)?;
            Ok(AbsentRootCandidate {
                custody_id: Uuid::parse_str(&id).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?,
                generation: row.get::<_, i64>(1)?.cast_unsigned(),
                state: row.get(2)?,
                owner_session_id: owner
                    .map(|s| {
                        Uuid::parse_str(&s).map_err(|e| {
                            rusqlite::Error::FromSqlConversionFailure(
                                3,
                                rusqlite::types::Type::Text,
                                Box::new(e),
                            )
                        })
                    })
                    .transpose()?,
                owner_status: row.get(4)?,
                owner_updated_at: row.get(5)?,
                sandbox_root: row.get(6)?,
                sandbox_branch: row.get(7)?,
                allocation_id: row.get(8)?,
                canonical_repo_dir: row.get(9)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub(crate) fn absent_root_gate(&self, c: &AbsentRootCandidate) -> Result<Option<&'static str>> {
        let owner = c.owner_session_id;
        if c.state == "live"
            && !matches!(
                c.owner_status.as_deref(),
                Some("Completed" | "Failed" | "Interrupted" | "Archived" | "Deleted")
            )
        {
            return Ok(Some("owner_not_terminal"));
        }
        let unsafe_effect: bool = self
            .conn
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM sandbox_custody_roots r
                     WHERE r.custody_id=?1 AND r.generation=?2
                       AND (r.reserved_effects!=0 OR r.active_effects!=0
                         OR (r.state='live' AND (r.validation_state!='verified' OR r.validated_generation!=r.generation)))
                ) OR EXISTS(
                    SELECT 1 FROM sandbox_target_reclaim_intents i
                     WHERE i.custody_id=?1 AND i.generation=?2
                       AND i.state IN ('Prepared','Staged','Deleting')
                )",
                params![c.custody_id.to_string(), sql_generation(c.generation)?],
                |r| r.get(0),
            )
            .unwrap_or(true);
        if unsafe_effect {
            return Ok(Some("custody_effect_or_reclaim_pending"));
        }
        let checks: (i64,i64,i64,i64) = self.conn.query_row(
            "SELECT count(*),sum(CASE WHEN status IN ('Completed','Failed','Interrupted','Archived','Deleted') AND pending_archive=0 THEN 1 ELSE 0 END),
             sum(CASE WHEN sandbox_kind='GitWorktree' AND working_dir=?5 AND sandbox_root=?3 AND sandbox_branch=?4 AND ((?2='live' AND sandbox_cleanup_state='Live') OR (?2='quarantined' AND sandbox_cleanup_state='Failed')) THEN 1 ELSE 0 END),
             sum(CASE WHEN ?2='live' AND session_kind IN ('Group','Epic') THEN 1 ELSE 0 END)
             FROM sessions WHERE sandbox_custody_id=?1",
            params![c.custody_id.to_string(),c.state,c.sandbox_root,c.sandbox_branch,c.canonical_repo_dir],
            |r| Ok((r.get(0)?,r.get::<_,Option<i64>>(1)?.unwrap_or(0),r.get::<_,Option<i64>>(2)?.unwrap_or(0),r.get::<_,Option<i64>>(3)?.unwrap_or(0))))?;
        if checks.0 == 0 || checks.1 != checks.0 || checks.2 != checks.0 || checks.3 != 0 {
            return Ok(Some("participant_not_terminal"));
        }
        let mut participants = self
            .conn
            .prepare("SELECT id FROM sessions WHERE sandbox_custody_id=?1")?;
        let participant_ids = participants
            .query_map([c.custody_id.to_string()], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let retry_eligible = self.startup_retry_eligible_sessions()?;
        for id in &participant_ids {
            let participant = Uuid::parse_str(id).map_err(|error| {
                DaemonError::Store(format!("invalid participant UUID: {error}"))
            })?;
            if retry_eligible.contains(&participant) {
                return Ok(Some("retry_eligible"));
            }
        }
        for participant in &participant_ids {
            let lead: bool = self.conn.query_row("SELECT EXISTS(SELECT 1 FROM sessions WHERE lead_session_id=?1 AND status NOT IN ('Archived','Deleted'))",[participant],|r|r.get(0))?;
            if lead {
                return Ok(Some("live_lead"));
            }
            let dependent: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM agent_successor_reservations WHERE predecessor_session_id=?1 AND state IN ('reserved','launching','committed','uncertain'))
                  OR EXISTS(SELECT 1 FROM manager_review_assignments WHERE author_session_id=?1 AND state IN ('reserved','allocating','active'))
                  OR EXISTS(SELECT 1 FROM archive_cleanup_runs WHERE custody_id=?2 AND phase NOT IN ('settled','refused'))
                  OR EXISTS(SELECT 1 FROM source_worktree_settlement_items WHERE custody_id=?2 AND phase NOT IN ('refused','unattempted','settled'))
                  OR EXISTS(SELECT 1 FROM harness_manager_scopes WHERE manager_session_id=?1)
                  OR EXISTS(SELECT 1 FROM harness_manager_v2_operations WHERE state IN ('queued','running','uncertain') AND (target_session_id=?1 OR instr(payload_json,?1)>0 OR instr(payload_json,?2)>0))
                  OR EXISTS(SELECT 1 FROM harness_manager_v2_work_facts WHERE archived=0 AND (instr(record_key,?1)>0 OR instr(work_key,?1)>0 OR instr(payload_json,?1)>0))",
                params![participant,c.custody_id.to_string()],|r|r.get(0)).unwrap_or(true);
            if dependent {
                return Ok(Some("pending_consumer"));
            }
        }
        if let Some(owner_id) = owner {
            let dependent: bool=self.conn.query_row("SELECT EXISTS(SELECT 1 FROM agent_successor_reservations WHERE predecessor_session_id=?1 AND state IN ('reserved','launching','committed','uncertain'))",[owner_id.to_string()],|r|r.get(0)).unwrap_or(true);
            if dependent {
                return Ok(Some("successor_reserved"));
            }
        }
        Ok(None)
    }

    pub(crate) fn absent_root_dependency_gate(
        &self,
        c: &AbsentRootCandidate,
    ) -> Result<Option<&'static str>> {
        let dependencies =
            self.source_worktree_targeted_dependencies_for_absent_root(c.custody_id, c.generation)?;
        if !dependencies.complete {
            return Ok(Some(
                dependencies
                    .reason
                    .unwrap_or("dependency_evidence_incomplete"),
            ));
        }
        if dependencies.scheduled_dependency_count != 0
            || dependencies.session_path_dependency_count != 0
        {
            return Ok(Some("dependency_present"));
        }
        Ok(None)
    }

    pub(crate) fn begin_absent_adoption_run(
        &self,
        trigger: &str,
        dry_run: bool,
        max_count: usize,
        manager_operation_id: Option<Uuid>,
    ) -> Result<(Uuid, bool)> {
        let id = Uuid::new_v4();
        let requested_max = i64::try_from(max_count)
            .map_err(|_| DaemonError::InvalidParam("max_count exceeds SQLite INTEGER".into()))?;
        let inserted = self.conn.execute(
            "INSERT INTO sandbox_reclaim_runs(
                run_id,trigger,dry_run,requested_max,manager_operation_id,started_at
             ) VALUES(?1,?2,?3,?4,?5,?6)
             ON CONFLICT(manager_operation_id) WHERE manager_operation_id IS NOT NULL DO NOTHING",
            params![
                id.to_string(),
                trigger,
                i64::from(dry_run),
                requested_max,
                manager_operation_id.map(|value| value.to_string()),
                timestamp(),
            ],
        )?;
        if inserted == 1 {
            return Ok((id, true));
        }
        let manager_operation_id = manager_operation_id.ok_or_else(|| {
            DaemonError::Store(
                "sandbox reclaim run insert was ignored without an operation id".into(),
            )
        })?;
        let existing = self
            .conn
            .query_row(
                "SELECT run_id,trigger,dry_run,requested_max
                   FROM sandbox_reclaim_runs WHERE manager_operation_id=?1",
                [manager_operation_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, bool>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| {
                DaemonError::Store("sandbox reclaim manager operation row disappeared".into())
            })?;
        if existing.1 != trigger || existing.2 != dry_run || existing.3 != requested_max {
            return Err(DaemonError::Store(
                "sandbox reclaim manager operation retry changed its request".into(),
            ));
        }
        let run_id = Uuid::parse_str(&existing.0)
            .map_err(|error| DaemonError::Store(format!("invalid reclaim run UUID: {error}")))?;
        Ok((run_id, false))
    }

    pub(crate) fn record_absent_adoption_item(
        &mut self,
        run_id: Uuid,
        c: &AbsentRootCandidate,
        branch_oid: Option<&str>,
        outcome: AdoptionOutcome,
        dry_run: bool,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let generation = sql_generation(c.generation)?;
        let now = timestamp();
        let phase = match outcome {
            AdoptionOutcome::Adopted if dry_run => "settled",
            AdoptionOutcome::Adopted => "settled",
            AdoptionOutcome::Retained(_) => "retained",
        };
        let (class, reason) = match outcome {
            AdoptionOutcome::Adopted => ("absent_adopted", Some("external_removal")),
            AdoptionOutcome::Retained(code) => ("retained", Some(code)),
        };
        let effectful = matches!(outcome, AdoptionOutcome::Adopted) && !dry_run;
        tx.execute("INSERT INTO sandbox_reclaim_items(run_id,custody_id,generation,owner_session_id,owner_status,owner_updated_at,class,effectful,sandbox_root,sandbox_branch,branch_oid,phase,reason_code,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",params![run_id.to_string(),c.custody_id.to_string(),generation,c.owner_session_id.map(|v|v.to_string()),c.owner_status,c.owner_updated_at,class,i64::from(effectful),c.sandbox_root,c.sandbox_branch,branch_oid,phase,reason,now])?;
        if matches!(outcome, AdoptionOutcome::Adopted) && !dry_run {
            if c.state == "live" {
                transition_terminal_root_tx(
                    &tx,
                    c.custody_id,
                    c.generation,
                    CustodyCause::Purge,
                    "purged",
                    "tombstoned",
                    "historical_purged",
                    None,
                )?;
            } else {
                transition_quarantined_missing_root_tx(&tx, c)?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn finish_absent_adoption_run(
        &self,
        run_id: Uuid,
        stop_reason: Option<&str>,
    ) -> Result<()> {
        self.conn.execute("UPDATE sandbox_reclaim_runs SET finished_at=?2,stop_reason=?3,adopted_count=(SELECT count(*) FROM sandbox_reclaim_items WHERE run_id=?1 AND reason_code='external_removal'),retained_count=(SELECT count(*) FROM sandbox_reclaim_items WHERE run_id=?1 AND phase='retained'),row_version=row_version+1 WHERE run_id=?1",params![run_id.to_string(),timestamp(),stop_reason])?;
        Ok(())
    }
}

fn transition_quarantined_missing_root_tx(
    tx: &Transaction<'_>,
    c: &AbsentRootCandidate,
) -> Result<()> {
    let expected_generation = sql_generation(c.generation)?;
    let (sequence, error, canonical_repo_dir, sandbox_root, sandbox_branch): (
        i64,
        String,
        String,
        String,
        String,
    ) = tx
        .query_row(
            "SELECT event_sequence,validation_error_code,canonical_repo_dir,sandbox_root,sandbox_branch
               FROM sandbox_custody_roots
              WHERE custody_id=?1 AND state='quarantined' AND validation_state='invalid'
                AND validation_error_code='root_missing' AND owner_session_id IS NULL
                AND generation=?2 AND reserved_effects=0 AND active_effects=0",
            params![c.custody_id.to_string(), expected_generation],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?
        .ok_or_else(|| {
            DaemonError::Store("quarantined missing-root adoption fence lost".into())
        })?;
    let (linked, eligible, projections): (i64, i64, i64) = tx.query_row(
        "SELECT count(*),
                sum(CASE WHEN s.status IN ('Completed','Failed','Interrupted','Archived','Deleted')
                          AND s.pending_archive=0 AND s.sandbox_kind='GitWorktree'
                          AND s.working_dir=?2 AND s.sandbox_root=?3 AND s.sandbox_branch=?4
                          AND s.sandbox_cleanup_state='Failed' THEN 1 ELSE 0 END),
                (SELECT count(*) FROM session_execution_projections p
                  WHERE p.session_id IN (SELECT id FROM sessions WHERE sandbox_custody_id=?1))
           FROM sessions s WHERE s.sandbox_custody_id=?1",
        params![
            c.custody_id.to_string(),
            canonical_repo_dir,
            sandbox_root,
            sandbox_branch
        ],
        |r| {
            Ok((
                r.get(0)?,
                r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                r.get(2)?,
            ))
        },
    )?;
    if linked == 0 || eligible != linked || projections != linked {
        return Err(DaemonError::Store(
            "quarantined missing-root participants are incomplete or ineligible".into(),
        ));
    }
    let generation = c
        .generation
        .checked_add(1)
        .ok_or_else(|| DaemonError::Store("custody generation overflowed".into()))?;
    let generation = sql_generation(generation)?;
    let seq = sequence + 1;
    let now = timestamp();
    tx.execute("INSERT INTO sandbox_custody_events(event_id,custody_id,sequence,event_kind,cause,from_generation,to_generation,from_owner_session_id,to_owner_session_id,origin_session_id,scheduled_job_id,prior_state,next_state,error_code,occurred_at) VALUES(?1,?2,?3,'tombstoned','purge',?4,?5,NULL,NULL,NULL,NULL,'quarantined','purged',NULL,?6)",params![Uuid::new_v4().to_string(),c.custody_id.to_string(),seq,expected_generation,generation,now])?;
    if tx.execute("UPDATE sandbox_custody_roots SET state='purged',generation=?2,event_sequence=?3,validated_generation=?2,validated_at=?4,validation_error_code=?5,tombstoned_at=?4,updated_at=?4 WHERE custody_id=?1 AND state='quarantined' AND owner_session_id IS NULL AND generation=?6",params![c.custody_id.to_string(),generation,seq,now,error,expected_generation])?!=1{return Err(DaemonError::Store("quarantined adoption compare-and-swap lost".into()));}
    if tx.execute("UPDATE sessions SET sandbox_cleanup_state='Purged',sandbox_root=NULL,sandbox_branch=NULL,updated_at=?2 WHERE sandbox_custody_id=?1",params![c.custody_id.to_string(),now])? != sql_count(linked)? {
        return Err(DaemonError::Store("quarantined adoption session settlement incomplete".into()));
    }
    if tx.execute("UPDATE session_execution_projections SET execution_state='historical_purged',freshness='verified',effective_cwd=NULL,custody_id=?1,custody_generation=?2,validated_at=?3,error_code=NULL,updated_at=?3 WHERE session_id IN (SELECT id FROM sessions WHERE sandbox_custody_id=?1)",params![c.custody_id.to_string(),generation,now])? != sql_count(projections)? {
        return Err(DaemonError::Store("quarantined adoption projection settlement incomplete".into()));
    }
    Ok(())
}
