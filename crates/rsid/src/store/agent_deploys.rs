//! #1045 slice 2: durable state for `AgentRequestDeploy`.
//!
//! One row per deploy request. At most one row is live (`staged` or
//! `restarting`) at a time. Settlement CASes a live state to a terminal one and,
//! in the same transaction, inserts the owner's single resume wake, so a second
//! settle writes nothing.

use super::Store;
use super::scheduled_jobs::insert_scheduled_job_conn;
use crate::error::{DaemonError, Result};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use rsi_common::agent_deploy::{
    DEPLOY_IN_PROGRESS, DEPLOY_KEY_CONFLICT, DeployBinaryV1, DeployState,
};
use rsi_common::types::{Recurrence, ScheduleSpec, ScheduledJob, WakeMode};
use rusqlite::{OptionalExtension, Row, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use uuid::Uuid;

// The provisional number is assigned at landing.
// RSI-RELEASED-MIGRATION-BEGIN: agent-deploys-migration
/// Provisional schema version of the deploy catalog; the lander renumbers it.
pub(crate) const AGENT_DEPLOYS_SCHEMA_VERSION: i32 = 146;

const CATALOG: &str = "CREATE TABLE agent_deploys (
    id TEXT PRIMARY KEY CHECK(rsi_uuid_is_canonical(id)),
    owner_session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE RESTRICT CHECK(rsi_uuid_is_canonical(owner_session_id)),
    idempotency_digest TEXT NOT NULL CHECK(length(idempotency_digest)=64),
    request_fingerprint TEXT NOT NULL CHECK(length(request_fingerprint)=64),
    sha TEXT NOT NULL CHECK(length(sha)=40 AND sha=lower(sha) AND sha NOT GLOB '*[^0-9a-f]*'),
    manifest_json TEXT NOT NULL CHECK(json_valid(manifest_json)),
    state TEXT NOT NULL CHECK(state IN ('staged','restarting','succeeded','failed','timed_out')),
    reason TEXT CHECK(reason IS NULL OR length(reason) BETWEEN 1 AND 1024),
    wake_job_id TEXT UNIQUE CHECK(wake_job_id IS NULL OR rsi_uuid_is_canonical(wake_job_id)),
    created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at)),
    deadline_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(deadline_at)),
    restarted_at TEXT CHECK(restarted_at IS NULL OR rsi_rfc3339_nanos_is_canonical(restarted_at)),
    settled_at TEXT CHECK(settled_at IS NULL OR rsi_rfc3339_nanos_is_canonical(settled_at)),
    UNIQUE(owner_session_id, idempotency_digest),
    CHECK((state IN ('staged','restarting') AND settled_at IS NULL AND wake_job_id IS NULL)
       OR (state IN ('succeeded','failed','timed_out') AND settled_at IS NOT NULL AND wake_job_id IS NOT NULL))
);
CREATE UNIQUE INDEX agent_deploys_one_live ON agent_deploys((1)) WHERE state IN ('staged','restarting');
CREATE TRIGGER agent_deploys_no_delete BEFORE DELETE ON agent_deploys BEGIN SELECT RAISE(ABORT,'agent deploys are append only'); END;
CREATE TRIGGER agent_deploys_terminal_immutable BEFORE UPDATE ON agent_deploys WHEN OLD.state IN ('succeeded','failed','timed_out') BEGIN SELECT RAISE(ABORT,'settled agent deploys are immutable'); END;";

/// Catalog objects, for the fixture rewind and presence assertions.
#[cfg(test)]
pub(crate) const CATALOG_OBJECTS: [(&str, &str); 4] = [
    ("table", "agent_deploys"),
    ("index", "agent_deploys_one_live"),
    ("trigger", "agent_deploys_no_delete"),
    ("trigger", "agent_deploys_terminal_immutable"),
];

pub(crate) fn apply_migration(store: &Store, version: i32) -> Result<()> {
    let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)?;
    let prior: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if prior != version - 1 {
        return Err(DaemonError::Store(format!(
            "agent deploys require V{}, found V{prior}",
            version - 1
        )));
    }
    tx.execute_batch(CATALOG)?;
    tx.pragma_update(None, "user_version", version)?;
    tx.commit()?;
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: agent-deploys-migration

