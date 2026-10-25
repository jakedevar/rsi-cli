//! Reuse only unchanged, write-free intent observations (#211 / hub #2494).
//!
//! #2939: an observation is keyed on its own inputs, not on every write of the
//! connection. An SQLite update hook counts the row writes of the tables the
//! intent pass reads ([`INTENT_INPUT_TABLES`]); `PRAGMA data_version` still
//! covers every commit of another connection. Inserted conversation events
//! (the busiest table, about one row a second on the hub) matter only for a
//! session still `Starting` (legacy zero spend) and a lead judged stale, so they
//! are re-checked by id range instead of invalidating every observation.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Duration, Utc};
use rusqlite::hooks::Action;
use serde_json::Value;
use uuid::Uuid;

use super::ManagerIntentReconciliationV2;
use crate::error::Result;
use crate::store::Store;

const MAX_PROJECTS: usize = 64;
const BACKSTOP_SECONDS: i64 = 30;
/// A held claim memo is re-derived at least this often.
pub(crate) const HELD_CLAIM_BACKSTOP_SECONDS: i64 = 30;

/// #2939: every table the intent pass reads (cohort, work, requests,
/// resources, the per-Epic loop), except `conversation_events` (see the module
/// docs) and the `json_each` table function. Sorted for `binary_search`; a test
/// compares it with the tables SQLite's authorizer reports for a full pass.
/// None is a WITHOUT ROWID table (the update hook does not see those).
pub(crate) const INTENT_INPUT_TABLES: &[&str] = &[
    "agent_successor_reservations",
    "approvals",
    "appserver_approval_publications",
    "daemon_settings",
    "epic_lead_generations",
    "global_manager_grants",
    "harness_manager_messages",
    "harness_manager_rotation_edges",
    "harness_manager_scopes",
    "harness_manager_v2_entities",
    "harness_manager_v2_events",
    "harness_manager_v2_operations",
    "harness_manager_v2_policies",
    "harness_manager_v2_records",
    "harness_manager_v2_work_facts",
    "manager_portfolio_appointments",
    "manager_portfolio_coverage",
    "manager_portfolio_nodes",
    "manager_review_assignments",
    "manager_review_findings",
    "manager_review_receipts",
    "manager_root_resource_origins",
    "manager_root_successions",
    "master_no_idle_capacity_incidents",
    "model_invocations",
    "projects",
    "provider_rate_limit_windows",
    "rotation_events",
    "sandbox_custody_roots",
    "scheduled_jobs",
    "session_lineage_detachments",
    "session_summaries",
    "session_tags",
    "sessions",
    "topology_executions",
    "topology_node_attempts",
];

const EVENTS_TABLE: &str = "conversation_events";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct ObservationVersion {
    local_changes: u64,
    external_changes: i64,
}

/// Row writes of this connection the update hook counted (#2939).
pub(crate) struct InputWrites {
    /// Writes to [`INTENT_INPUT_TABLES`], and deletes of conversation events.
    relevant: AtomicU64,
    /// Inserted conversation events.
    events: AtomicU64,
    /// #3486: `relevant`, split by table (index into [`INTENT_INPUT_TABLES`]),
    /// so a rejected observation can name the tables that moved.
    by_table: [AtomicU64; INPUT_TABLE_COUNT],
    /// Deleted conversation events (part of `relevant`).
    event_deletes: AtomicU64,
}

impl Default for InputWrites {
    fn default() -> Self {
        Self {
            relevant: AtomicU64::new(0),
            events: AtomicU64::new(0),
            by_table: std::array::from_fn(|_| AtomicU64::new(0)),
            event_deletes: AtomicU64::new(0),
        }
    }
}

const INPUT_TABLE_COUNT: usize = INTENT_INPUT_TABLES.len();

/// The intent pass's inputs as of one instant on this connection (#2939).
/// Equal versions mean no input table changed in between, except by inserted
/// conversation events, which [`IntentInputPin::events_since`] re-checks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct IntentInputVersion {
    relevant_writes: u64,
    external_changes: i64,
}

/// An [`IntentInputVersion`] plus the conversation-event position it saw.
#[derive(Clone, Copy, Debug)]
pub struct IntentInputPin {
    version: IntentInputVersion,
    event_inserts: u64,
    events_high_water: i64,
    /// #3486: per-table write counts at the pin ([`InputWrites::by_table`]).
    table_writes: [u64; INPUT_TABLE_COUNT],
    event_deletes: u64,
}

