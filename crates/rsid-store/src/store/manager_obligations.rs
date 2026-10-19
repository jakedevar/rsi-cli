//! #2471: what a manager seat owes survives every same-role seat change.
//!
//! A manager seat holds two kinds of resume wakes. A *backstop* (a recurring
//! safety net, a lead check-in, a generic `resume` continuation) belongs to
//! the seat, not to the session that armed it, so a rotation or self-handoff
//! of the seat (`backstop_follows`) moves it with its name, message and fire
//! time (#2511); a wake whose time has passed fires on the successor once. A
//! seat handed to a different session (appointment, replacement) leaves it. A durable *obligation* is a result or a predicate the
//! seat is waiting on, and the seat's successor must receive it exactly once:
//!
//! - the outcome wake of a settled daemon job (`agent_jobs.wake_job_id`);
//! - a `mode:"when"` predicate wake (`jobs_terminal`, `issue_closed`,
//!   `todo_closed`, `topology_execution_settled`, `sha_on_rolling`,
//!   `health_anomaly`), including
//!   an all-terminal batch wait whose jobs already settled;
//! - an enabled terminal watch (`on_terminal:`);
//! - a live deploy and its outcome wake;
//! - an unsettled rolling-queue outcome (see `rolling_queue`);
//! - every other enabled `resume` wake of the seat (the backstop, #2511).
//!
//! [`transfer_manager_obligations_on`] retargets all of them to the successor
//! inside the caller's transaction, so the sweep never sees an obligation on a
//! superseded session. Protected rows (manager notice watches, the wakes of an
//! open capacity incident) and the daemon's own per-session rows (`keepalive-*`
//! child valves, `background-process-*` wakes) keep their owners; the
//! maintenance sweep retires those with the superseded session. Rows are
//! retargeted or disabled (a duplicate terminal watch), never deleted.

use super::Store;
use crate::error::{DaemonError, Result};
use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

impl Store {
    /// The enabled wakes of `session` that a seat change would hand to its
    /// successor (#2511): the cap handoff lists exactly these as armed.
    pub fn seat_change_carried_wake_ids(&self, session: Uuid) -> Result<Vec<Uuid>> {
        carried_wake_ids_on(&self.conn, session)
    }
}

/// Wakes whose own owner is not the ordinary scheduler.
macro_rules! not_protected {
    () => {
        "NOT EXISTS(SELECT 1 FROM harness_manager_watches w WHERE w.job_id=scheduled_jobs.id)
     AND NOT EXISTS(SELECT 1 FROM master_no_idle_capacity_incidents i
                    WHERE i.state='open' AND (i.wake_job_id=scheduled_jobs.id OR i.program_guard_job_id=scheduled_jobs.id))"
    };
}

/// What one transfer retargeted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ObligationsMoved {
    /// Enabled terminal watches handed over.
    pub watches: usize,
    /// Live deploys handed over.
    pub deploys: usize,
    /// Outcome and predicate wakes handed over (deploy wakes included).
    pub wakes: usize,
}

fn stamp() -> String {
    super::harness_manager_v2::now()
}