fn stamp(now: DateTime<Utc>) -> String {
    now.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn digest(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

/// Fingerprint of the request fields a replay must match.
#[must_use]
pub(crate) fn deploy_fingerprint(sha: &str, source_dir: &str, max_wait_secs: u32) -> String {
    digest(&[sha, source_dir, &max_wait_secs.to_string()])
}

/// A stored deploy.
#[derive(Debug, Clone)]
pub(crate) struct DeployRow {
    pub(crate) id: Uuid,
    pub(crate) owner_session_id: Uuid,
    pub(crate) sha: String,
    pub(crate) manifest: Vec<DeployBinaryV1>,
    pub(crate) state: DeployState,
    pub(crate) reason: Option<String>,
    pub(crate) deadline_at: DateTime<Utc>,
}

const COLUMNS: &str = "id, owner_session_id, sha, manifest_json, state, reason, deadline_at";

fn map_row(row: &Row<'_>) -> rusqlite::Result<DeployRow> {
    let bad = |what: &str| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            what.to_string().into(),
        )
    };
    let text = |index: usize| row.get::<_, String>(index);
    let manifest: Vec<DeployBinaryV1> =
        serde_json::from_str(&text(3)?).map_err(|_| bad("manifest"))?;
    Ok(DeployRow {
        id: Uuid::parse_str(&text(0)?).map_err(|_| bad("id"))?,
        owner_session_id: Uuid::parse_str(&text(1)?).map_err(|_| bad("owner"))?,
        sha: text(2)?,
        manifest,
        state: DeployState::parse(&text(4)?).ok_or_else(|| bad("state"))?,
        reason: row.get(5)?,
        deadline_at: DateTime::parse_from_rfc3339(&text(6)?)
            .map_err(|_| bad("deadline"))?
            .with_timezone(&Utc),
    })
}

/// Inputs for [`Store::insert_agent_deploy`].
pub(crate) struct NewDeploy<'a> {
    pub(crate) id: Uuid,
    pub(crate) owner_session_id: Uuid,
    pub(crate) idempotency_key: &'a str,
    pub(crate) sha: &'a str,
    pub(crate) fingerprint: String,
    pub(crate) manifest: &'a [DeployBinaryV1],
    pub(crate) max_wait_secs: u32,
}

impl Store {
    /// The existing row for `(owner, key)`, for an idempotent replay. A key
    /// reused with a different request is `deploy_idempotency_key_conflict`.
    pub(crate) fn replay_agent_deploy(
        &self,
        owner: Uuid,
        key: &str,
        fingerprint: &str,
    ) -> Result<Option<DeployRow>> {
        let existing = self
            .conn
            .query_row(
                &format!(
                    "SELECT {COLUMNS}, request_fingerprint FROM agent_deploys \
                     WHERE owner_session_id=?1 AND idempotency_digest=?2"
                ),
                params![owner.to_string(), digest(&[key])],
                |row| Ok((map_row(row)?, row.get::<_, String>(7)?)),
            )
            .optional()?;
        match existing {
            Some((_, stored)) if stored != fingerprint => {
                Err(DaemonError::InvalidParam(DEPLOY_KEY_CONFLICT.into()))
            }
            Some((row, _)) => Ok(Some(row)),
            None => Ok(None),
        }
    }

    /// Insert a `staged` deploy. `deploy_already_in_progress` when another
    /// deploy is live (the one-live unique index).
    pub(crate) fn insert_agent_deploy(
        &self,
        new: &NewDeploy<'_>,
        now: DateTime<Utc>,
    ) -> Result<DeployRow> {
        let manifest = serde_json::to_string(new.manifest)
            .map_err(|error| DaemonError::Store(error.to_string()))?;
        let deadline = now + Duration::seconds(i64::from(new.max_wait_secs));
        let inserted = self.conn.execute(
            "INSERT INTO agent_deploys (id, owner_session_id, idempotency_digest, \
             request_fingerprint, sha, manifest_json, state, created_at, deadline_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'staged', ?7, ?8)",
            params![
                new.id.to_string(),
                new.owner_session_id.to_string(),
                digest(&[new.idempotency_key]),
                new.fingerprint,
                new.sha,
                manifest,
                stamp(now),
                stamp(deadline),
            ],
        );
        match inserted {
            Ok(_) => {}
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::ConstraintViolation
                    && self.live_agent_deploy()?.is_some() =>
            {
                return Err(DaemonError::InvalidParam(DEPLOY_IN_PROGRESS.into()));
            }
            Err(error) => return Err(error.into()),
        }
        self.get_agent_deploy(new.id)?
            .ok_or_else(|| DaemonError::Store("deploy vanished after insert".into()))
    }