struct Observation {
    project: Uuid,
    pin: IntentInputPin,
    /// Leads this observation judged stale: a new event of one ends that.
    event_watch: Vec<Uuid>,
    observed_at: DateTime<Utc>,
    valid_until: DateTime<Utc>,
    result: ManagerIntentReconciliationV2,
}

/// #2494: the claim scan found nothing to claim but one create the host
/// admission gate refused. While nothing was written and no queued action
/// became due, the next tick need only ask the gate about that create again.
#[derive(Clone)]
pub(crate) struct HeldClaim {
    version: ObservationVersion,
    hold_worker_starts: bool,
    valid_until: DateTime<Utc>,
    pub(crate) operation_id: String,
    pub(crate) not_before: String,
    pub(crate) target: Option<Uuid>,
}

#[derive(Default)]
pub(crate) struct IdleIntentCache {
    entries: Vec<Observation>,
    /// Installed with the update hook on the first version read.
    input_writes: Option<Arc<InputWrites>>,
    held_claim: Option<HeldClaim>,
    /// #3486: when each project's intent pass last ran to completion on this
    /// connection (or was answered by its memo). Bounds how long a pass whose
    /// off-lock observation was rejected may be deferred.
    last_applied: std::collections::HashMap<Uuid, DateTime<Utc>>,
    /// Claim scans skipped by [`HeldClaim`] (observability and tests).
    pub(crate) held_claim_reuses: u64,
    // Incremented only by the test seam at the actual snapshot entry point.
    #[allow(dead_code)] // Production never increments the test-seam counter.
    pub(crate) snapshot_recomputations: u64,
}

impl IdleIntentCache {
    pub(crate) fn held_claim(
        &self,
        version: ObservationVersion,
        hold_worker_starts: bool,
        at: DateTime<Utc>,
    ) -> Option<HeldClaim> {
        self.held_claim
            .as_ref()
            .filter(|held| {
                held.version == version
                    && held.hold_worker_starts == hold_worker_starts
                    && at < held.valid_until
            })
            .cloned()
    }

    pub(crate) fn remember_held_claim(
        &mut self,
        version: ObservationVersion,
        hold_worker_starts: bool,
        valid_until: DateTime<Utc>,
        operation_id: String,
        not_before: String,
        target: Option<Uuid>,
    ) {
        self.held_claim = Some(HeldClaim {
            version,
            hold_worker_starts,
            valid_until,
            operation_id,
            not_before,
            target,
        });
    }

    pub(crate) fn forget_held_claim(&mut self) {
        self.held_claim = None;
    }

    fn entry(
        &self,
        project: Uuid,
        version: IntentInputVersion,
        at: DateTime<Utc>,
    ) -> Option<usize> {
        self.entries.iter().position(|entry| {
            entry.project == project
                && entry.pin.version == version
                && at >= entry.observed_at
                && at < entry.valid_until
        })
    }

    pub(super) fn remember(
        &mut self,
        project: Uuid,
        pin: IntentInputPin,
        event_watch: Vec<Uuid>,
        observed_at: DateTime<Utc>,
        valid_until: DateTime<Utc>,
        result: ManagerIntentReconciliationV2,
    ) {
        self.entries.retain(|entry| entry.project != project);
        if self.entries.len() == MAX_PROJECTS {
            self.entries.remove(0);
        }
        self.entries.push(Observation {
            project,
            pin,
            event_watch,
            observed_at,
            valid_until,
            result,
        });
    }
}

impl Store {
    /// #2939: the intent pass's input version, or `None` inside a transaction
    /// (its writes could still roll back). The first call installs the update
    /// hook that counts input writes; nothing else on this connection may own
    /// the update hook.
    pub fn manager_intent_input_version(&self) -> Result<Option<IntentInputVersion>> {
        if !self.conn.is_autocommit() {
            return Ok(None);
        }
        let writes = self.manager_intent_input_writes();
        Ok(Some(IntentInputVersion {
            relevant_writes: writes.relevant.load(Ordering::Relaxed),
            external_changes: self.data_version()?,
        }))
    }