/// Hand every durable obligation of `predecessor` to `successor`. Callers
/// validate the seat custody and hold the publication transaction.
pub(crate) fn transfer_manager_obligations_on(
    conn: &Connection,
    predecessor: Uuid,
    successor: Uuid,
    backstop_follows: bool,
) -> Result<ObligationsMoved> {
    if conn.is_autocommit() {
        return Err(DaemonError::Store(
            "manager obligation transfer requires a transaction".into(),
        ));
    }
    if predecessor == successor {
        return Ok(ObligationsMoved::default());
    }
    // Live jobs, the rolling-queue outcomes and the batch waits that follow
    // their jobs.
    super::agent_jobs::transfer_manager_jobs_on(conn, predecessor, successor)?;
    let old = predecessor.to_string();
    let target = successor.to_string();
    let stamp = stamp();
    let mut moved = ObligationsMoved::default();
    let owned: Vec<(String, String)> = {
        let mut statement = conn.prepare(
            "SELECT j.id, j.wake_mode FROM scheduled_jobs j
             WHERE j.enabled=1 AND j.wake_session_id=?1 AND j.wake_mode LIKE 'on_terminal:%'
               AND NOT EXISTS(SELECT 1 FROM harness_manager_watches w WHERE w.job_id=j.id)
             ORDER BY j.id",
        )?;
        statement
            .query_map([&old], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?
    };
    for (job, mode) in owned {
        // One terminal fires one wake: a watch the successor already holds on
        // the same session wins and this duplicate is disabled instead.
        let held: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM scheduled_jobs
             WHERE enabled=1 AND wake_session_id=?1 AND wake_mode=?2)",
            params![target, mode],
            |r| r.get(0),
        )?;
        let changed = if held {
            conn.execute(
                "UPDATE scheduled_jobs SET enabled=0,updated_at=?2 WHERE id=?1 AND enabled=1",
                params![job, stamp],
            )?
        } else {
            conn.execute(
                "UPDATE scheduled_jobs SET wake_session_id=?2,updated_at=?3
                 WHERE id=?1 AND enabled=1 AND wake_session_id=?4",
                params![job, target, stamp, old],
            )?
        };
        moved.watches += usize::from(changed == 1 && !held);
    }
    // Earlier hops of the same slot's lineage too: a watch armed before this
    // change shipped may still sit on a seat retired several handovers ago.
    for hop in slot_lineage_on(conn, predecessor)? {
        follow_slot_watches_on(conn, hop, successor, &stamp)?;
    }
    // #1572: only the settling owner moves; the deploy keeps the request
    // identity (`request_session_id`) its key was recorded under, so a key the
    // successor already used for a settled deployment cannot block the handover.
    moved.deploys += conn.execute(
        "UPDATE agent_deploys SET owner_session_id=?2
         WHERE owner_session_id=?1 AND state IN ('staged','restarting')",
        params![old, target],
    )?;
    // Deploy outcome wakes, and the outcome wake of every settled job. A job
    // row is immutable once terminal, so its outcome wake is the only thing
    // that can follow the seat.
    moved.wakes += conn.execute(
        concat!(
            "UPDATE scheduled_jobs SET wake_session_id=?2,updated_at=?3
             WHERE enabled=1 AND wake_mode='resume' AND wake_session_id=?1 AND ",
            not_protected!(),
            " AND (id IN (SELECT wake_job_id FROM agent_deploys WHERE wake_job_id IS NOT NULL)
                 OR id IN (SELECT wake_job_id FROM agent_jobs WHERE wake_job_id IS NOT NULL))"
        ),
        params![old, target, stamp],
    )?;
    // Predicate waits. A batch wait follows its jobs when every member is a
    // known job and every live member belongs to this seat change; the other
    // predicates name no job. A terminal job row is immutable, so its owner
    // stays the first seat it ran under: it is seat-neutral here, or the wait
    // would stop following the seat after one hop (#2483).
    moved.wakes += conn.execute(
        concat!(
            "UPDATE scheduled_jobs SET wake_session_id=?2,updated_at=?3
             WHERE enabled=1 AND wake_mode='resume' AND wake_session_id=?1 AND ",
            not_protected!(),
            " AND json_valid(schedule_json)
               AND json_extract(schedule_json,'$.wake_when') IS NOT NULL
               AND (json_type(schedule_json,'$.wake_when.predicate.jobs_terminal') IS NULL
                 OR (EXISTS(SELECT 1 FROM json_each(schedule_json,'$.wake_when.predicate.jobs_terminal'))
                     AND NOT EXISTS(SELECT 1
                       FROM json_each(schedule_json,'$.wake_when.predicate.jobs_terminal') ids
                       LEFT JOIN agent_jobs j ON j.id=ids.value
                       WHERE j.id IS NULL
                          OR (j.state IN ('queued','running')
                              AND j.owner_session_id NOT IN (?1,?2)))))"
        ),
        params![old, target, stamp],
    )?;
    // The seat's backstop: every other enabled resume wake. A name the
    // successor already holds wins (one name, one live wake); the leftover
    // is retired by the maintenance sweep once its session is archived. A
    // seat handed to a different session (an appointment, a replacement)
    // keeps no such inheritance: only a rotation or self-handoff of the
    // same seat carries it.
    // #2652: an explicit `succeed_manager` hands over durable obligations only.
    // Its publication (state `established`, set in the same transaction just
    // before this transfer) cancels the displaced seat's generic backstops and
    // names them in the final receipt; an automatic rotation has no such row.
    if !backstop_follows || explicit_succession_publishing_on(conn, predecessor)? {
        return Ok(moved);
    }
    moved.wakes += conn.execute(
        concat!(
            "UPDATE scheduled_jobs SET wake_session_id=?2,updated_at=?3
             WHERE enabled=1 AND wake_mode='resume' AND wake_session_id=?1 AND ",
            not_protected!(),
            " AND NOT (json_valid(schedule_json)
                       AND json_extract(schedule_json,'$.wake_when') IS NOT NULL)
               AND name NOT LIKE 'keepalive-%' AND name NOT LIKE 'background-process-%'
               AND NOT EXISTS(SELECT 1 FROM scheduled_jobs held
                              WHERE held.enabled=1 AND held.wake_mode='resume'
                                AND held.wake_session_id=?2 AND held.name=scheduled_jobs.name
                                AND held.name<>'agent-wake')"
        ),
        params![old, target, stamp],
    )?;
    Ok(moved)
}

