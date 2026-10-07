//! #1122: operator-requested quiet-point restarts.
//!
//! The operator's rebuild (`make release-install`) reuses the `AgentRequestDeploy`
//! catalog and runner (#1045). An operator deploy has no owning session and no
//! settlement wake (`origin='operator'`); its quiet point also counts managers
//! (parentless sessions) that are mid-turn, because the operator, unlike a
//! manager, is not one of them. `forced` skips the quiet gate (the operator
//! pressed "restart now").

use super::Store;
use super::agent_deploys::{DeployRow, NewOperatorDeploy, deploy_digest, map_deploy_row};
use crate::error::{DaemonError, Result};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};

// The provisional number is assigned at landing.
// RSI-RELEASED-MIGRATION-BEGIN: agent-deploys-operator-migration
/// Provisional schema version of the operator-origin deploy rebuild.
pub(crate) const AGENT_DEPLOYS_OPERATOR_SCHEMA_VERSION: i32 = 151;

const COPY_COLUMNS: &str = "id, owner_session_id, idempotency_digest, request_fingerprint, sha, \
    manifest_json, state, reason, wake_job_id, created_at, deadline_at, restarted_at, settled_at";

const CATALOG: &str = "CREATE TABLE agent_deploys_rebuilt (
    id TEXT PRIMARY KEY CHECK(rsi_uuid_is_canonical(id)),
    owner_session_id TEXT REFERENCES sessions(id) ON DELETE RESTRICT CHECK(owner_session_id IS NULL OR rsi_uuid_is_canonical(owner_session_id)),
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
    origin TEXT NOT NULL DEFAULT 'agent' CHECK(origin IN ('agent','operator')),
    forced INTEGER NOT NULL DEFAULT 0 CHECK(forced IN (0,1)),
    UNIQUE(owner_session_id, idempotency_digest),
    CHECK((origin='agent' AND owner_session_id IS NOT NULL) OR (origin='operator' AND owner_session_id IS NULL)),
    CHECK((state IN ('staged','restarting') AND settled_at IS NULL AND wake_job_id IS NULL)
       OR (state IN ('succeeded','failed','timed_out') AND settled_at IS NOT NULL
           AND ((origin='agent' AND wake_job_id IS NOT NULL) OR (origin='operator' AND wake_job_id IS NULL))))
);
INSERT INTO agent_deploys_rebuilt (id, owner_session_id, idempotency_digest, request_fingerprint, sha,
    manifest_json, state, reason, wake_job_id, created_at, deadline_at, restarted_at, settled_at)
    SELECT id, owner_session_id, idempotency_digest, request_fingerprint, sha,
    manifest_json, state, reason, wake_job_id, created_at, deadline_at, restarted_at, settled_at
    FROM agent_deploys;
DROP TABLE agent_deploys;
ALTER TABLE agent_deploys_rebuilt RENAME TO agent_deploys;
CREATE UNIQUE INDEX agent_deploys_one_live ON agent_deploys((1)) WHERE state IN ('staged','restarting');
CREATE TRIGGER agent_deploys_no_delete BEFORE DELETE ON agent_deploys BEGIN SELECT RAISE(ABORT,'agent deploys are append only'); END;
CREATE TRIGGER agent_deploys_terminal_immutable BEFORE UPDATE ON agent_deploys WHEN OLD.state IN ('succeeded','failed','timed_out') BEGIN SELECT RAISE(ABORT,'settled agent deploys are immutable'); END;";

pub(crate) fn apply_migration(store: &Store, version: i32) -> Result<()> {
    let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)?;
    let prior: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != AGENT_DEPLOYS_OPERATOR_SCHEMA_VERSION || prior != version - 1 {
        return Err(DaemonError::Store(format!(
            "operator deploys require V{}, found V{prior}",
            version - 1
        )));
    }
    tx.execute_batch(CATALOG)?;
    tx.pragma_update(None, "user_version", version)?;
    tx.commit()?;
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: agent-deploys-operator-migration