    /// [`Self::manager_intent_input_version`] with the conversation-event
    /// position, for an observation computed now or on another connection.
    pub fn manager_intent_input_pin(&self) -> Result<Option<IntentInputPin>> {
        let Some(version) = self.manager_intent_input_version()? else {
            return Ok(None);
        };
        let event_inserts = self
            .manager_intent_input_writes()
            .events
            .load(Ordering::Relaxed);
        let events_high_water = self
            .conn
            .prepare_cached("SELECT COALESCE(MAX(id),0) FROM conversation_events")?
            .query_row([], |row| row.get(0))?;
        let writes = self.manager_intent_input_writes();
        Ok(Some(IntentInputPin {
            version,
            event_inserts,
            events_high_water,
            table_writes: std::array::from_fn(|i| writes.by_table[i].load(Ordering::Relaxed)),
            event_deletes: writes.event_deletes.load(Ordering::Relaxed),
        }))
    }

    /// Whether the inputs still equal `pin` for an observation whose stale
    /// leads are `watch`. Also usable inside the pass's own transaction.
    pub(crate) fn manager_intent_inputs_unchanged(
        &self,
        pin: &IntentInputPin,
        watch: &[Uuid],
    ) -> Result<bool> {
        Ok(self.manager_intent_input_change(pin, watch)?.is_none())
    }

    /// #3486: why the inputs no longer equal `pin`, or `None` while they do.
    /// The reason names what moved: `external_commit` (another connection
    /// committed), `tables:<a>,<b>` (this connection wrote those input tables)
    /// or `conversation_events` (an event that matters was inserted).
    pub(crate) fn manager_intent_input_change(
        &self,
        pin: &IntentInputPin,
        watch: &[Uuid],
    ) -> Result<Option<String>> {
        let writes = self.manager_intent_input_writes();
        let external = self.data_version()? != pin.version.external_changes;
        if writes.relevant.load(Ordering::Relaxed) != pin.version.relevant_writes || external {
            let mut moved: Vec<&str> = INTENT_INPUT_TABLES
                .iter()
                .zip(&pin.table_writes)
                .zip(&writes.by_table)
                .filter(|((_, at_pin), now)| now.load(Ordering::Relaxed) != **at_pin)
                .map(|((table, _), _)| *table)
                .collect();
            if writes.event_deletes.load(Ordering::Relaxed) != pin.event_deletes {
                moved.push(EVENTS_TABLE);
            }
            let mut reason = Vec::new();
            if external {
                reason.push("external_commit".to_owned());
            }
            if !moved.is_empty() {
                reason.push(format!("tables:{}", moved.join(",")));
            }
            return Ok(Some(reason.join(";")));
        }
        if writes.events.load(Ordering::Relaxed) == pin.event_inserts {
            return Ok(None);
        }
        Ok(self
            .manager_intent_events_matter(pin.events_high_water, watch)?
            .then(|| EVENTS_TABLE.to_owned()))
    }

    /// #3486: record that `project`'s intent pass completed now.
    pub(super) fn manager_intent_note_applied(&self, project: Uuid, at: DateTime<Utc>) {
        self.manager_intent_idle
            .borrow_mut()
            .last_applied
            .insert(project, at);
    }

    /// #3486: whether `project`'s intent pass completed within `window` of `at`.
    pub(super) fn manager_intent_applied_within(
        &self,
        project: Uuid,
        at: DateTime<Utc>,
        window: Duration,
    ) -> bool {
        self.manager_intent_idle
            .borrow()
            .last_applied
            .get(&project)
            .is_some_and(|last| at >= *last && at - *last < window)
    }