/// True while `predecessor`'s explicit manager succession is being published:
/// its root row is `established` and only the publication transaction moves it
/// on to `committed`.
fn explicit_succession_publishing_on(conn: &Connection, predecessor: Uuid) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM manager_root_successions
         WHERE predecessor_session_id=?1 AND state='established')",
        [predecessor.to_string()],
        |r| r.get(0),
    )?)
}

/// #2560: a terminal watch on a seat is a watch on the slot. When the seat
/// changes hands, every ordinary terminal watch ON the predecessor (armed by a
/// parent, not by the seat itself) is retargeted to the successor, so routine
/// succession neither fires it nor leaves a stale watch behind. A watch its
/// owner already holds on the successor wins and the duplicate is disabled
/// (one watch per slot). Manager notice watches keep their own routes.
fn follow_slot_watches_on(
    conn: &Connection,
    predecessor: Uuid,
    successor: Uuid,
    stamp: &str,
) -> Result<usize> {
    let old_mode = format!("on_terminal:{predecessor}");
    let new_mode = format!("on_terminal:{successor}");
    let owners: Vec<(String, String)> = {
        let mut statement = conn.prepare(
            "SELECT j.id, j.wake_session_id FROM scheduled_jobs j
             WHERE j.enabled=1 AND j.wake_mode=?1
               AND j.wake_session_id IS NOT NULL
               AND j.wake_session_id NOT IN (?2,?3)
               AND NOT EXISTS(SELECT 1 FROM harness_manager_watches w WHERE w.job_id=j.id)
             ORDER BY j.id",
        )?;
        statement
            .query_map(
                params![old_mode, predecessor.to_string(), successor.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?
            .collect::<std::result::Result<_, _>>()?
    };
    let mut followed = 0;
    for (job, owner) in owners {
        let held: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM scheduled_jobs
             WHERE enabled=1 AND wake_session_id=?1 AND wake_mode=?2)",
            params![owner, new_mode],
            |r| r.get(0),
        )?;
        let changed = if held {
            conn.execute(
                "UPDATE scheduled_jobs SET enabled=0,updated_at=?2 WHERE id=?1 AND enabled=1",
                params![job, stamp],
            )?
        } else {
            conn.execute(
                "UPDATE scheduled_jobs SET wake_mode=?2,updated_at=?3
                 WHERE id=?1 AND enabled=1 AND wake_mode=?4",
                params![job, new_mode, stamp, old_mode],
            )?
        };
        followed += changed;
    }
    Ok(followed)
}

