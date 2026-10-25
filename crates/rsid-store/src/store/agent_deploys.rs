//! #1045 slice 2: durable state for `AgentRequestDeploy`.
//!
//! One row per deploy request. At most one row is live (`staged` or
//! `restarting`) at a time. Settlement CASes a live state to a terminal one and,
//! in the same transaction, inserts the owner's single resume wake, so a second
//! settle writes nothing.

use super::Store;
use super::satellite_scope::DisplacedSeat;
use super::scheduled_jobs::insert_scheduled_job_conn;
use crate::error::{DaemonError, Result};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use rsi_common::agent_deploy::{
    DEPLOY_BLOCKER_SUBJECTS_MAX, DEPLOY_CANCEL_TOO_LATE, DEPLOY_CANCELLED, DEPLOY_IN_PROGRESS,
    DEPLOY_KEY_CONFLICT, DEPLOY_NOT_FOUND, DeployBinaryV1, DeployBlockerSubjectV1, DeployBlockerV1,
    DeployState, skipped_binaries,
};
use rsi_common::types::{Recurrence, ScheduleSpec, ScheduledJob, WakeMode};
use rusqlite::{OptionalExtension, Row, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use uuid::Uuid;

// The provisional number is assigned at landing.
#[cfg(test)]
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

pub(crate) fn deploy_digest(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

/// Fingerprint of the request fields a replay must match.
#[must_use]
pub fn deploy_fingerprint(sha: &str, source_dir: &str, max_wait_secs: u32) -> String {
    deploy_fingerprint_with(sha, source_dir, max_wait_secs, false)
}

/// [`deploy_fingerprint`] including `interrupt_workers` (#1461). A request that
/// does not interrupt workers keeps the fingerprint it always had, so rows
/// stored before the option existed still replay.
#[must_use]
pub fn deploy_fingerprint_with(
    sha: &str,
    source_dir: &str,
    max_wait_secs: u32,
    interrupt_workers: bool,
) -> String {
    let wait = max_wait_secs.to_string();
    if interrupt_workers {
        deploy_digest(&[sha, source_dir, &wait, "interrupt_workers"])
    } else {
        deploy_digest(&[sha, source_dir, &wait])
    }
}

/// #1461: what a deploy row records about interrupting workers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeployInterrupt {
    /// The request accepted interrupting workers mid-turn after the drain hold.
    pub requested: bool,
    /// Worker sessions still mid-turn when the restart began; empty until then.
    pub interrupted: Vec<Uuid>,
    /// #2048: jobs this deploy stopped as `interrupted_by_deploy`, oldest first.
    pub cancelled_jobs: Vec<Uuid>,
}

/// A stored deploy.
#[derive(Debug, Clone)]
pub struct DeployRow {
    pub id: Uuid,
    /// `None` for an operator-requested restart (#1122).
    pub owner_session_id: Option<Uuid>,
    pub sha: String,
    pub manifest: Vec<DeployBinaryV1>,
    pub state: DeployState,
    pub reason: Option<String>,
    pub deadline_at: DateTime<Utc>,
    pub operator: bool,
    /// The operator pressed "restart now": the quiet gate is skipped.
    pub forced: bool,
}

pub(crate) const COLUMNS: &str =
    "id, owner_session_id, sha, manifest_json, state, reason, deadline_at, origin, forced";

pub(crate) fn map_deploy_row(row: &Row<'_>) -> rusqlite::Result<DeployRow> {
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
        owner_session_id: row
            .get::<_, Option<String>>(1)?
            .map(|owner| Uuid::parse_str(&owner).map_err(|_| bad("owner")))
            .transpose()?,
        sha: text(2)?,
        manifest,
        state: DeployState::parse(&text(4)?).ok_or_else(|| bad("state"))?,
        reason: row.get(5)?,
        deadline_at: DateTime::parse_from_rfc3339(&text(6)?)
            .map_err(|_| bad("deadline"))?
            .with_timezone(&Utc),
        operator: text(7)? == "operator",
        forced: row.get::<_, i64>(8)? != 0,
    })
}

/// Inputs for `Store::insert_operator_deploy` (#1122).
pub struct NewOperatorDeploy<'a> {
    pub id: Uuid,
    pub sha: &'a str,
    pub manifest: &'a [DeployBinaryV1],
    pub max_wait_secs: u32,
    pub forced: bool,
}

/// Inputs for [`Store::insert_agent_deploy`].
pub struct NewDeploy<'a> {
    pub id: Uuid,
    pub owner_session_id: Uuid,
    pub idempotency_key: &'a str,
    pub sha: &'a str,
    pub fingerprint: String,
    pub manifest: &'a [DeployBinaryV1],
    pub max_wait_secs: u32,
    /// #1461: workers mid-turn stop blocking once the drain hold is over.
    pub interrupt_workers: bool,
}

/// The idempotency digest of a deploy request: owner-keyed (hub deploys) or
/// scoped to a declared satellite scope root (#1131), so a replay is stable
/// across the root's rotation while the row's owner is the settling tip.
fn request_digest(scope: Option<Uuid>, key: &str) -> String {
    match scope {
        Some(root) => deploy_digest(&[&root.to_string(), key]),
        None => deploy_digest(&[key]),
    }
}

impl Store {
    /// The existing row for `(owner, key)`, for an idempotent replay. A key
    /// reused with a different request is `deploy_idempotency_key_conflict`.
    pub fn replay_agent_deploy(
        &self,
        owner: Uuid,
        key: &str,
        fingerprint: &str,
    ) -> Result<Option<DeployRow>> {
        self.replay_agent_deploy_by(Some(owner), &request_digest(None, key), fingerprint)
    }

    /// #1131: the existing row recorded under a declared scope root, whoever
    /// owns it now (the root's rotation tip owns the settlement, the root keeps
    /// the replay identity). The first matching scope wins.
    pub fn replay_agent_deploy_scoped(
        &self,
        scopes: &[Uuid],
        key: &str,
        fingerprint: &str,
    ) -> Result<Option<DeployRow>> {
        for scope in scopes {
            let found =
                self.replay_agent_deploy_by(None, &request_digest(Some(*scope), key), fingerprint)?;
            if found.is_some() {
                return Ok(found);
            }
        }
        Ok(None)
    }