    /// An event inserted after `high_water` for a session that is `Starting`
    /// (its legacy spend reads "no events yet") or for a watched stale lead.
    fn manager_intent_events_matter(&self, high_water: i64, watch: &[Uuid]) -> Result<bool> {
        Ok(self
            .conn
            .prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM conversation_events e
               LEFT JOIN sessions s ON s.id=e.session_id
              WHERE e.id>?1 AND (s.status='Starting'
                 OR e.session_id IN (SELECT value FROM json_each(?2))))",
            )?
            .query_row(
                rusqlite::params![high_water, serde_json::to_string(watch)?],
                |row| row.get(0),
            )?)
    }

    /// The memoized result of `project`'s last write-free pass, when its inputs
    /// are unchanged and its deadline has not passed.
    pub(super) fn manager_intent_memo(
        &self,
        project: Uuid,
        at: DateTime<Utc>,
    ) -> Result<Option<ManagerIntentReconciliationV2>> {
        let Some(version) = self.manager_intent_input_version()? else {
            return Ok(None);
        };
        let (index, pin, watch) = {
            let cache = self.manager_intent_idle.borrow();
            let Some(index) = cache.entry(project, version, at) else {
                return Ok(None);
            };
            let entry = &cache.entries[index];
            (index, entry.pin, entry.event_watch.clone())
        };
        if !self.manager_intent_inputs_unchanged(&pin, &watch)? {
            return Ok(None);
        }
        let events = self
            .manager_intent_input_writes()
            .events
            .load(Ordering::Relaxed);
        if events != pin.event_inserts {
            // Only unrelated events arrived: move the memo past them so the
            // next tick does not re-read the same range.
            let Some(advanced) = self.manager_intent_input_pin()? else {
                return Ok(None);
            };
            self.manager_intent_idle.borrow_mut().entries[index].pin = advanced;
        }
        let mut result = self.manager_intent_idle.borrow().entries[index]
            .result
            .clone();
        // The memo reuses decisions, not the previous pass's read activity.
        // A full pass may have populated it under the lock; this hit reads
        // none of the cohort/work/request/resource snapshot again (#3047).
        result.read_under_lock = false;
        Ok(Some(result))
    }

    fn manager_intent_input_writes(&self) -> Arc<InputWrites> {
        if let Some(writes) = &self.manager_intent_idle.borrow().input_writes {
            return Arc::clone(writes);
        }
        let writes = Arc::new(InputWrites::default());
        let counter = Arc::clone(&writes);
        self.conn.update_hook(Some(
            move |action: Action, _db: &str, table: &str, _rowid: i64| {
                if table == EVENTS_TABLE {
                    // Production updates of an event rewrite only `content` or
                    // `metadata`, which the pass never reads (pinned by a test).
                    match action {
                        Action::SQLITE_INSERT => counter.events.fetch_add(1, Ordering::Relaxed),
                        Action::SQLITE_DELETE => {
                            counter.event_deletes.fetch_add(1, Ordering::Relaxed);
                            counter.relevant.fetch_add(1, Ordering::Relaxed)
                        }
                        _ => 0,
                    };
                } else if let Ok(index) = INTENT_INPUT_TABLES.binary_search(&table) {
                    counter.by_table[index].fetch_add(1, Ordering::Relaxed);
                    counter.relevant.fetch_add(1, Ordering::Relaxed);
                }
            },
        ));
        self.manager_intent_idle.borrow_mut().input_writes = Some(Arc::clone(&writes));
        writes
    }

    fn data_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .prepare_cached("PRAGMA data_version")?
            .query_row([], |row| row.get(0))?)
    }

    /// Both signals are needed: data_version changes for another connection's
    /// commits, while total_changes includes this connection's trigger writes
    /// and rolled-back writes. Never memoize inside a transaction: its values
    /// could survive rollback without a subsequent total_changes increment.
    pub(crate) fn manager_intent_observation_version(&self) -> Result<Option<ObservationVersion>> {
        if !self.conn.is_autocommit() {
            return Ok(None);
        }
        let external_changes = self
            .conn
            .prepare_cached("PRAGMA data_version")?
            .query_row([], |row| row.get(0))?;
        Ok(Some(ObservationVersion {
            local_changes: self.conn.total_changes(),
            external_changes,
        }))
    }
}

/// The intent observer's time-dependent inputs are request overdue flags and
/// provider usage resets. Seat retry/backoff and queued action not_before are
/// evaluated by their separate, uncached runners. A backstop also retries
/// transient refusals and observations that are not represented by a DB write.
pub(super) fn next_observation_at(
    at: DateTime<Utc>,
    requests: &[Value],
    resources: &Value,
) -> DateTime<Utc> {
    let mut next = at + Duration::seconds(BACKSTOP_SECONDS);
    for request in requests {
        if let Some(deadline) = request["deadline"]
            .as_str()
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&Utc))
            && deadline >= at
        {
            next = next.min(deadline);
        }
    }
    if let Some(providers) = resources["provider_windows"].as_array() {
        for provider in providers {
            if let Some(windows) = provider["windows"].as_array() {
                for window in windows {
                    if let Some(reset) = window["resets_at_epoch"]
                        .as_i64()
                        .and_then(|epoch| DateTime::from_timestamp(epoch, 0))
                        && reset >= at
                    {
                        next = next.min(reset);
                    }
                }
            }
        }
    }
    next
}

#[cfg(test)]
mod input_tests;
#[cfg(test)]
mod tests;