/// `seat` and its `continued_from` ancestors in the same project, nearest
/// first (bounded; stops at a cycle or a project change).
fn slot_lineage_on(conn: &Connection, seat: Uuid) -> Result<Vec<Uuid>> {
    let mut hops = vec![seat];
    let mut current = seat;
    for _ in 0..32 {
        let parent: Option<(Option<String>, Option<String>, Option<String>)> = conn
            .query_row(
                "SELECT s.continued_from, s.project_id, p.project_id FROM sessions s
                 LEFT JOIN sessions p ON p.id=s.continued_from WHERE s.id=?1",
                [current.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((Some(parent), project, parent_project)) = parent else {
            break;
        };
        if project != parent_project {
            break;
        }
        let Ok(parent) = Uuid::parse_str(&parent) else {
            break;
        };
        if hops.contains(&parent) {
            break;
        }
        hops.push(parent);
        current = parent;
    }
    Ok(hops)
}

/// Enabled wakes of `session` that a seat change hands to the successor
/// ([`transfer_manager_obligations_on`] moves exactly these), for the cap
/// handoff's "Armed wakes" list (#2511).
pub(crate) fn carried_wake_ids_on(conn: &Connection, session: Uuid) -> Result<Vec<Uuid>> {
    let mut statement = conn.prepare(concat!(
        "SELECT id FROM scheduled_jobs
         WHERE enabled=1 AND wake_session_id=?1
           AND (wake_mode='resume' OR wake_mode LIKE 'on_terminal:%')
           AND name NOT LIKE 'keepalive-%' AND name NOT LIKE 'background-process-%' AND ",
        not_protected!(),
        " AND (NOT json_valid(schedule_json)
               OR json_extract(schedule_json,'$.wake_when') IS NULL
               OR json_type(schedule_json,'$.wake_when.predicate.jobs_terminal') IS NULL
               OR (EXISTS(SELECT 1 FROM json_each(schedule_json,'$.wake_when.predicate.jobs_terminal'))
                   AND NOT EXISTS(SELECT 1
                     FROM json_each(schedule_json,'$.wake_when.predicate.jobs_terminal') ids
                     LEFT JOIN agent_jobs j ON j.id=ids.value
                     WHERE j.id IS NULL
                        OR (j.state IN ('queued','running') AND j.owner_session_id<>?1))))"
    ))?;
    let ids: Vec<String> = statement
        .query_map([session.to_string()], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(ids
        .iter()
        .filter_map(|id| Uuid::parse_str(id).ok())
        .collect())
}

/// Shared arming and assertions for the seat-transition matrix (#2471).
#[cfg(test)]
#[allow(dead_code, clippy::unwrap_used, clippy::expect_used)]
pub(crate) mod test_support {
    use super::super::Store;
    use crate::store_support::schedule_wake_job::{ScheduleWakeRequest, build_agent_scheduled_job};
    use rsi_common::agent_jobs::{AgentJobResultV1, JobState};
    use rsi_common::wake_predicate::{WakePredicate, WakeWhenState};
    use uuid::Uuid;

    /// Every obligation one seat can owe, plus the rows a transfer must leave.
    pub(crate) struct Armed {
        /// Label and wake id of each durable obligation.
        pub obligations: Vec<(&'static str, Uuid)>,
        /// The seat's recurring safety net (#2511): carried with its name,
        /// message and fire time.
        pub keepalive: Uuid,
        /// The backstop as armed, to compare after a transfer.
        pub keepalive_row: rsi_common::types::ScheduledJob,
        /// A predicate wake that is a protected manager watch.
        pub protected: Option<Uuid>,
        /// The live deploy the seat staged.
        pub live_deploy: Uuid,
        /// The live daemon job the seat runs.
        pub live_job: Uuid,
    }

    pub(crate) fn wake_row(
        owner: Uuid,
        mode: &str,
        name: &str,
        every: Option<i64>,
        watch: Option<Uuid>,
    ) -> rsi_common::types::ScheduledJob {
        build_agent_scheduled_job(ScheduleWakeRequest {
            message: format!("wake {name}"),
            in_seconds: Some(600),
            at: None,
            name: Some(name.into()),
            every_seconds: every,
            mode: Some(mode.into()),
            working_dir: std::path::PathBuf::from("/tmp/issue-2471"),
            provider: None,
            model: None,
            project_id: None,
            origin_session_id: Some(owner),
            watch_session_id: watch,
        })
        .unwrap()
    }

    pub(crate) fn job(owner: Uuid) -> crate::store::agent_jobs::NewAgentJob {
        let id = Uuid::new_v4();
        crate::store::agent_jobs::NewAgentJob {
            id,
            owner_session_id: owner,
            project_id: None,
            name: None,
            params: serde_json::from_value(serde_json::json!({
                "kind":"build", "command":"check", "workspace":true
            }))
            .unwrap(),
            cwd: "/tmp/issue-2471".into(),
            unit_name: format!("rsi-job-{id}"),
            log_path: format!("/tmp/{id}.log"),
            status_path: format!("/tmp/{id}.status"),
            idempotency_key: Some(format!("k-{id}")),
            wake: rsi_common::agent_jobs::JobWake::Owner,
        }
    }

    pub(crate) fn state(predicate: WakePredicate) -> WakeWhenState {
        WakeWhenState {
            predicate,
            armed_at: chrono::Utc::now(),
            deadline: None,
            repo_dir: None,
        }
    }

    /// Record, for `owner`: a completed job's outcome wake, an all-terminal
    /// batch wait, a batch wait on a still-live job, `issue_closed`,
    /// `todo_closed`, `topology_execution_settled`, `sha_on_rolling` and
    /// `health_anomaly` waits, a terminal watch, a settled deploy's outcome wake, a live
    /// deploy and a generic keep-alive. `protect` names `(project, epic)` for a
    /// predicate wake that is also a protected manager watch.
    pub(crate) fn arm(store: &Store, owner: Uuid, protect: Option<(Uuid, Uuid)>) -> Armed {
        use crate::store::agent_deploys::{NewDeploy, deploy_fingerprint_with};
        use rsi_common::agent_deploy::DeployState;
        const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
        let mut obligations = Vec::new();

        let done = job(owner);
        store.insert_agent_job(&done, chrono::Utc::now()).unwrap();
        let outcome = store
            .settle_agent_job(
                done.id,
                JobState::Succeeded,
                &AgentJobResultV1::default(),
                true,
                chrono::Utc::now(),
            )
            .unwrap()
            .expect("the settled job's outcome wake");
        obligations.push(("job outcome", outcome));

        let batch_member = job(owner);
        store
            .insert_agent_job(&batch_member, chrono::Utc::now())
            .unwrap();
        store
            .settle_agent_job(
                batch_member.id,
                JobState::Failed,
                &AgentJobResultV1::default(),
                false,
                chrono::Utc::now(),
            )
            .unwrap();
        let batch = wake_row(owner, "resume", "batch", None, None);
        store
            .insert_wake_when(
                &batch,
                &state(WakePredicate {
                    jobs_terminal: Some(vec![batch_member.id]),
                    ..WakePredicate::default()
                }),
                false,
            )
            .unwrap();
        obligations.push(("all-terminal batch", batch.id));

        let live = job(owner);
        store.insert_agent_job(&live, chrono::Utc::now()).unwrap();
        let live_batch = wake_row(owner, "resume", "live-batch", None, None);
        store
            .insert_wake_when(
                &live_batch,
                &state(WakePredicate {
                    jobs_terminal: Some(vec![live.id]),
                    ..WakePredicate::default()
                }),
                false,
            )
            .unwrap();
        obligations.push(("live batch", live_batch.id));

        for (label, predicate) in [
            (
                "issue_closed",
                WakePredicate {
                    issue_closed: Some(7),
                    ..WakePredicate::default()
                },
            ),
            (
                "todo_closed",
                WakePredicate {
                    todo_closed: Some(Uuid::new_v4()),
                    ..WakePredicate::default()
                },
            ),
            (
                "topology_execution_settled",
                WakePredicate {
                    topology_execution_settled: Some(Uuid::new_v4()),
                    ..WakePredicate::default()
                },
            ),
            (
                "sha_on_rolling",
                WakePredicate {
                    sha_on_rolling: Some(SHA.into()),
                    ..WakePredicate::default()
                },
            ),
            // #2747: a manager's standing health wait follows the seat.
            (
                "health_anomaly",
                WakePredicate {
                    health_anomaly: Some(rsi_common::health_anomaly::HealthAnomalySpec::default()),
                    ..WakePredicate::default()
                },
            ),
        ] {
            let row = wake_row(owner, "resume", label, None, None);
            store.insert_scheduled_job(&row).unwrap();
            store
                .set_wake_when_state_for_test(row.id, &state(predicate))
                .unwrap();
            obligations.push((label, row.id));
        }

        let watch = wake_row(
            owner,
            "on_terminal",
            "terminal-watch",
            None,
            Some(Uuid::new_v4()),
        );
        store.insert_scheduled_job(&watch).unwrap();
        obligations.push(("terminal watch", watch.id));

        fn deploy(owner: Uuid, key: &str) -> NewDeploy<'_> {
            NewDeploy {
                id: Uuid::new_v4(),
                owner_session_id: owner,
                idempotency_key: key,
                sha: SHA,
                fingerprint: deploy_fingerprint_with(SHA, "x", 60, false),
                manifest: &[],
                max_wait_secs: 60,
                interrupt_workers: false,
            }
        }
        let settled = store
            .insert_agent_deploy(&deploy(owner, "settled"), chrono::Utc::now())
            .unwrap();
        let deploy_wake = store
            .settle_agent_deploy(
                settled.id,
                DeployState::Failed,
                Some("test"),
                chrono::Utc::now(),
            )
            .unwrap()
            .expect("the settled deploy's outcome wake");
        obligations.push(("deploy outcome", deploy_wake));
        let live_deploy = store
            .insert_agent_deploy(&deploy(owner, "live"), chrono::Utc::now())
            .unwrap()
            .id;

        let keepalive = wake_row(owner, "resume", "keep-alive", Some(600), None);
        store.insert_scheduled_job(&keepalive).unwrap();

        let protected = protect.map(|(project, epic)| {
            let row = wake_row(owner, "resume", "protected", None, None);
            store.insert_scheduled_job(&row).unwrap();
            store
                .set_wake_when_state_for_test(
                    row.id,
                    &state(WakePredicate {
                        issue_closed: Some(8),
                        ..WakePredicate::default()
                    }),
                )
                .unwrap();
            store
                .conn
                .execute(
                    "INSERT INTO harness_manager_watches(job_id,project_id,epic_id,scope_version,direction,source_session_id,target_session_id,attention_signature)
                     VALUES(?1,?2,?3,1,'to_manager',?4,?4,'sig')",
                    rusqlite::params![
                        row.id.to_string(),
                        project.to_string(),
                        epic.to_string(),
                        owner.to_string()
                    ],
                )
                .unwrap();
            row.id
        });

        Armed {
            obligations,
            keepalive: keepalive.id,
            keepalive_row: keepalive,
            protected,
            live_deploy,
            live_job: live.id,
        }
    }

    fn wake_owner(store: &Store, id: Uuid) -> (Option<Uuid>, bool) {
        let row = store
            .get_scheduled_job(&id)
            .unwrap()
            .expect("retargeted, never deleted");
        (row.wake_session_id, row.enabled)
    }

    /// Every obligation now belongs to `successor` and is still armed; the
    /// live job and deploy followed; the protected watch kept `predecessor`.
    pub(crate) fn assert_carried(
        store: &Store,
        armed: &Armed,
        predecessor: Uuid,
        successor: Uuid,
        backstop_follows: bool,
    ) {
        for (label, id) in &armed.obligations {
            assert_eq!(
                wake_owner(store, *id),
                (Some(successor), true),
                "{label} reaches the successor"
            );
        }
        assert_eq!(
            store
                .get_agent_job(armed.live_job)
                .unwrap()
                .unwrap()
                .job
                .owner_session_id,
            successor
        );
        assert_eq!(
            store
                .get_agent_deploy(armed.live_deploy)
                .unwrap()
                .unwrap()
                .owner_session_id,
            Some(successor)
        );
        if let Some(protected) = armed.protected {
            assert_eq!(
                wake_owner(store, protected).0,
                Some(predecessor),
                "a protected manager watch keeps its owner"
            );
        }
        if !backstop_follows {
            assert_eq!(
                wake_owner(store, armed.keepalive),
                (Some(predecessor), true),
                "a displaced holder's continuation stays with it"
            );
            return;
        }
        assert_eq!(
            wake_owner(store, armed.keepalive),
            (Some(successor), true),
            "the backstop follows the seat"
        );
        let carried = store
            .list_owned_wake_views(successor, false, 64)
            .unwrap()
            .into_iter()
            .find(|wake| wake.job_id == armed.keepalive)
            .expect("the successor's AgentListWakes returns the backstop");
        assert_eq!(carried.name, armed.keepalive_row.name);
        assert_eq!(carried.mode, "resume");
        assert_eq!(carried.next_fire_at, armed.keepalive_row.next_fire_at);
        let row = store.get_scheduled_job(&armed.keepalive).unwrap().unwrap();
        assert_eq!(row.message, armed.keepalive_row.message);
    }

    /// Maintenance runs before delivery: it retires nothing the seat carried.
    pub(crate) fn assert_maintenance_keeps_obligations(store: &Store, armed: &Armed) {
        let retired = store.sweep_superseded_resume_wakes().unwrap();
        for (label, id) in &armed.obligations {
            assert!(!retired.contains(id), "{label} survives maintenance");
            assert!(wake_owner(store, *id).1, "{label} stays armed");
        }
        assert!(!retired.contains(&armed.protected.unwrap_or_default()));
    }

    /// Run one seat transfer in its own publication transaction.
    pub(crate) fn transfer(
        store: &Store,
        predecessor: Uuid,
        successor: Uuid,
    ) -> super::ObligationsMoved {
        let tx = rusqlite::Transaction::new_unchecked(
            &store.conn,
            rusqlite::TransactionBehavior::Immediate,
        )
        .unwrap();
        let moved =
            super::transfer_manager_obligations_on(&store.conn, predecessor, successor, true)
                .unwrap();
        tx.commit().unwrap();
        moved
    }

    /// A second transfer finds nothing: each obligation reaches the successor once
    /// (the backstop is checked by `assert_carried`).
    pub(crate) fn assert_second_transfer_is_empty(
        store: &Store,
        predecessor: Uuid,
        successor: Uuid,
    ) {
        let tx = rusqlite::Transaction::new_unchecked(
            &store.conn,
            rusqlite::TransactionBehavior::Immediate,
        )
        .unwrap();
        let moved =
            super::transfer_manager_obligations_on(&store.conn, predecessor, successor, false)
                .unwrap();
        tx.commit().unwrap();
        assert_eq!(moved, super::ObligationsMoved::default());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::super::Store;
    use super::test_support as ob;
    use rsi_common::agent_jobs::{AgentJobResultV1, JobState};
    use rsi_common::wake_predicate::WakePredicate;
    use uuid::Uuid;

    /// A stored session, so job and wake foreign keys resolve.
    fn seat(store: &Store) -> Uuid {
        let session = crate::test_support::test_session(
            Uuid::new_v4(),
            std::path::PathBuf::from("/tmp/issue-2483"),
        );
        store.insert_session(&session).unwrap();
        session.id
    }

    fn wake_owner(store: &Store, id: Uuid) -> Option<Uuid> {
        store
            .get_scheduled_job(&id)
            .unwrap()
            .expect("retargeted, never deleted")
            .wake_session_id
    }

    /// #2483: a terminal job row keeps its first owner, so a settled-job
    /// batch wait must keep following the seat on every later change.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn settled_batch_wait_follows_the_seat_across_two_changes_2483() {
        let store = Store::open_in_memory().unwrap();
        let (a, b, c) = (seat(&store), seat(&store), seat(&store));
        let armed = ob::arm(&store, a, None);

        ob::transfer(&store, a, b);
        ob::transfer(&store, b, c);

        // Every obligation, the settled-job batch wait included, reached C.
        ob::assert_carried(&store, &armed, a, c, true);
        let batch = armed
            .obligations
            .iter()
            .find(|(label, _)| *label == "all-terminal batch")
            .expect("the settled batch wait")
            .1;
        assert_eq!(wake_owner(&store, batch), Some(c));
        ob::assert_second_transfer_is_empty(&store, b, c);
    }

    /// A batch with a live member owned by an unrelated seat is not moved,
    /// whatever the terminal members are.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn batch_wait_with_a_live_job_of_an_unrelated_seat_stays_2483() {
        let store = Store::open_in_memory().unwrap();
        let (a, b, stranger) = (seat(&store), seat(&store), seat(&store));
        let settled = ob::job(a);
        store
            .insert_agent_job(&settled, chrono::Utc::now())
            .unwrap();
        store
            .settle_agent_job(
                settled.id,
                JobState::Succeeded,
                &AgentJobResultV1::default(),
                false,
                chrono::Utc::now(),
            )
            .unwrap();
        let foreign = ob::job(stranger);
        store
            .insert_agent_job(&foreign, chrono::Utc::now())
            .unwrap();
        let wait = ob::wake_row(a, "resume", "mixed-batch", None, None);
        store.insert_scheduled_job(&wait).unwrap();
        store
            .set_wake_when_state_for_test(
                wait.id,
                &ob::state(WakePredicate {
                    jobs_terminal: Some(vec![settled.id, foreign.id]),
                    ..WakePredicate::default()
                }),
            )
            .unwrap();

        ob::transfer(&store, a, b);

        assert_eq!(wake_owner(&store, wait.id), Some(a));
    }

    /// #2511: an overdue safety net follows the seat with its fire time; a name
    /// the successor already holds wins; the daemon's per-session rows stay;
    /// the handoff's carried list names exactly what moves.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn overdue_backstop_follows_the_seat_and_the_carried_list_is_exact_2511() {
        let store = Store::open_in_memory().unwrap();
        let (a, b) = (seat(&store), seat(&store));
        let overdue = ob::wake_row(a, "resume", "gm-safety-net", Some(3600), None);
        let past = (chrono::Utc::now() - chrono::Duration::hours(2))
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        store.insert_scheduled_job(&overdue).unwrap();
        store
            .conn
            .execute(
                "UPDATE scheduled_jobs SET next_fire_at=?2 WHERE id=?1",
                rusqlite::params![overdue.id.to_string(), past],
            )
            .unwrap();
        let contested = ob::wake_row(a, "resume", "contested", Some(600), None);
        store.insert_scheduled_job(&contested).unwrap();
        let held = ob::wake_row(b, "resume", "contested", Some(600), None);
        store.insert_scheduled_job(&held).unwrap();
        let daemon = ob::wake_row(a, "resume", "keepalive-session", None, None);
        store.insert_scheduled_job(&daemon).unwrap();
        let guard = ob::wake_row(a, "fresh", "other-mode", None, None);
        store.insert_scheduled_job(&guard).unwrap();

        let mut before = store.seat_change_carried_wake_ids(a).unwrap();
        before.sort();
        let mut expected = vec![overdue.id, contested.id];
        expected.sort();
        assert_eq!(before, expected, "the carried list is what a change moves");

        ob::transfer(&store, a, b);

        let moved = store.get_scheduled_job(&overdue.id).unwrap().unwrap();
        assert_eq!(moved.wake_session_id, Some(b));
        assert!(moved.enabled);
        assert_eq!(
            moved
                .next_fire_at
                .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            past,
            "the fire time is kept, so an overdue wake fires on the successor"
        );
        assert_eq!(wake_owner(&store, contested.id), Some(a), "b's name wins");
        assert_eq!(wake_owner(&store, held.id), Some(b));
        assert_eq!(wake_owner(&store, daemon.id), Some(a));
        assert_eq!(wake_owner(&store, guard.id), Some(a));
        ob::assert_second_transfer_is_empty(&store, a, b);
    }
}