/// Fixture rewind: restore the V146 catalog shape (agent-owned rows only).
#[cfg(test)]
pub(crate) fn rewind_to_agent_only(connection: &rusqlite::Connection) -> rusqlite::Result<()> {
    connection.execute_batch(&format!(
        "ALTER TABLE agent_deploys RENAME TO agent_deploys_rewound;
         DROP TRIGGER agent_deploys_terminal_immutable;
         DROP TRIGGER agent_deploys_no_delete;
         DROP INDEX agent_deploys_one_live;
         CREATE TABLE agent_deploys (
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
         INSERT INTO agent_deploys ({COPY_COLUMNS}) SELECT {COPY_COLUMNS}
            FROM agent_deploys_rewound WHERE origin='agent';
         DROP TABLE agent_deploys_rewound;
         CREATE UNIQUE INDEX agent_deploys_one_live ON agent_deploys((1)) WHERE state IN ('staged','restarting');
         CREATE TRIGGER agent_deploys_no_delete BEFORE DELETE ON agent_deploys BEGIN SELECT RAISE(ABORT,'agent deploys are append only'); END;
         CREATE TRIGGER agent_deploys_terminal_immutable BEFORE UPDATE ON agent_deploys WHEN OLD.state IN ('succeeded','failed','timed_out') BEGIN SELECT RAISE(ABORT,'settled agent deploys are immutable'); END;"
    ))
}

fn stamp(now: DateTime<Utc>) -> String {
    now.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

impl Store {
    /// Insert a `staged` operator deploy. `deploy_already_in_progress` when
    /// another deploy is live.
    pub fn insert_operator_deploy(
        &self,
        new: &NewOperatorDeploy<'_>,
        now: DateTime<Utc>,
    ) -> Result<DeployRow> {
        let manifest = serde_json::to_string(new.manifest)
            .map_err(|error| DaemonError::Store(error.to_string()))?;
        let deadline = now + Duration::seconds(i64::from(new.max_wait_secs));
        let inserted = self.conn.execute(
            "INSERT INTO agent_deploys (id, owner_session_id, idempotency_digest, \
             request_fingerprint, sha, manifest_json, state, created_at, deadline_at, \
             origin, forced) VALUES (?1, NULL, ?2, ?3, ?4, ?5, 'staged', ?6, ?7, 'operator', ?8)",
            params![
                new.id.to_string(),
                deploy_digest(&["operator", &new.id.to_string()]),
                deploy_digest(&[new.sha, "operator"]),
                new.sha,
                manifest,
                stamp(now),
                stamp(deadline),
                i64::from(new.forced),
            ],
        );
        match inserted {
            Ok(_) => {}
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::ConstraintViolation
                    && self.live_agent_deploy()?.is_some() =>
            {
                return Err(DaemonError::InvalidParam(
                    rsi_common::agent_deploy::DEPLOY_IN_PROGRESS.into(),
                ));
            }
            Err(error) => return Err(error.into()),
        }
        self.get_agent_deploy(new.id)?
            .ok_or_else(|| DaemonError::Store("deploy vanished after insert".into()))
    }

    /// Skip the quiet gate of the live `staged` operator deploy. False when
    /// there is none.
    pub fn force_operator_deploy(&self) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE agent_deploys SET forced=1 WHERE origin='operator' AND state='staged'",
            [],
        )? == 1)
    }

    /// The newest operator deploy of any state, for the status read.
    pub fn latest_operator_deploy(&self) -> Result<Option<DeployRow>> {
        Ok(self
            .conn
            .query_row(
                &format!(
                    "SELECT {} FROM agent_deploys WHERE origin='operator' \
                     ORDER BY created_at DESC LIMIT 1",
                    super::agent_deploys::COLUMNS
                ),
                [],
                map_deploy_row,
            )
            .optional()?)
    }

    /// Quiet-point blockers for an operator deploy: everything an agent
    /// deploy waits for, plus every manager (parentless, non-container
    /// session) that is mid-turn. Returns `(blockers, turns_in_flight)`.
    pub fn operator_quiet_blockers(&self) -> Result<(Vec<&'static str>, i64)> {
        let mut blockers = self.deploy_quiet_blockers(uuid::Uuid::nil())?;
        let managers: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM sessions WHERE status IN ('Starting','Running') \
             AND parent_id IS NULL AND COALESCE(session_kind,'') NOT IN ('Group','Epic')",
            [],
            |row| row.get(0),
        )?;
        if managers > 0 {
            blockers.push("manager_mid_turn");
        }
        let turns: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM sessions WHERE status IN ('Starting','Running') \
             AND COALESCE(session_kind,'') NOT IN ('Group','Epic')",
            [],
            |row| row.get(0),
        )?;
        Ok((blockers, turns))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::LATEST_SCHEMA_VERSION;
    use crate::store::agent_deploys::{NewDeploy, deploy_fingerprint};
    use rsi_common::agent_deploy::DeployState;
    use uuid::Uuid;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn the_rebuild_keeps_agent_deploys_and_accepts_an_ownerless_operator_deploy() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("deploys.sqlite");
        let (owner, agent_deploy) = (Uuid::new_v4(), Uuid::new_v4());
        let now = Utc::now();
        {
            let store = Store::open(&database).unwrap();
            store
                .conn
                .execute(
                    "INSERT INTO sessions (id, provider, query, working_dir, status, created_at, \
                     updated_at, session_kind) VALUES (?1,'Claude','q','/tmp','Running',?2,?2,'Task')",
                    params![owner.to_string(), stamp(now)],
                )
                .unwrap();
            store
                .insert_agent_deploy(
                    &NewDeploy {
                        id: agent_deploy,
                        owner_session_id: owner,
                        idempotency_key: "k",
                        sha: SHA,
                        fingerprint: deploy_fingerprint(SHA, "x", 60),
                        manifest: &[],
                        max_wait_secs: 60,
                        interrupt_workers: false,
                    },
                    now,
                )
                .unwrap();
            store
                .settle_agent_deploy(agent_deploy, DeployState::Succeeded, None, now)
                .unwrap();
            crate::store::tests::rewind_post_v121_tail_to(
                &store.conn,
                AGENT_DEPLOYS_OPERATOR_SCHEMA_VERSION - 1,
            );
        }
        let store = Store::open(&database).unwrap();
        let version: i32 = store
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, LATEST_SCHEMA_VERSION);
        let kept = store.get_agent_deploy(agent_deploy).unwrap().unwrap();
        assert_eq!(kept.owner_session_id, Some(owner));
        assert_eq!(kept.state, DeployState::Succeeded);
        assert!(!kept.operator && !kept.forced);
        let wakes: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM scheduled_jobs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(wakes, 1, "the agent deploy's settlement wake is untouched");

        let id = Uuid::new_v4();
        let row = store
            .insert_operator_deploy(
                &NewOperatorDeploy {
                    id,
                    sha: SHA,
                    manifest: &[],
                    max_wait_secs: 60,
                    forced: false,
                },
                now,
            )
            .unwrap();
        assert!(row.operator && row.owner_session_id.is_none());
        assert_eq!(store.live_agent_deploy().unwrap().unwrap().id, id);
        store
            .settle_agent_deploy(id, DeployState::Failed, Some("cancelled_by_operator"), now)
            .unwrap();
        let wakes: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM scheduled_jobs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(wakes, 1, "an operator deploy settles without a wake");
    }
}