    pub(crate) fn get_agent_deploy(&self, id: Uuid) -> Result<Option<DeployRow>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM agent_deploys WHERE id=?1"),
                [id.to_string()],
                map_row,
            )
            .optional()?)
    }

    /// The single live (`staged` or `restarting`) deploy, if any.
    pub(crate) fn live_agent_deploy(&self) -> Result<Option<DeployRow>> {
        Ok(self
            .conn
            .query_row(
                &format!(
                    "SELECT {COLUMNS} FROM agent_deploys \
                     WHERE state IN ('staged','restarting') LIMIT 1"
                ),
                [],
                map_row,
            )
            .optional()?)
    }

    /// The newest deploy of any state (satellite health, #1017 slice 2).
    pub(crate) fn latest_agent_deploy(&self) -> Result<Option<DeployRow>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM agent_deploys ORDER BY created_at DESC LIMIT 1"),
                [],
                map_row,
            )
            .optional()?)
    }

    /// Deploys that reached a restart inside the window (budget guard).
    pub(crate) fn count_agent_deploy_restarts_since(&self, since: DateTime<Utc>) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM agent_deploys WHERE restarted_at IS NOT NULL AND restarted_at>=?1",
            [stamp(since)],
            |row| row.get(0),
        )?)
    }

    /// CAS `staged -> restarting`. False when the row was not `staged`.
    pub(crate) fn mark_agent_deploy_restarting(
        &self,
        id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE agent_deploys SET state='restarting', restarted_at=?2 \
             WHERE id=?1 AND state='staged'",
            params![id.to_string(), stamp(now)],
        )? == 1)
    }

    /// CAS a live row to `terminal` and insert the owner's single wake in the
    /// same transaction. `None` (nothing written) when the row was already
    /// settled.
    pub(crate) fn settle_agent_deploy(
        &self,
        id: Uuid,
        terminal: DeployState,
        reason: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<Option<Uuid>> {
        if !terminal.is_terminal() {
            return Err(DaemonError::InvalidParam(
                "deploy_settle_state_not_terminal".into(),
            ));
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let Some(row) = tx
            .query_row(
                &format!(
                    "SELECT {COLUMNS} FROM agent_deploys \
                     WHERE id=?1 AND state IN ('staged','restarting')"
                ),
                [id.to_string()],
                map_row,
            )
            .optional()?
        else {
            return Ok(None);
        };
        let wake_id = Uuid::new_v4();
        let reason = reason.map(|text| text.chars().take(1024).collect::<String>());
        let changed = tx.execute(
            "UPDATE agent_deploys SET state=?2, reason=?3, wake_job_id=?4, settled_at=?5 \
             WHERE id=?1 AND state IN ('staged','restarting')",
            params![
                id.to_string(),
                terminal.as_str(),
                reason,
                wake_id.to_string(),
                stamp(now)
            ],
        )?;
        if changed != 1 {
            return Ok(None);
        }
        let message = format!(
            "Deploy {id} {state}: sha {sha}{reason}. Check AgentGetDaemonInfo for the running build.",
            state = terminal.as_str(),
            sha = row.sha,
            reason = reason
                .as_deref()
                .map(|r| format!(" ({r})"))
                .unwrap_or_default(),
        );
        insert_scheduled_job_conn(
            &tx,
            &ScheduledJob {
                id: wake_id,
                name: format!("deploy-{id}"),
                message,
                schedule: ScheduleSpec {
                    recurrence: Recurrence::Once,
                    anchor: now,
                },
                last_fired_at: None,
                next_fire_at: now,
                enabled: true,
                working_dir: None,
                provider: None,
                model: None,
                project_id: None,
                created_at: now,
                updated_at: now,
                wake_mode: WakeMode::Resume,
                wake_session_id: Some(row.owner_session_id),
            },
        )?;
        tx.commit()?;
        Ok(Some(wake_id))
    }

    /// Quiet-point blockers, in a stable order; empty means quiet. Idle and
    /// `WaitingApproval` sessions do not block; the caller's own session and
    /// unscoped (parentless) sessions are not workers.
    pub(crate) fn deploy_quiet_blockers(&self, caller: Uuid) -> Result<Vec<&'static str>> {
        // Static SQL only: the p2_07 protected-writer audit (#392) must be able
        // to prove these reads write nothing.
        let mut blockers = Vec::new();
        let landing: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM rolling_queue_entries WHERE state IN ('admitted','gating'))",
            [],
            |row| row.get(0),
        )?;
        if landing {
            blockers.push("landing_in_progress");
        }
        let job: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM agent_jobs WHERE state='running')",
            [],
            |row| row.get(0),
        )?;
        if job {
            blockers.push("job_running");
        }
        let worker: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE status IN ('Starting','Running') \
             AND parent_id IS NOT NULL AND id!=?1 \
             AND COALESCE(session_kind,'') NOT IN ('Group','Epic'))",
            [caller.to_string()],
            |row| row.get(0),
        )?;
        if worker {
            blockers.push("worker_mid_turn");
        }
        Ok(blockers)
    }
}