    fn replay_agent_deploy_by(
        &self,
        owner: Option<Uuid>,
        key_digest: &str,
        fingerprint: &str,
    ) -> Result<Option<DeployRow>> {
        let existing = self
            .conn
            .query_row(
                &format!(
                    "SELECT {COLUMNS}, request_fingerprint FROM agent_deploys \
                     WHERE (?1 IS NULL OR request_session_id=?1 OR owner_session_id=?1) \
                     AND idempotency_digest=?2 \
                     ORDER BY (request_session_id IS ?1) DESC, created_at LIMIT 1"
                ),
                params![owner.map(|id| id.to_string()), key_digest],
                |row| Ok((map_deploy_row(row)?, row.get::<_, String>(9)?)),
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
    pub fn insert_agent_deploy(
        &self,
        new: &NewDeploy<'_>,
        now: DateTime<Utc>,
    ) -> Result<DeployRow> {
        self.insert_agent_deploy_scoped(new, None, now)
    }

    /// Same, with the replay identity scoped to a declared satellite scope root
    /// (#1131); `owner_session_id` stays the session that settles the deploy.
    pub fn insert_agent_deploy_scoped(
        &self,
        new: &NewDeploy<'_>,
        scope: Option<Uuid>,
        now: DateTime<Utc>,
    ) -> Result<DeployRow> {
        let manifest = serde_json::to_string(new.manifest)
            .map_err(|error| DaemonError::Store(error.to_string()))?;
        let deadline = now + Duration::seconds(i64::from(new.max_wait_secs));
        let inserted = self.conn.execute(
            "INSERT INTO agent_deploys (id, owner_session_id, idempotency_digest, \
             request_fingerprint, sha, manifest_json, state, created_at, deadline_at, \
             interrupt_workers, request_session_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'staged', ?7, ?8, ?9, ?2)",
            params![
                new.id.to_string(),
                new.owner_session_id.to_string(),
                request_digest(scope, new.idempotency_key),
                new.fingerprint,
                new.sha,
                manifest,
                stamp(now),
                stamp(deadline),
                i64::from(new.interrupt_workers),
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

    pub fn get_agent_deploy(&self, id: Uuid) -> Result<Option<DeployRow>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM agent_deploys WHERE id=?1"),
                [id.to_string()],
                map_deploy_row,
            )
            .optional()?)
    }

    /// The single live (`staged` or `restarting`) deploy, if any.
    pub fn live_agent_deploy(&self) -> Result<Option<DeployRow>> {
        Ok(self
            .conn
            .query_row(
                &format!(
                    "SELECT {COLUMNS} FROM agent_deploys \
                     WHERE state IN ('staged','restarting') LIMIT 1"
                ),
                [],
                map_deploy_row,
            )
            .optional()?)
    }

    /// The commits of the newest succeeded deploys, newest first and without
    /// repeats (a redeploy of one commit counts once), at most `limit`. The
    /// staging prune keeps the deployed build and the one before it (#2681).
    pub fn recent_succeeded_deploy_shas(&self, limit: usize) -> Result<Vec<String>> {
        // Distinct commits first, then the limit: a commit redeployed many
        // times must not crowd the previous commit out of the window (#2701).
        let mut stmt = self.conn.prepare(
            "SELECT sha FROM agent_deploys WHERE state = 'succeeded' \
             GROUP BY sha ORDER BY MAX(settled_at) DESC, MAX(created_at) DESC LIMIT ?1",
        )?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = stmt.query_map([limit], |row| row.get::<_, String>(0))?;
        rows.collect::<std::result::Result<Vec<String>, _>>()
            .map_err(Into::into)
    }

    /// The newest deploy of any state (satellite health, #1017 slice 2).
    pub fn latest_agent_deploy(&self) -> Result<Option<DeployRow>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM agent_deploys ORDER BY created_at DESC LIMIT 1"),
                [],
                map_deploy_row,
            )
            .optional()?)
    }

    /// Deploys that reached a restart inside the window (budget guard).
    pub fn count_agent_deploy_restarts_since(&self, since: DateTime<Utc>) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM agent_deploys WHERE restarted_at IS NOT NULL AND restarted_at>=?1",
            [stamp(since)],
            |row| row.get(0),
        )?)
    }

    /// Restart times inside the window, oldest first (budget guard and the
    /// `retry_after` of a refusal, #1777).
    pub fn agent_deploy_restart_times_since(
        &self,
        since: DateTime<Utc>,
    ) -> Result<Vec<DateTime<Utc>>> {
        let mut statement = self.conn.prepare(
            "SELECT restarted_at FROM agent_deploys \
             WHERE restarted_at IS NOT NULL AND restarted_at>=?1 ORDER BY restarted_at",
        )?;
        let rows = statement.query_map([stamp(since)], |row| row.get::<_, String>(0))?;
        let mut times = Vec::new();
        for row in rows {
            let text = row?;
            times.push(
                DateTime::parse_from_rfc3339(&text)
                    .map_err(|error| DaemonError::Store(format!("restarted_at {text}: {error}")))?
                    .with_timezone(&Utc),
            );
        }
        Ok(times)
    }

    /// CAS `staged -> restarting`. False when the row was not `staged`.
    pub fn mark_agent_deploy_restarting(&self, id: Uuid, now: DateTime<Utc>) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE agent_deploys SET state='restarting', restarted_at=?2 \
             WHERE id=?1 AND state='staged'",
            params![id.to_string(), stamp(now)],
        )? == 1)
    }

    /// #1461: the deploy's interrupt-workers request and, once the restart
    /// began, the worker sessions it cut off. A deploy that is not found reads
    /// as no request.
    pub fn agent_deploy_interrupt(&self, id: Uuid) -> Result<DeployInterrupt> {
        let found: Option<(i64, Option<String>)> = self
            .conn
            .query_row(
                "SELECT interrupt_workers, interrupted_sessions FROM agent_deploys WHERE id=?1",
                [id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((requested, interrupted)) = found else {
            return Ok(DeployInterrupt::default());
        };
        Ok(DeployInterrupt {
            requested: requested != 0,
            interrupted: interrupted
                .map(|text| parse_interrupted(&text))
                .transpose()?
                .unwrap_or_default(),
            cancelled_jobs: self.deploy_cancelled_jobs(id)?,
        })
    }

    /// #2048: the jobs deploy `id` stopped: those settled `interrupted_by_deploy`
    /// (linked through the `deploy_id` in the settled result) and, #2278, those it
    /// stopped and put back in the queue (linked through `requeued_by_deploy`).
    pub fn deploy_cancelled_jobs(&self, id: Uuid) -> Result<Vec<Uuid>> {
        let mut statement = self.conn.prepare(
            "SELECT id FROM agent_jobs WHERE (state='failed' \
             AND json_extract(result_json,'$.deploy_id')=?1) \
             OR json_extract(params_json,'$.requeued_by_deploy')=?1 ORDER BY sequence",
        )?;
        let ids = statement
            .query_map([id.to_string()], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.iter()
            .map(|id| {
                Uuid::parse_str(id)
                    .map_err(|error| DaemonError::Store(format!("cancelled job id: {error}")))
            })
            .collect()
    }

    /// #1461: record the worker sessions this deploy's restart interrupts, once
    /// and only while it is `restarting`. False when nothing was written.
    pub fn record_agent_deploy_interrupted(&self, id: Uuid, workers: &[Uuid]) -> Result<bool> {
        let ids: Vec<String> = workers.iter().map(Uuid::to_string).collect();
        let json =
            serde_json::to_string(&ids).map_err(|error| DaemonError::Store(error.to_string()))?;
        Ok(self.conn.execute(
            "UPDATE agent_deploys SET interrupted_sessions=?2 \
             WHERE id=?1 AND state='restarting' AND interrupted_sessions IS NULL",
            params![id.to_string(), json],
        )? == 1)
    }

    /// #2371: the worker sessions the live `restarting` deploy named as
    /// interrupted. Empty unless an `interrupt_workers` deploy is restarting and
    /// has recorded its set; the DRAIN wait stops waiting for exactly these.
    pub fn restarting_deploy_interrupted_workers(&self) -> Result<Vec<Uuid>> {
        let texts: Vec<String> = self
            .conn
            .prepare(
                "SELECT interrupted_sessions FROM agent_deploys \
                 WHERE state='restarting' AND interrupt_workers=1 \
                 AND interrupted_sessions IS NOT NULL",
            )?
            .query_map([], |row| row.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        let mut workers = Vec::new();
        for text in texts {
            workers.extend(parse_interrupted(&text)?);
        }
        Ok(workers)
    }

    /// When the deploy settled (`None` while it is staged or restarting).
    pub fn agent_deploy_settled_at(&self, id: Uuid) -> Result<Option<DateTime<Utc>>> {
        let text: Option<String> = self
            .conn
            .query_row(
                "SELECT settled_at FROM agent_deploys WHERE id=?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        text.map(|text| {
            DateTime::parse_from_rfc3339(&text)
                .map(|stamp| stamp.with_timezone(&Utc))
                .map_err(|error| DaemonError::Store(format!("deploy settled_at: {error}")))
        })
        .transpose()
    }

    /// When the deploy was requested: the start of its launch hold (#1320).
    pub fn agent_deploy_created_at(&self, id: Uuid) -> Result<Option<DateTime<Utc>>> {
        let text: Option<String> = self
            .conn
            .query_row(
                "SELECT created_at FROM agent_deploys WHERE id=?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        text.map(|text| {
            DateTime::parse_from_rfc3339(&text)
                .map(|stamp| stamp.with_timezone(&Utc))
                .map_err(|error| DaemonError::Store(format!("deploy created_at: {error}")))
        })
        .transpose()
    }

    /// #1320/#1311: the owner cancels its own waiting deploy, named by the
    /// idempotency key (and sha) it was requested with. A `staged` deploy
    /// settles `failed` with `deploy_cancelled`; its outcome wake is recorded
    /// disabled, because the caller already holds the answer. An already
    /// settled deploy is returned as it is (an idempotent replay).
    ///
    /// # Errors
    /// `deploy_not_found` (no such deploy of this owner, or another sha),
    /// `deploy_cancel_too_late` (the restart began) or a persistence error.
    pub fn cancel_agent_deploy(
        &self,
        owner: Uuid,
        key: &str,
        sha: &str,
        now: DateTime<Utc>,
    ) -> Result<DeployRow> {
        self.cancel_agent_deploy_with_policy(
            Some(owner),
            &request_digest(None, key),
            sha,
            now,
            false,
        )
    }

    /// Peer cancellation is a one-shot remote action: once the satellite row
    /// is no longer waiting, return the same stable refusal as a restart that
    /// has begun instead of replaying a settled receipt.
    pub fn cancel_agent_deploy_if_waiting(
        &self,
        owner: Uuid,
        key: &str,
        sha: &str,
        now: DateTime<Utc>,
    ) -> Result<DeployRow> {
        self.cancel_agent_deploy_with_policy(
            Some(owner),
            &request_digest(None, key),
            sha,
            now,
            true,
        )
    }

    /// Satellite deploys use the declared scope root as part of their replay
    /// namespace. A linked cancellation looks the row up exactly as the scoped
    /// replay does (#2000): by the digest under any declared scope root, whoever
    /// owns it now, because the seat that staged the deploy may have rotated (or
    /// the first live root changed) while it waited. An operator deploy has no
    /// scoped digest, so it can never match.
    pub fn cancel_scoped_agent_deploy_if_waiting(
        &self,
        scopes: &[Uuid],
        key: &str,
        sha: &str,
        now: DateTime<Utc>,
    ) -> Result<DeployRow> {
        for scope in scopes {
            match self.cancel_agent_deploy_with_policy(
                None,
                &request_digest(Some(*scope), key),
                sha,
                now,
                true,
            ) {
                Err(DaemonError::InvalidParam(code)) if code == DEPLOY_NOT_FOUND => {}
                other => return other,
            }
        }
        Err(DaemonError::InvalidParam(DEPLOY_NOT_FOUND.into()))
    }

    fn cancel_agent_deploy_with_policy(
        &self,
        owner: Option<Uuid>,
        digest: &str,
        sha: &str,
        now: DateTime<Utc>,
        require_waiting: bool,
    ) -> Result<DeployRow> {
        let found = self
            .conn
            .query_row(
                &format!(
                    "SELECT {COLUMNS} FROM agent_deploys \
                     WHERE (?1 IS NULL OR request_session_id=?1 OR owner_session_id=?1) \
                     AND idempotency_digest=?2 AND sha=?3 \
                     ORDER BY (state IN ('staged','restarting')) DESC, \
                              (request_session_id IS ?1) DESC, created_at LIMIT 1"
                ),
                params![owner.map(|id| id.to_string()), digest, sha],
                map_deploy_row,
            )
            .optional()?;
        let row = found.ok_or_else(|| DaemonError::InvalidParam(DEPLOY_NOT_FOUND.into()))?;
        match row.state {
            DeployState::Staged => {}
            DeployState::Restarting => {
                return Err(DaemonError::PolicyDenied(DEPLOY_CANCEL_TOO_LATE.into()));
            }
            _ if !require_waiting => return Ok(row),
            _ => return Err(DaemonError::PolicyDenied(DEPLOY_CANCEL_TOO_LATE.into())),
        }
        self.settle_agent_deploy_waking(
            row.id,
            DeployState::Failed,
            Some(DEPLOY_CANCELLED),
            now,
            false,
        )?;
        self.get_agent_deploy(row.id)?
            .ok_or_else(|| DaemonError::Store("deploy vanished after cancel".into()))
    }

    /// CAS a live row to `terminal` and insert the owner's single wake in the
    /// same transaction. `None` (nothing written) when the row was already
    /// settled.
    pub fn settle_agent_deploy(
        &self,
        id: Uuid,
        terminal: DeployState,
        reason: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<Option<Uuid>> {
        self.settle_agent_deploy_waking(id, terminal, reason, now, true)
    }

    /// [`Self::settle_agent_deploy`]; `wake == false` records the owner's
    /// wake row disabled (an owner-requested cancel needs no resume).
    fn settle_agent_deploy_waking(
        &self,
        id: Uuid,
        terminal: DeployState,
        reason: Option<&str>,
        now: DateTime<Utc>,
        wake: bool,
    ) -> Result<Option<Uuid>> {
        if !terminal.is_terminal() {
            return Err(DaemonError::InvalidParam(
                "deploy_settle_state_not_terminal".into(),
            ));
        }
        // #272: the authority projection reads under its own snapshot, so the
        // wake's recipient is resolved before the settle transaction opens.
        let recipient = match self
            .get_agent_deploy(id)?
            .and_then(|row| row.owner_session_id)
        {
            Some(owner) => Some((owner, self.deploy_wake_recipient(owner)?)),
            None => None,
        };
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let Some(row) = tx
            .query_row(
                &format!(
                    "SELECT {COLUMNS} FROM agent_deploys \
                     WHERE id=?1 AND state IN ('staged','restarting')"
                ),
                [id.to_string()],
                map_deploy_row,
            )
            .optional()?
        else {
            return Ok(None);
        };
        let wake_id = Uuid::new_v4();
        let reason = reason.map(|text| text.chars().take(1024).collect::<String>());
        let Some(owner) = row.owner_session_id else {
            // Operator deploy (#1122): nobody to wake; the row is the record.
            let changed = tx.execute(
                "UPDATE agent_deploys SET state=?2, reason=?3, settled_at=?4 \
                 WHERE id=?1 AND state IN ('staged','restarting')",
                params![id.to_string(), terminal.as_str(), reason, stamp(now)],
            )?;
            tx.commit()?;
            return Ok((changed == 1).then_some(wake_id));
        };
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
        let skipped = skipped_binaries(&row.manifest);
        let skipped = if skipped.is_empty() {
            String::new()
        } else {
            format!(" Skipped (not in binaries_dir): {}.", skipped.join(", "))
        };
        let interrupted = format!(
            "{}{}",
            interrupted_note(&tx, id)?,
            cancelled_jobs_note(&tx, id)?
        );
        let message = format!(
            "Deploy {id} {state}: sha {sha}{reason}.{skipped}{interrupted} Check AgentGetDaemonInfo for the running build.",
            state = terminal.as_str(),
            sha = row.sha,
            reason = reason
                .as_deref()
                .map(|r| format!(" ({r})"))
                .unwrap_or_default(),
        );
        // A row whose owner changed since the read above (#1553 hands a live
        // deploy to a new seat) is woken as recorded.
        let recipient = recipient
            .filter(|(read, _)| *read == owner)
            .map_or(WakeRecipient::Owner, |(_, recipient)| recipient);
        let (wake_session, message, wake) = match recipient {
            WakeRecipient::Owner => (owner, message, wake),
            WakeRecipient::Seat(seat) => {
                tracing::info!(deploy = %id, owner = %owner, seat = %seat,
                    "deploy outcome wake redirected from a displaced seat to the current seat");
                (
                    seat,
                    format!(
                        "{message} Redirected (#272): deploy owner {owner} no longer holds its project's manager appointment; you are the current appointed seat."
                    ),
                    wake,
                )
            }
            WakeRecipient::NoSeat(project) => {
                tracing::warn!(deploy = %id, owner = %owner, project = %project,
                    "deploy outcome wake not delivered: displaced owner and no live appointed seat");
                (
                    owner,
                    format!(
                        "{message} Not delivered (#272): deploy owner {owner} no longer holds project {project}'s manager appointment and the project has no live appointed seat; the settled deploy row is the record."
                    ),
                    false,
                )
            }
        };
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
                enabled: wake,
                working_dir: None,
                provider: None,
                model: None,
                project_id: None,
                created_at: now,
                updated_at: now,
                wake_mode: WakeMode::Resume,
                wake_session_id: Some(wake_session),
            },
        )?;
        tx.commit()?;
        Ok(Some(wake_id))
    }

    /// #272: the session a deploy's outcome wake reaches. An owner that still
    /// holds manager authority (the project seat, a portfolio or node seat)
    /// keeps it. An owner another session replaced as its project's seat (a
    /// deposed manager still declared as a satellite scope root, whose deploy
    /// no re-appointment handed over) is not resumed: the project's current
    /// seat gets the wake, or, with no live seat, nobody does.
    fn deploy_wake_recipient(&self, owner: Uuid) -> Result<WakeRecipient> {
        let manager = self
            .agent_authority_projection(owner)
            .is_ok_and(|projection| projection.is_manager || projection.is_global_manager);
        if manager {
            return Ok(WakeRecipient::Owner);
        }
        Ok(match self.displaced_seat_route(owner)? {
            DisplacedSeat::NotDisplaced => WakeRecipient::Owner,
            DisplacedSeat::Reseated { seat, .. } => WakeRecipient::Seat(seat),
            DisplacedSeat::NoLiveSeat { project } => WakeRecipient::NoSeat(project),
        })
    }

    /// Quiet-point blockers, in a stable order; empty means quiet. A running
    /// `cloud_sweep` / `cloud_gate` job does not block (#1177): it runs in a
    /// transient `systemd-run --user` unit that is a sibling of rsid, so a
    /// restart does not kill it, and the job runner's startup poll reconciles
    /// it and still delivers the owner's resume wake. A `bench` job (#3181)
    /// never blocks either: its unit is a sibling of rsid that runs for hours,
    /// and the trials a restart interrupts are rerun once. Local `test`, `build` and
    /// `landing` jobs still block. Idle and
    /// `WaitingApproval` sessions do not block; the caller's own session and
    /// unscoped (parentless) sessions are not workers.
    pub fn deploy_quiet_blockers(&self, caller: Uuid) -> Result<Vec<&'static str>> {
        self.deploy_quiet_blockers_with(caller, true)
    }

    /// [`Self::deploy_quiet_blockers`] where `workers_block == false` leaves
    /// `worker_mid_turn` out (#1461, `interrupt_workers` past the drain hold).
    /// A landing and a running local job still block.
    pub fn deploy_quiet_blockers_with(
        &self,
        caller: Uuid,
        workers_block: bool,
    ) -> Result<Vec<&'static str>> {
        self.deploy_quiet_blockers_surviving(caller, workers_block, &[])
    }

    /// [`Self::deploy_quiet_blockers_with`] where the running jobs in `surviving`
    /// do not block (#3554): an `interrupt_workers` deploy lets ordinary test
    /// jobs run across its restart, so they are no reason to wait.
    pub fn deploy_quiet_blockers_surviving(
        &self,
        caller: Uuid,
        workers_block: bool,
        surviving: &[Uuid],
    ) -> Result<Vec<&'static str>> {
        Ok(self
            .quiet_blocker_subjects(caller, workers_block, surviving)?
            .into_iter()
            .map(|(kind, _, _)| kind)
            .collect())
    }

    /// #2056: [`Self::deploy_quiet_blockers_with`] with the ids that name each
    /// blocker (queue sequence and batch, job and owner, worker session). Both
    /// come from one query set, so the names and the details cannot disagree.
    pub fn deploy_quiet_blocker_details_with(
        &self,
        caller: Uuid,
        workers_block: bool,
    ) -> Result<Vec<DeployBlockerV1>> {
        self.deploy_quiet_blocker_details_surviving(caller, workers_block, &[])
    }

    /// [`Self::deploy_quiet_blocker_details_with`] without the `surviving` jobs.
    pub fn deploy_quiet_blocker_details_surviving(
        &self,
        caller: Uuid,
        workers_block: bool,
        surviving: &[Uuid],
    ) -> Result<Vec<DeployBlockerV1>> {
        Ok(self
            .quiet_blocker_subjects(caller, workers_block, surviving)?
            .into_iter()
            .map(|(kind, subjects, total)| DeployBlockerV1 {
                kind: kind.to_string(),
                subjects,
                subjects_total: total,
            })
            .collect())
    }

    #[allow(clippy::type_complexity)]
    fn quiet_blocker_subjects(
        &self,
        caller: Uuid,
        workers_block: bool,
        surviving: &[Uuid],
    ) -> Result<Vec<(&'static str, Vec<DeployBlockerSubjectV1>, u32)>> {
        // Static SQL only: the p2_07 protected-writer audit (#392) must be able
        // to prove these reads write nothing.
        const CAP: i64 = DEPLOY_BLOCKER_SUBJECTS_MAX as i64;
        let uuid = |value: String, what: &str| {
            Uuid::parse_str(&value)
                .map_err(|error| DaemonError::Store(format!("deploy blocker {what}: {error}")))
        };
        let mut blockers = Vec::new();
        let mut stmt = self.conn.prepare(
            "SELECT sequence, state, batch_id, enqueued_at, COUNT(*) OVER () \
             FROM rolling_queue_entries WHERE state IN ('admitted','gating') \
             ORDER BY sequence LIMIT ?1",
        )?;
        let mut total = 0_u32;
        let queue = stmt
            .query_map([CAP], |row| {
                total = u32::try_from(row.get::<_, i64>(4)?).unwrap_or(u32::MAX);
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if !queue.is_empty() {
            let subjects = queue
                .into_iter()
                .map(|(sequence, state, batch, since)| {
                    Ok(DeployBlockerSubjectV1 {
                        queue_sequence: Some(sequence),
                        queue_state: Some(state),
                        batch_id: batch.map(|batch| uuid(batch, "batch")).transpose()?,
                        since: Some(since),
                        ..DeployBlockerSubjectV1::default()
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            blockers.push(("landing_in_progress", subjects, total));
        }
        // The running jobs are few (a handful of units), so the survivors are
        // filtered here rather than in SQL, which must stay a static string.
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, owner_session_id, started_at \
             FROM agent_jobs WHERE state='running' \
             AND kind NOT IN ('cloud_sweep','cloud_gate','bench') ORDER BY sequence",
        )?;
        let surviving: Vec<String> = surviving.iter().map(Uuid::to_string).collect();
        let jobs = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|(id, ..)| !surviving.contains(id))
            .collect::<Vec<_>>();
        let total = u32::try_from(jobs.len()).unwrap_or(u32::MAX);
        if !jobs.is_empty() {
            let subjects = jobs
                .into_iter()
                .take(DEPLOY_BLOCKER_SUBJECTS_MAX)
                .map(|(id, kind, owner, since)| {
                    Ok(DeployBlockerSubjectV1 {
                        job_id: Some(uuid(id, "job")?),
                        job_kind: Some(kind),
                        owner_session_id: Some(uuid(owner, "job owner")?),
                        since,
                        ..DeployBlockerSubjectV1::default()
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            blockers.push(("job_running", subjects, total));
        }
        if workers_block {
            let workers = self.deploy_mid_turn_workers(caller)?;
            if !workers.is_empty() {
                let total = u32::try_from(workers.len()).unwrap_or(u32::MAX);
                let subjects = workers
                    .into_iter()
                    .take(DEPLOY_BLOCKER_SUBJECTS_MAX)
                    .map(|id| DeployBlockerSubjectV1 {
                        session_id: Some(id),
                        ..DeployBlockerSubjectV1::default()
                    })
                    .collect();
                blockers.push(("worker_mid_turn", subjects, total));
            }
        }
        Ok(blockers)
    }

    /// #3554: record that the running jobs `jobs` keep running across deploy
    /// `id`'s restart (an `interrupt_workers` deploy stops no ordinary test job:
    /// the new daemon re-adopts the unit and settles it once). The deploy is kept
    /// under `survived_deploy` in the job's params, like `requeued_by_deploy`
    /// (#2278), so no schema changes. Only a job still `running` is marked, and
    /// only while the deploy is `restarting`. Returns the ids actually marked.
    pub fn record_agent_deploy_surviving_jobs(&self, id: Uuid, jobs: &[Uuid]) -> Result<Vec<Uuid>> {
        let mut marked = Vec::new();
        for job in jobs {
            let changed = self.conn.execute(
                "UPDATE agent_jobs SET params_json=json_set(params_json,'$.survived_deploy',?2), \
                 row_version=row_version+1 WHERE id=?1 AND state='running' \
                 AND EXISTS(SELECT 1 FROM agent_deploys WHERE id=?2 AND state='restarting')",
                params![job.to_string(), id.to_string()],
            )?;
            if changed == 1 {
                marked.push(*job);
            }
        }
        Ok(marked)
    }

    /// #3554: the jobs deploy `id` let run across its restart, oldest first, as
    /// [`Self::record_agent_deploy_surviving_jobs`] marked them; a job that
    /// finished before the restart was never marked.
    pub fn deploy_surviving_jobs(&self, id: Uuid) -> Result<Vec<Uuid>> {
        surviving_jobs_conn(&self.conn, id)
    }

    /// #1461: the worker sessions the quiet gate counts as mid-turn (the
    /// `worker_mid_turn` blocker), in a stable order.
    pub fn deploy_mid_turn_workers(&self, caller: Uuid) -> Result<Vec<Uuid>> {
        // The same predicate as the `worker_mid_turn` blocker, as static SQL;
        // `workers_mid_turn_are_listed_and_can_stop_blocking` pins that they agree.
        let mut stmt = self.conn.prepare(
            "SELECT id FROM sessions WHERE status IN ('Starting','Running') \
             AND parent_id IS NOT NULL AND id!=?1 \
             AND COALESCE(session_kind,'') NOT IN ('Group','Epic') ORDER BY id",
        )?;
        let ids = stmt
            .query_map([caller.to_string()], |row| row.get::<_, String>(0))?
            .map(|id| {
                Uuid::parse_str(&id?)
                    .map_err(|error| DaemonError::Store(format!("worker session id: {error}")))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut workers = Vec::new();
        for id in ids {
            if self.adoptable_provider_turn_for_session(id)?.is_none() {
                workers.push(id);
            }
        }
        Ok(workers)
    }
}

/// #272: who a settling deploy's outcome wake resumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WakeRecipient {
    /// The recorded owner.
    Owner,
    /// The current appointed seat that replaced a displaced owner.
    Seat(Uuid),
    /// The owner was displaced and this project has no live seat: the wake is
    /// recorded disabled.
    NoSeat(Uuid),
}

/// The most interrupted worker ids a settle wake names; the row keeps them all.
const INTERRUPTED_NOTE_MAX: usize = 64;

fn parse_interrupted(text: &str) -> Result<Vec<Uuid>> {
    let bad = |what: String| DaemonError::Store(format!("interrupted sessions: {what}"));
    let ids: Vec<String> = serde_json::from_str(text).map_err(|error| bad(error.to_string()))?;
    ids.iter()
        .map(|id| Uuid::parse_str(id).map_err(|error| bad(error.to_string())))
        .collect()
}

/// #2549: a topology node session is named with its execution and node
/// (` [topology node implement of execution <id>]`), so the manager reads the
/// deploy outcome as "this execution's node was cut and resumes", not as a
/// stray worker. Empty for any other session or on a read error.
fn topology_node_label(tx: &Transaction<'_>, session: Uuid) -> String {
    tx.query_row(
        "SELECT node_id,execution_id FROM topology_node_attempts WHERE session_id=?1 \
         ORDER BY created_at DESC LIMIT 1",
        [session.to_string()],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    )
    .optional()
    .ok()
    .flatten()
    .map(|(node, execution)| format!(" [topology node {node} of execution {execution}]"))
    .unwrap_or_default()
}

/// #1461: the settle wake's account of the workers a restart interrupted
/// (empty when none were recorded). Read inside the settling transaction.
fn interrupted_note(tx: &Transaction<'_>, id: Uuid) -> Result<String> {
    let text: Option<String> = tx
        .query_row(
            "SELECT interrupted_sessions FROM agent_deploys WHERE id=?1",
            [id.to_string()],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    let workers = text.map(|text| parse_interrupted(&text)).transpose()?;
    let Some(workers) = workers.filter(|workers| !workers.is_empty()) else {
        return Ok(String::new());
    };
    let shown: Vec<String> = workers
        .iter()
        .take(INTERRUPTED_NOTE_MAX)
        .map(|worker| format!("{worker}{}", topology_node_label(tx, *worker)))
        .collect();
    let more = workers.len().saturating_sub(INTERRUPTED_NOTE_MAX);
    Ok(format!(
        " Workers still mid-turn at the restart (interrupted unless their turn ended inside the drain window; each is resumed with a continue prompt, #1461): {}{}.",
        shown.join(", "),
        if more > 0 {
            format!(" and {more} more")
        } else {
            String::new()
        }
    ))
}

/// #3554: the jobs deploy `id` marked as running across its restart.
fn surviving_jobs_conn(conn: &rusqlite::Connection, id: Uuid) -> Result<Vec<Uuid>> {
    let ids = conn
        .prepare(
            "SELECT id FROM agent_jobs \
             WHERE json_extract(params_json,'$.survived_deploy')=?1 ORDER BY sequence",
        )?
        .query_map([id.to_string()], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ids.iter()
        .map(|id| {
            Uuid::parse_str(id)
                .map_err(|error| DaemonError::Store(format!("surviving job id: {error}")))
        })
        .collect()
}

/// #2048: the settle wake's account of the verification jobs the deploy
/// stopped after its grace (empty when none), read in the settling transaction.
/// #2278: a stopped test job is requeued under its own id, so the note names the
/// requeued jobs apart from the ones that settled `interrupted_by_deploy`.
/// #3554: an ordinary test job is no longer stopped; the jobs that kept running
/// across the restart are named too, with how to read their wake.
fn cancelled_jobs_note(tx: &Transaction<'_>, id: Uuid) -> Result<String> {
    let stopped = tx
        .prepare(
            "SELECT id FROM agent_jobs WHERE state='failed' \
             AND json_extract(result_json,'$.deploy_id')=?1 ORDER BY sequence",
        )?
        .query_map([id.to_string()], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let requeued = tx
        .prepare(
            "SELECT id FROM agent_jobs \
             WHERE json_extract(params_json,'$.requeued_by_deploy')=?1 ORDER BY sequence",
        )?
        .query_map([id.to_string()], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let surviving = surviving_jobs_conn(tx, id)?;
    let mut note = String::new();
    if !surviving.is_empty() {
        let surviving: Vec<String> = surviving.iter().map(Uuid::to_string).collect();
        note.push_str(&format!(
            " Running test jobs were not stopped: their units kept running across the restart and the new daemon re-adopted them; your wake for each fires once, when the job ends, #3554: {}.",
            surviving.join(", ")
        ));
    }
    if !requeued.is_empty() {
        note.push_str(&format!(
            " Running test jobs stopped after the grace were requeued under the same id, owner and wake and rerun on the new daemon; your wake for each fires once, when the rerun ends, #2278: {}.",
            requeued.join(", ")
        ));
    }
    if !stopped.is_empty() {
        note.push_str(&format!(
            " Running test jobs stopped as interrupted_by_deploy after the grace (resubmit them once on the new daemon, #2048): {}.",
            stopped.join(", ")
        ));
    }
    Ok(note)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn session(store: &Store, status: &str, parent: Option<Uuid>, kind: &str) -> Uuid {
        let id = Uuid::new_v4();
        store
            .conn
            .execute(
                "INSERT INTO sessions (id, provider, query, working_dir, status, created_at, \
                 updated_at, session_kind, parent_id) VALUES (?1,'Claude','q','/tmp',?2,?3,?3,?4,?5)",
                params![
                    id.to_string(),
                    status,
                    stamp(Utc::now()),
                    kind,
                    parent.map(|p| p.to_string()),
                ],
            )
            .unwrap();
        id
    }

    fn staged(store: &Store, owner: Uuid, key: &str, interrupt_workers: bool) -> DeployRow {
        store
            .insert_agent_deploy(
                &NewDeploy {
                    id: Uuid::new_v4(),
                    owner_session_id: owner,
                    idempotency_key: key,
                    sha: SHA,
                    fingerprint: deploy_fingerprint_with(SHA, "x", 60, interrupt_workers),
                    manifest: &[],
                    max_wait_secs: 60,
                    interrupt_workers,
                },
                Utc::now(),
            )
            .unwrap()
    }

    fn wake_message(store: &Store, owner: Uuid) -> String {
        store
            .list_scheduled_jobs()
            .unwrap()
            .into_iter()
            .find(|job| job.wake_session_id == Some(owner))
            .expect("the owner's outcome wake")
            .message
    }

    /// A live session of the store's seeded project, idle between turns.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    fn project_session(store: &Store) -> Uuid {
        let mut row = crate::store::tests::make_test_session();
        row.status = rsi_common::SessionStatus::Completed;
        store.insert_session(&row).unwrap();
        row.id
    }

    /// The operator appoints `session` as the seeded project's manager.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    fn appoint(store: &Store, session: Uuid) {
        let project = crate::store::d04_test_project_id();
        let version = store
            .get_harness_manager(project)
            .unwrap()
            .map_or(0, |config| config.row_version);
        store
            .configure_harness_manager(
                &rsi_common::harness_manager::ConfigureHarnessManagerRequestV1 {
                    project_id: project,
                    session_id: session,
                    epic_ids: None,
                    group_ids: Vec::new(),
                    expected_row_version: version,
                },
            )
            .unwrap();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    fn wake_job(store: &Store, id: Uuid) -> ScheduledJob {
        store
            .list_scheduled_jobs()
            .unwrap()
            .into_iter()
            .find(|job| job.id == id)
            .expect("the deploy's outcome wake")
    }

    /// #272: a deposed seat (still declared as a satellite scope root) owns a
    /// deploy that no re-appointment handed over, because it was not the
    /// replaced anchor's lineage. Its outcome wake reaches the session that
    /// holds the appointment at settlement, after a further re-appointment,
    /// and the wake names the original owner; the row keeps that owner as its
    /// record. A deploy the current seat owns still wakes that seat.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn a_displaced_owner_s_deploy_wake_reaches_the_current_appointed_seat() {
        let store = Store::open_in_memory().unwrap();
        let deposed = project_session(&store);
        appoint(&store, deposed);
        let interim = project_session(&store);
        appoint(&store, interim);
        let row = staged(&store, deposed, "k-deposed", false);
        let current = project_session(&store);
        appoint(&store, current);

        let wake = store
            .settle_agent_deploy(row.id, DeployState::Succeeded, None, Utc::now())
            .unwrap()
            .expect("settled");
        let job = wake_job(&store, wake);
        assert_eq!(job.wake_session_id, Some(current));
        assert!(job.enabled);
        assert!(
            job.message
                .contains(&format!("deploy owner {deposed} no longer holds")),
            "{}",
            job.message
        );
        let settled = store.get_agent_deploy(row.id).unwrap().unwrap();
        assert_eq!(settled.owner_session_id, Some(deposed));

        let own = staged(&store, current, "k-current", false);
        let wake = store
            .settle_agent_deploy(own.id, DeployState::Succeeded, None, Utc::now())
            .unwrap()
            .expect("settled");
        let job = wake_job(&store, wake);
        assert_eq!(job.wake_session_id, Some(current));
        assert!(job.enabled);
        assert!(
            job.message
                .ends_with("Check AgentGetDaemonInfo for the running build.")
        );
    }

    /// #272: with no live appointed seat to redirect to (the appointment that
    /// displaced the owner was revoked), a displaced owner's outcome wake is
    /// recorded disabled with the reason; the settled row is the record the
    /// hub and operator read.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn a_displaced_owner_s_deploy_wake_with_no_live_seat_resumes_nobody() {
        let store = Store::open_in_memory().unwrap();
        let deposed = project_session(&store);
        appoint(&store, deposed);
        let current = project_session(&store);
        appoint(&store, current);
        let row = staged(&store, deposed, "k-orphan", false);
        // The operator revokes the appointment: the seat is still published
        // but no live seat resolves.
        let project = crate::store::d04_test_project_id();
        let version = store
            .get_harness_manager(project)
            .unwrap()
            .unwrap()
            .row_version;
        store
            .configure_harness_manager(
                &rsi_common::harness_manager::ConfigureHarnessManagerRequestV1 {
                    project_id: project,
                    session_id: current,
                    epic_ids: Some(Vec::new()),
                    group_ids: Vec::new(),
                    expected_row_version: version,
                },
            )
            .unwrap();

        let wake = store
            .settle_agent_deploy(row.id, DeployState::Failed, Some("boom"), Utc::now())
            .unwrap()
            .expect("settled");
        let job = wake_job(&store, wake);
        assert!(!job.enabled);
        assert!(
            job.message.contains("Not delivered (#272)"),
            "{}",
            job.message
        );
        let settled = store.get_agent_deploy(row.id).unwrap().unwrap();
        assert_eq!(settled.state, DeployState::Failed);
    }

    /// #2880: a displaced owner whose replacement was archived (the appointment
    /// stays published with no live tip) resumes nobody, and a never-appointed
    /// owner beside its managed project keeps its own wake (#2881).
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn a_deploy_wake_follows_who_held_the_appointment_when_the_seat_has_no_live_tip() {
        let store = Store::open_in_memory().unwrap();
        let deposed = project_session(&store);
        appoint(&store, deposed);
        let current = project_session(&store);
        appoint(&store, current);
        let bystander = project_session(&store);
        let displaced_row = staged(&store, deposed, "k-displaced", false);
        store
            .update_session_status(current, rsi_common::SessionStatus::Archived)
            .unwrap();

        let wake = store
            .settle_agent_deploy(
                displaced_row.id,
                DeployState::Failed,
                Some("boom"),
                Utc::now(),
            )
            .unwrap()
            .expect("settled");
        let job = wake_job(&store, wake);
        assert!(!job.enabled);
        assert!(
            job.message.contains("Not delivered (#272)"),
            "{}",
            job.message
        );

        let bystander_row = staged(&store, bystander, "k-bystander", false);
        let wake = store
            .settle_agent_deploy(bystander_row.id, DeployState::Succeeded, None, Utc::now())
            .unwrap()
            .expect("settled");
        let job = wake_job(&store, wake);
        assert_eq!(job.wake_session_id, Some(bystander));
        assert!(job.enabled);
    }

    /// #1461: a request that does not interrupt workers keeps the fingerprint
    /// every earlier row was stored with; asking to interrupt is a different
    /// request, so a replay cannot flip the option.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn only_the_interrupt_request_changes_the_replay_fingerprint() {
        assert_eq!(
            deploy_fingerprint(SHA, "/b", 900),
            deploy_fingerprint_with(SHA, "/b", 900, false)
        );
        assert_ne!(
            deploy_fingerprint_with(SHA, "/b", 900, true),
            deploy_fingerprint_with(SHA, "/b", 900, false)
        );
    }

    /// #1461: the request rides the row (V158 columns), and a deploy that never
    /// asked reads as no request and no interrupted workers.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn the_row_records_the_interrupt_request_and_defaults_to_none() {
        let store = Store::open_in_memory().unwrap();
        let owner = session(&store, "Running", None, "Task");
        let plain = staged(&store, owner, "plain", false);
        assert_eq!(
            store.agent_deploy_interrupt(plain.id).unwrap(),
            DeployInterrupt::default()
        );
        store
            .settle_agent_deploy(plain.id, DeployState::Failed, Some("x"), Utc::now())
            .unwrap();
        let asked = staged(&store, owner, "asked", true);
        assert_eq!(
            store.agent_deploy_interrupt(asked.id).unwrap(),
            DeployInterrupt {
                requested: true,
                interrupted: Vec::new(),
                cancelled_jobs: Vec::new(),
            }
        );
        assert_eq!(
            store.agent_deploy_interrupt(Uuid::new_v4()).unwrap(),
            DeployInterrupt::default(),
            "an unknown deploy reads as no request"
        );
        let defaults: Vec<(String, i64, Option<String>)> = store
            .conn
            .prepare(
                "SELECT name, \"notnull\", dflt_value FROM pragma_table_info('agent_deploys') \
                 WHERE name IN ('interrupt_workers','interrupted_sessions') ORDER BY name",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            defaults,
            vec![
                ("interrupt_workers".to_string(), 1, Some("0".to_string())),
                ("interrupted_sessions".to_string(), 0, None),
            ]
        );
    }

    /// #1461: the worker predicate is one definition for the blocker and the
    /// interrupted list: scoped Starting/Running sessions of anyone but the
    /// caller; idle, parentless and container sessions are not workers.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn workers_mid_turn_are_listed_and_can_stop_blocking() {
        let store = Store::open_in_memory().unwrap();
        let caller = session(&store, "Running", None, "Task");
        let running = session(&store, "Running", Some(caller), "Task");
        let starting = session(&store, "Starting", Some(caller), "Task");
        session(&store, "Completed", Some(caller), "Task");
        session(&store, "WaitingApproval", Some(caller), "Task");
        session(&store, "Running", Some(caller), "Epic");
        session(&store, "Running", Some(caller), "Group");
        session(&store, "Running", None, "Task");
        let mut expected = vec![running, starting];
        expected.sort();
        assert_eq!(store.deploy_mid_turn_workers(caller).unwrap(), expected);
        assert_eq!(
            store.deploy_quiet_blockers(caller).unwrap(),
            vec!["worker_mid_turn"]
        );
        assert_eq!(
            store.deploy_quiet_blockers_with(caller, true).unwrap(),
            vec!["worker_mid_turn"]
        );
        assert!(
            store
                .deploy_quiet_blockers_with(caller, false)
                .unwrap()
                .is_empty()
        );
        let caller_is_worker = session(&store, "Running", Some(caller), "Task");
        assert!(
            !store
                .deploy_mid_turn_workers(caller_is_worker)
                .unwrap()
                .contains(&caller_is_worker),
            "the caller's own session is never one of the workers"
        );
    }

    /// #1461: the interrupted workers are recorded once, only while the deploy
    /// is restarting, and the settle wake (written by the next process) names
    /// them; a deploy that interrupted nobody adds nothing.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn the_settle_wake_names_the_workers_the_restart_interrupted() {
        let store = Store::open_in_memory().unwrap();
        let owner = session(&store, "Running", None, "Task");
        let row = staged(&store, owner, "k", true);
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        assert!(
            !store.record_agent_deploy_interrupted(row.id, &[a]).unwrap(),
            "only a restarting deploy interrupts"
        );
        assert!(
            store
                .mark_agent_deploy_restarting(row.id, Utc::now())
                .unwrap()
        );
        assert!(
            store
                .record_agent_deploy_interrupted(row.id, &[a, b])
                .unwrap()
        );
        assert!(
            !store.record_agent_deploy_interrupted(row.id, &[b]).unwrap(),
            "recorded once"
        );
        assert_eq!(
            store.agent_deploy_interrupt(row.id).unwrap(),
            DeployInterrupt {
                requested: true,
                interrupted: vec![a, b],
                cancelled_jobs: Vec::new(),
            }
        );
        store
            .settle_agent_deploy(row.id, DeployState::Succeeded, None, Utc::now())
            .unwrap();
        let message = wake_message(&store, owner);
        assert!(message.contains(&a.to_string()), "{message}");
        assert!(message.contains(&b.to_string()), "{message}");
        assert!(message.contains("continue prompt"), "{message}");
    }

    /// #1461: V158 adds the two columns to a V157 catalog, keeps the rows it
    /// finds (they read as "no interrupt request") and is a no-op when re-run.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn v158_upgrades_a_v157_catalog_and_old_rows_read_as_no_request() {
        let store = Store::open_in_memory().unwrap();
        let owner = session(&store, "Running", None, "Task");
        let legacy = staged(&store, owner, "legacy", false);
        store
            .conn
            .execute_batch(
                "ALTER TABLE agent_deploys DROP COLUMN interrupt_workers;
                 ALTER TABLE agent_deploys DROP COLUMN interrupted_sessions;
                 PRAGMA user_version=157;",
            )
            .unwrap();
        store.migrate_v158(157).unwrap();
        let version: i32 = store
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 158);
        assert_eq!(
            store.agent_deploy_interrupt(legacy.id).unwrap(),
            DeployInterrupt::default()
        );
        store
            .settle_agent_deploy(legacy.id, DeployState::Failed, Some("x"), Utc::now())
            .unwrap();
        let asked = staged(&store, owner, "after", true);
        assert!(store.agent_deploy_interrupt(asked.id).unwrap().requested);
        store.migrate_v158(158).unwrap();
        let wrong: i32 = store
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(wrong, 158, "re-running a settled step changes nothing");
        store
            .conn
            .execute_batch("PRAGMA user_version=150;")
            .unwrap();
        assert!(
            store.migrate_v158(150).is_err(),
            "the step needs V157 as its predecessor"
        );
    }

    /// #2549: a topology node session the restart interrupted is named with its
    /// execution and node, so the manager reads the outcome as that execution's
    /// node being resumed.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn the_settle_wake_names_the_topology_node_of_an_interrupted_worker() {
        let store = Store::open_in_memory().unwrap();
        let owner = session(&store, "Running", None, "Task");
        let row = staged(&store, owner, "topology-node", true);
        let (node_session, plain) = (Uuid::new_v4(), Uuid::new_v4());
        let execution = Uuid::new_v4();
        store.conn.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
        store
            .conn
            .execute(
                "INSERT INTO topology_node_attempts(id,execution_id,node_id,iteration,attempt_no,\
                 node_kind,status,dedup_key,session_id,input_json,created_at,updated_at) \
                 VALUES (?1,?2,'implement',0,1,'session','running','k',?3,'{}',?4,?4)",
                params![
                    Uuid::new_v4().to_string(),
                    execution.to_string(),
                    node_session.to_string(),
                    stamp(Utc::now())
                ],
            )
            .unwrap();
        assert!(
            store
                .mark_agent_deploy_restarting(row.id, Utc::now())
                .unwrap()
        );
        assert!(
            store
                .record_agent_deploy_interrupted(row.id, &[node_session, plain])
                .unwrap()
        );
        store
            .settle_agent_deploy(row.id, DeployState::Succeeded, None, Utc::now())
            .unwrap();
        let message = wake_message(&store, owner);
        assert!(
            message.contains(&format!(
                "{node_session} [topology node implement of execution {execution}]"
            )),
            "{message}"
        );
        assert!(message.contains(&plain.to_string()), "{message}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn a_settle_with_no_interrupted_workers_says_nothing_about_them() {
        let store = Store::open_in_memory().unwrap();
        let owner = session(&store, "Running", None, "Task");
        let row = staged(&store, owner, "none", true);
        assert!(
            store
                .mark_agent_deploy_restarting(row.id, Utc::now())
                .unwrap()
        );
        assert!(store.record_agent_deploy_interrupted(row.id, &[]).unwrap());
        store
            .settle_agent_deploy(row.id, DeployState::Succeeded, None, Utc::now())
            .unwrap();
        let message = wake_message(&store, owner);
        assert!(!message.contains("mid-turn"), "{message}");
    }

    /// #2701: many successful deploys of the current commit never push the
    /// previous distinct commit out of the rollback window.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn recent_succeeded_shas_keep_the_previous_commit_after_many_redeploys() {
        const OTHER: &str = "fedcba9876543210fedcba9876543210fedcba98";
        let store = Store::open_in_memory().unwrap();
        let owner = session(&store, "Running", None, "Task");
        let base = Utc::now();
        for n in 0..70_i64 {
            let sha = if n == 0 { SHA } else { OTHER };
            let at = base + Duration::seconds(n);
            let row = store
                .insert_agent_deploy(
                    &NewDeploy {
                        id: Uuid::new_v4(),
                        owner_session_id: owner,
                        idempotency_key: &format!("k{n}"),
                        sha,
                        fingerprint: deploy_fingerprint_with(sha, "x", 60, false),
                        manifest: &[],
                        max_wait_secs: 60,
                        interrupt_workers: false,
                    },
                    at,
                )
                .unwrap();
            store
                .settle_agent_deploy(row.id, DeployState::Succeeded, None, at)
                .unwrap()
                .expect("settled");
        }
        assert_eq!(
            store.recent_succeeded_deploy_shas(2).unwrap(),
            vec![OTHER.to_string(), SHA.to_string()]
        );
        assert_eq!(
            store.recent_succeeded_deploy_shas(1).unwrap(),
            vec![OTHER.to_string()]
        );
    }

    /// #2278: the settle wake names the test jobs the deploy stopped and
    /// requeued, and says each owner's wake fires on the rerun.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn the_settle_wake_names_the_jobs_the_deploy_requeued() {
        let store = Store::open_in_memory().unwrap();
        let owner = session(&store, "Running", None, "Task");
        let row = staged(&store, owner, "requeue", true);
        let job = Uuid::new_v4();
        store
            .conn
            .execute(
                "INSERT INTO agent_jobs(id, owner_session_id, kind, params_json, cwd, \
                 unit_name, log_path, status_path, state, created_at, row_version) \
                 VALUES (?1,?2,'test','{}','/tmp','u','/tmp/l','/tmp/s','running',?3,1)",
                params![job.to_string(), owner.to_string(), stamp(Utc::now())],
            )
            .unwrap();
        assert!(store.requeue_agent_job_for_deploy(job, row.id).unwrap());
        assert_eq!(
            store.agent_deploy_interrupt(row.id).unwrap().cancelled_jobs,
            [job]
        );
        store
            .settle_agent_deploy(row.id, DeployState::TimedOut, None, Utc::now())
            .unwrap();
        let message = wake_message(&store, owner);
        assert!(message.contains(&job.to_string()), "{message}");
        assert!(message.contains("requeued"), "{message}");
    }
    /// #1572: the V180 rebuild gives every existing agent row its owner as
    /// request identity and keeps every record.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn the_v180_rebuild_backfills_request_identity_from_the_owner_1572() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("deploys.sqlite");
        let (done, live);
        let owner;
        {
            let store = Store::open(&database).unwrap();
            owner = session(&store, "Running", None, "Task");
            done = staged(&store, owner, "settled", false);
            store
                .settle_agent_deploy(done.id, DeployState::Failed, Some("x"), Utc::now())
                .unwrap();
            live = staged(&store, owner, "live", true);
            crate::store::tests::rewind_post_v121_tail_to(&store.conn, 179);
            let rewound: bool = store
                .conn
                .query_row(
                    "SELECT NOT EXISTS(SELECT 1 FROM pragma_table_info('agent_deploys') \
                     WHERE name='request_session_id')",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(rewound);
        }
        let store = Store::open(&database).unwrap();
        let identities: Vec<(String, String)> = store
            .conn
            .prepare("SELECT id, request_session_id FROM agent_deploys ORDER BY id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(identities.len(), 2);
        for (id, identity) in identities {
            assert!([done.id, live.id].contains(&Uuid::parse_str(&id).unwrap()));
            assert_eq!(identity, owner.to_string());
        }
        assert_eq!(
            store.get_agent_deploy(done.id).unwrap().unwrap().state,
            DeployState::Failed
        );
        assert_eq!(
            store.get_agent_deploy(live.id).unwrap().unwrap().state,
            DeployState::Staged
        );
        // The request identity cannot be rewritten.
        let error = store
            .conn
            .execute(
                "UPDATE agent_deploys SET request_session_id=?2 WHERE id=?1",
                params![live.id.to_string(), Uuid::new_v4().to_string()],
            )
            .unwrap_err();
        assert!(error.to_string().contains("request identity is immutable"));
    }
}
