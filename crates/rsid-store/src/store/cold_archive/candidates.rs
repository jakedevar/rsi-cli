//! The cold set: which sessions are candidates, which are held back, and which
//! of a cold session's rows move (plan §2.2-§2.4, as revised by review #2392).
//!
//! * **Base predicate.** `Archived`/`Deleted` sessions whose newest conversation
//!   event is at least `cold_archive_after_days` old. Activity is the newest
//!   *event*, not `sessions.updated_at`, because sandbox maintenance bulk-bumps
//!   `updated_at` on archived rows. Timestamps are parsed in Rust, never compared
//!   as strings: stored values mix sub-second precision and legacy offsets. The
//!   newest-by-sequence row is only a cheap prefilter; [`prove_age`] reads every
//!   `created_at` of the session and requires all of them to parse strictly and
//!   to be older than the cutoff, so every row of a cold session is provably old.
//! * **Holds.** A session something live still points at is never cold. Any one
//!   hold keeps it, and the pass counts the session under that hold's reason.
//!   A state-bearing hold is a *settled-state exclusion*: a referencing row holds
//!   the session unless its state is in that hold's settled list, bound as JSON
//!   from [`HOLD_STATE_CLASSES`]. An unknown, new or NULL state therefore holds.
//!   [`check_hold_classification`] proves, against the schema, that every
//!   admitted state is classified live or settled.
//! * **Live tail.** Only `ToolUse`/`ToolResult` rows move. Every other row, the
//!   newest row, the newest provider row, the two `MAX(created_at)` witness rows
//!   and every row another table points at stay in `conversation_events`.

use super::format::{ColumnRoles, table_columns};
use super::{ColdError, ColdResult};
use crate::store::row_mappers::parse_timestamp;
use chrono::{DateTime, Utc};
use rsi_common::types::SessionStatus;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OptionalExtension};

/// Sessions read per candidate page.
pub const CANDIDATE_PAGE: usize = 256;

/// The columns of `conversation_events` the archive encodes, in order. The pass
/// refuses to archive when the live table no longer has exactly these.
pub const EVENT_COLUMNS: [&str; 11] = [
    "id",
    "session_id",
    "sequence",
    "event_type",
    "role",
    "content",
    "tool_name",
    "tool_input",
    "created_at",
    "tool_use_id",
    "metadata",
];

/// The columns of `conversation_event_provenance` the archive encodes.
pub const PROVENANCE_COLUMNS: [&str; 5] = [
    "conversation_event_id",
    "producer_kind",
    "model_invocation_id",
    "provider_event_type",
    "created_at",
];

pub const EVENTS_TABLE: &str = "conversation_events";
pub const PROVENANCE_TABLE: &str = "conversation_event_provenance";

/// Which columns carry the id, sequence and content in each table.
pub const EVENT_ROLES: ColumnRoles = ColumnRoles {
    id: Some(0),
    sequence: Some(2),
    content: Some(5),
};
pub const PROVENANCE_ROLES: ColumnRoles = ColumnRoles {
    id: Some(0),
    sequence: None,
    content: None,
};

/// Every foreign key into `conversation_events(id)` other than provenance
/// (`(table, column)`). A test compares it with `pragma_foreign_key_list`, so a
/// new reference cannot go unnoticed: a referenced row must stay live (`RESTRICT`
/// would refuse the move, `CASCADE` would silently delete the referencing row).
#[cfg(test)]
pub const EVENT_FK_REFERENCES: [(&str, &str); 5] = [
    ("agent_message_delivery_attempts", "acknowledged_event_id"),
    (
        "agent_message_provider_request_effects",
        "evidence_event_id",
    ),
    ("closure_output_validations", "conversation_event_id"),
    ("closure_evidence", "review_handoff_event_id"),
    ("pending_question_publications", "conversation_event_id"),
];

/// The rows of one session that move, in archive order. Parameter `?1` is the
/// session id.
///
/// * T1 only `ToolUse` and `ToolResult` rows move; every other row type stays, so a
///   row type added later stays live by default.
/// * T2 the newest row (by sequence) stays.
/// * T3 the newest provider row (`role = 'Assistant'` or `Thinking`) stays.
/// * T5 the row that attains the TEXT `MAX(created_at)` stays (ties to the newest
///   `(sequence, id)`): `created_at` is `TEXT NOT NULL`, so `ORDER BY created_at DESC`
///   under the BINARY collation reaches the same value `MAX(created_at)` does.
/// * T6 the same over non-`System` rows.
/// * T4 a row another table points at stays (`EVENT_FK_REFERENCES`).
///
/// The three statements below repeat the same filter as plain literals (the
/// protected-DML source audit needs every SQL string at a sink to be statically
/// known); `movable_statements_share_one_filter` compares them.
pub const MOVABLE_EVENTS_SQL: &str = "SELECT e.id, e.session_id, e.sequence, e.event_type, e.role, e.content, e.tool_name,
        e.tool_input, e.created_at, e.tool_use_id, e.metadata
 FROM conversation_events e
 WHERE e.session_id = ?1
  AND e.event_type IN ('ToolUse','ToolResult')
  AND e.id <> (SELECT id FROM conversation_events WHERE session_id = ?1
               ORDER BY sequence DESC, id DESC LIMIT 1)
  AND e.id IS NOT (SELECT id FROM conversation_events WHERE session_id = ?1
               AND (role = 'Assistant' OR event_type = 'Thinking')
               ORDER BY sequence DESC, id DESC LIMIT 1)
  AND e.id <> (SELECT id FROM conversation_events WHERE session_id = ?1
               ORDER BY created_at DESC, sequence DESC, id DESC LIMIT 1)
  AND e.id IS NOT (SELECT id FROM conversation_events WHERE session_id = ?1
               AND event_type <> 'System'
               ORDER BY created_at DESC, sequence DESC, id DESC LIMIT 1)
  AND NOT EXISTS (SELECT 1 FROM agent_message_delivery_attempts x WHERE x.acknowledged_event_id = e.id)
  AND NOT EXISTS (SELECT 1 FROM agent_message_provider_request_effects x WHERE x.evidence_event_id = e.id)
  AND NOT EXISTS (SELECT 1 FROM closure_output_validations x WHERE x.conversation_event_id = e.id)
  AND NOT EXISTS (SELECT 1 FROM closure_evidence x WHERE x.review_handoff_event_id = e.id)
  AND NOT EXISTS (SELECT 1 FROM pending_question_publications x WHERE x.conversation_event_id = e.id)
 ORDER BY e.sequence, e.id";

/// The provenance rows of the movable events of `?1`.
pub const MOVABLE_PROVENANCE_SQL: &str = "SELECT p.conversation_event_id, p.producer_kind, p.model_invocation_id,
        p.provider_event_type, p.created_at
 FROM conversation_event_provenance p
 WHERE p.conversation_event_id IN (SELECT e.id FROM conversation_events e
 WHERE e.session_id = ?1
  AND e.event_type IN ('ToolUse','ToolResult')
  AND e.id <> (SELECT id FROM conversation_events WHERE session_id = ?1
               ORDER BY sequence DESC, id DESC LIMIT 1)
  AND e.id IS NOT (SELECT id FROM conversation_events WHERE session_id = ?1
               AND (role = 'Assistant' OR event_type = 'Thinking')
               ORDER BY sequence DESC, id DESC LIMIT 1)
  AND e.id <> (SELECT id FROM conversation_events WHERE session_id = ?1
               ORDER BY created_at DESC, sequence DESC, id DESC LIMIT 1)
  AND e.id IS NOT (SELECT id FROM conversation_events WHERE session_id = ?1
               AND event_type <> 'System'
               ORDER BY created_at DESC, sequence DESC, id DESC LIMIT 1)
  AND NOT EXISTS (SELECT 1 FROM agent_message_delivery_attempts x WHERE x.acknowledged_event_id = e.id)
  AND NOT EXISTS (SELECT 1 FROM agent_message_provider_request_effects x WHERE x.evidence_event_id = e.id)
  AND NOT EXISTS (SELECT 1 FROM closure_output_validations x WHERE x.conversation_event_id = e.id)
  AND NOT EXISTS (SELECT 1 FROM closure_evidence x WHERE x.review_handoff_event_id = e.id)
  AND NOT EXISTS (SELECT 1 FROM pending_question_publications x WHERE x.conversation_event_id = e.id))
 ORDER BY p.conversation_event_id";

/// `COUNT`, `MIN(sequence)`, `MAX(sequence)` of the movable rows of `?1`.
pub const MOVABLE_COUNT_SQL: &str = "SELECT COUNT(*), MIN(e.sequence), MAX(e.sequence)
 FROM conversation_events e
 WHERE e.session_id = ?1
  AND e.event_type IN ('ToolUse','ToolResult')
  AND e.id <> (SELECT id FROM conversation_events WHERE session_id = ?1
               ORDER BY sequence DESC, id DESC LIMIT 1)
  AND e.id IS NOT (SELECT id FROM conversation_events WHERE session_id = ?1
               AND (role = 'Assistant' OR event_type = 'Thinking')
               ORDER BY sequence DESC, id DESC LIMIT 1)
  AND e.id <> (SELECT id FROM conversation_events WHERE session_id = ?1
               ORDER BY created_at DESC, sequence DESC, id DESC LIMIT 1)
  AND e.id IS NOT (SELECT id FROM conversation_events WHERE session_id = ?1
               AND event_type <> 'System'
               ORDER BY created_at DESC, sequence DESC, id DESC LIMIT 1)
  AND NOT EXISTS (SELECT 1 FROM agent_message_delivery_attempts x WHERE x.acknowledged_event_id = e.id)
  AND NOT EXISTS (SELECT 1 FROM agent_message_provider_request_effects x WHERE x.evidence_event_id = e.id)
  AND NOT EXISTS (SELECT 1 FROM closure_output_validations x WHERE x.conversation_event_id = e.id)
  AND NOT EXISTS (SELECT 1 FROM closure_evidence x WHERE x.review_handoff_event_id = e.id)
  AND NOT EXISTS (SELECT 1 FROM pending_question_publications x WHERE x.conversation_event_id = e.id)";

/// Every event row of `?1`.
pub const TOTAL_EVENTS_SQL: &str = "SELECT COUNT(*) FROM conversation_events WHERE session_id = ?1";

/// Every provenance row of an event of `?1`.
pub const TOTAL_PROVENANCE_SQL: &str = "SELECT COUNT(*) FROM conversation_event_provenance p
     WHERE p.conversation_event_id IN
           (SELECT id FROM conversation_events WHERE session_id = ?1)";

/// Every `created_at` of the events of `?1` (the age proof).
pub const SESSION_CREATED_AT_SQL: &str =
    "SELECT created_at FROM conversation_events WHERE session_id = ?1";

/// Where a classified column's admitted set comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admitted {
    /// The first `column IN ('a','b',...)` in the table's `CHECK` constraints.
    CheckList,
    /// `sessions.status` has no `CHECK`. The admitted set is the `SessionStatus`
    /// names ([`session_status_names`]) plus every status stored in `sessions`:
    /// the enum is `#[non_exhaustive]`, so no `match` can force a new variant to
    /// be classified, but a status in use that is not classified fails the check.
    SessionStatus,
}

/// One state-bearing column a hold reads: which of its values keep a session live
/// and which let it go. `settled` is bound as the JSON array of `?param`; any
/// other value (an unknown or newly added state) holds the session.
#[derive(Debug, Clone, Copy)]
pub struct StateClass {
    /// The hold this column belongs to (a [`HOLDS`] code).
    pub hold: &'static str,
    /// The positional parameter of the hold's SQL (`json_each(?N)`), from 2.
    pub param: usize,
    pub table: &'static str,
    pub column: &'static str,
    pub live: &'static [&'static str],
    pub settled: &'static [&'static str],
    pub admitted: Admitted,
}

/// Every state class, one per state-bearing column of a hold. Every column here
/// is `NOT NULL`, so a NULL cannot occur; a nullable column would need an
/// explicit `null_is` and the hand-written `IS NULL OR` in its hold's SQL
/// (`issue_tracker_dispatches.terminal_state` is that case and has its own hold).
pub const HOLD_STATE_CLASSES: [StateClass; 20] = [
    StateClass {
        hold: "agent_job",
        param: 2,
        table: "agent_jobs",
        column: "state",
        live: &["queued", "running"],
        settled: &["succeeded", "failed", "lost"],
        admitted: Admitted::CheckList,
    },
    StateClass {
        hold: "queue_entry",
        param: 2,
        table: "rolling_queue_entries",
        column: "state",
        live: &["queued", "admitted", "gating"],
        settled: &["published", "refused", "failed", "superseded"],
        admitted: Admitted::CheckList,
    },
    StateClass {
        hold: "topology",
        param: 2,
        table: "topology_executions",
        column: "status",
        live: &["accepted", "running", "cancelling", "blocked"],
        settled: &["succeeded", "failed", "cancelled"],
        admitted: Admitted::CheckList,
    },
    StateClass {
        hold: "manager_seat",
        param: 2,
        table: "manager_nodes",
        column: "state",
        live: &["active"],
        settled: &["revoked"],
        admitted: Admitted::CheckList,
    },
    StateClass {
        hold: "manager_seat",
        param: 3,
        table: "global_manager_grants",
        column: "state",
        live: &["active"],
        settled: &["revoked"],
        admitted: Admitted::CheckList,
    },
    StateClass {
        hold: "succession",
        param: 2,
        table: "agent_successor_reservations",
        column: "state",
        live: &["reserved", "launching", "uncertain"],
        settled: &["committed", "failed"],
        admitted: Admitted::CheckList,
    },
    StateClass {
        hold: "succession",
        param: 3,
        table: "manager_root_successions",
        column: "state",
        live: &[
            "reserved",
            "executing",
            "established",
            "cleanup_required",
            "blocked",
        ],
        settled: &["committed", "failed", "revoked"],
        admitted: Admitted::CheckList,
    },
    StateClass {
        hold: "review",
        param: 2,
        table: "manager_review_assignments",
        column: "state",
        live: &["reserved", "allocating", "active"],
        settled: &["submitted", "superseded", "cancelled", "failed"],
        admitted: Admitted::CheckList,
    },
    StateClass {
        hold: "manager_operation",
        param: 2,
        table: "harness_manager_v2_operations",
        column: "state",
        live: &["queued", "running", "uncertain"],
        settled: &["succeeded", "failed", "blocked", "revoked"],
        admitted: Admitted::CheckList,
    },
    StateClass {
        hold: "agent_mail",
        param: 2,
        table: "agent_messages",
        column: "state",
        live: &["queued", "claimed", "injected", "uncertain"],
        settled: &["acknowledged", "failed", "expired"],
        admitted: Admitted::CheckList,
    },
    StateClass {
        hold: "spawn_restart_watch",
        param: 2,
        table: "agent_spawn_requests",
        column: "state",
        live: &["reserved", "queued", "launching"],
        settled: &["launched", "failed"],
        admitted: Admitted::CheckList,
    },
    StateClass {
        hold: "spawn_restart_watch",
        param: 3,
        table: "daemon_restart_intents",
        column: "state",
        live: &["pending", "claimed"],
        settled: &["delivered", "failed"],
        admitted: Admitted::CheckList,
    },
    StateClass {
        hold: "spawn_restart_watch",
        param: 4,
        table: "agent_child_watch_witness",
        column: "state",
        live: &["pending", "armed"],
        settled: &["consumed", "abandoned", "disabled", "deleted"],
        admitted: Admitted::CheckList,
    },
    StateClass {
        hold: "custody_authority",
        param: 2,
        table: "provider_turn_custody",
        column: "state",
        live: &["live", "adopted"],
        settled: &["finished", "abandoned"],
        admitted: Admitted::CheckList,
    },
    StateClass {
        hold: "custody_authority",
        param: 3,
        table: "sandbox_custody_roots",
        column: "state",
        live: &["live"],
        settled: &["purged", "failed", "quarantined"],
        admitted: Admitted::CheckList,
    },
    StateClass {
        hold: "custody_authority",
        param: 4,
        table: "execution_origin_claims",
        column: "phase",
        live: &[
            "claimed",
            "launch_ready",
            "launching",
            "provider_live",
            "recovery_quarantined",
            "settling",
            "quarantined",
        ],
        settled: &["settled", "failed", "abandoned"],
        admitted: Admitted::CheckList,
    },
    StateClass {
        hold: "settlement",
        param: 2,
        table: "source_worktree_settlement_items",
        column: "phase",
        live: &[
            "planned",
            "intent_committed",
            "worktree_removed",
            "branch_removed",
            "recovery_required",
            "unattempted",
        ],
        settled: &["settled", "refused"],
        admitted: Admitted::CheckList,
    },
    StateClass {
        hold: "settlement",
        param: 3,
        table: "archive_cleanup_runs",
        column: "phase",
        live: &[
            "intent_committed",
            "quarantined",
            "removal_authorized",
            "worktree_removed",
            "recovery_required",
        ],
        settled: &["settled", "refused"],
        admitted: Admitted::CheckList,
    },
    StateClass {
        hold: "settlement",
        param: 4,
        table: "closure_sources",
        column: "state",
        live: &[
            "working",
            "outcome_committed",
            "outcome_no_change",
            "outcome_blocker",
            "outcome_blocked",
            "awaiting_evidence",
            "eligible",
            "integration_queued",
        ],
        settled: &["integrated", "retained", "cleanup_complete"],
        admitted: Admitted::CheckList,
    },
    StateClass {
        hold: "lineage",
        param: 2,
        table: "sessions",
        column: "status",
        live: &["Starting", "Running", "WaitingApproval"],
        settled: &["Completed", "Failed", "Interrupted", "Archived", "Deleted"],
        admitted: Admitted::SessionStatus,
    },
];

/// One never-cold hold per entry: `(reason code, SQL)`. The SQL is a single
/// `SELECT` yielding 0/1 over `?1`, the session id (a live reference to it), plus
/// `?2...` JSON arrays of settled states from [`HOLD_STATE_CLASSES`], in
/// parameter order. H17 (recently restored) and H20 (usage-limit window) need the
/// clock and are [`recently_restored`] and [`usage_limit_window_hold`].
pub const HOLDS: [(&str, &str); 18] = [
    // H1 wake (an enabled job names the session; NULL `enabled` holds).
    ("wake", "SELECT EXISTS(SELECT 1 FROM scheduled_jobs
              WHERE (enabled IS NULL OR enabled<>0)
                AND (wake_session_id=?1 OR wake_mode='on_terminal:'||?1))"),
    // H1b wake lineage: a delivery resolves to the lineage tip, so a descendant (through
    // `continued_from`) of an enabled job's target is held. Past the depth cap it holds.
    ("wake_lineage", "WITH RECURSIVE up(id, depth) AS (
                SELECT ?1, 0
                UNION ALL
                SELECT s.continued_from, u.depth+1 FROM sessions s JOIN up u ON s.id=u.id
                 WHERE s.continued_from IS NOT NULL AND u.depth<64)
              SELECT EXISTS(SELECT 1 FROM up u JOIN scheduled_jobs j
                             ON (j.wake_session_id=u.id OR j.wake_mode='on_terminal:'||u.id)
                            WHERE u.depth>0 AND (j.enabled IS NULL OR j.enabled<>0))
                  OR EXISTS(SELECT 1 FROM up WHERE depth>=64)"),
    ("agent_job", "SELECT EXISTS(SELECT 1 FROM agent_jobs
              WHERE owner_session_id=?1
                AND state NOT IN (SELECT value FROM json_each(?2)))"),
    ("queue_entry", "SELECT EXISTS(SELECT 1 FROM rolling_queue_entries
              WHERE (source_session_id=?1 OR wake_session_id=?1)
                AND state NOT IN (SELECT value FROM json_each(?2)))"),
    ("topology", "SELECT EXISTS(SELECT 1 FROM topology_node_attempts a
                JOIN topology_executions t ON t.id=a.execution_id
              WHERE a.session_id=?1
                AND t.status NOT IN (SELECT value FROM json_each(?2)))
              OR EXISTS(SELECT 1 FROM topology_executions
              WHERE (parent_session_id=?1 OR requested_by_session_id=?1)
                AND status NOT IN (SELECT value FROM json_each(?2)))"),
    ("manager_seat", "SELECT EXISTS(SELECT 1 FROM harness_manager_scopes WHERE manager_session_id=?1)
              OR EXISTS(SELECT 1 FROM global_manager_appointments WHERE session_id=?1)
              OR EXISTS(SELECT 1 FROM manager_portfolio_appointments WHERE session_id=?1)
              OR EXISTS(SELECT 1 FROM manager_nodes
                        WHERE seat_root_session_id=?1
                          AND state NOT IN (SELECT value FROM json_each(?2)))
              OR EXISTS(SELECT 1 FROM global_manager_grants
                        WHERE seat_session_id=?1
                          AND state NOT IN (SELECT value FROM json_each(?3)))"),
    ("succession", "SELECT EXISTS(SELECT 1 FROM agent_successor_reservations
              WHERE (predecessor_session_id=?1 OR candidate_session_id=?1)
                AND state NOT IN (SELECT value FROM json_each(?2)))
              OR EXISTS(SELECT 1 FROM manager_root_successions
              WHERE (manager_session_id=?1 OR predecessor_session_id=?1
                     OR candidate_session_id=?1)
                AND state NOT IN (SELECT value FROM json_each(?3)))"),
    ("review", "SELECT EXISTS(SELECT 1 FROM manager_review_assignments
              WHERE (author_session_id=?1 OR reviewer_session_id=?1)
                AND state NOT IN (SELECT value FROM json_each(?2)))"),
    ("watch", "SELECT EXISTS(SELECT 1 FROM harness_manager_watches
              WHERE source_session_id=?1 OR target_session_id=?1)"),
    ("manager_operation", "SELECT EXISTS(SELECT 1 FROM harness_manager_v2_operations
              WHERE target_session_id=?1
                AND state NOT IN (SELECT value FROM json_each(?2)))"),
    ("agent_mail", "SELECT EXISTS(SELECT 1 FROM agent_messages
              WHERE (owner_session_id=?1 OR target_session_id=?1)
                AND state NOT IN (SELECT value FROM json_each(?2)))"),
    ("spawn_restart_watch", "SELECT EXISTS(SELECT 1 FROM agent_spawn_requests
              WHERE (owner_session_id=?1 OR child_session_id=?1)
                AND state NOT IN (SELECT value FROM json_each(?2)))
              OR EXISTS(SELECT 1 FROM daemon_restart_intents
              WHERE session_id=?1 AND state NOT IN (SELECT value FROM json_each(?3)))
              OR EXISTS(SELECT 1 FROM agent_child_watch_witness
              WHERE (owner_session_id=?1 OR child_session_id=?1)
                AND state NOT IN (SELECT value FROM json_each(?4)))
              OR EXISTS(SELECT 1 FROM agent_child_relaunch_intents
              WHERE (caller_session_id=?1 OR target_session_id=?1 OR tip_session_id=?1)
                AND settled_at IS NULL)"),
    ("custody_authority", "SELECT EXISTS(SELECT 1 FROM provider_turn_custody
              WHERE session_id=?1 AND state NOT IN (SELECT value FROM json_each(?2)))
              OR EXISTS(SELECT 1 FROM sandbox_custody_roots
              WHERE owner_session_id=?1 AND state NOT IN (SELECT value FROM json_each(?3)))
              OR EXISTS(SELECT 1 FROM execution_origin_claims
              WHERE (claimant_session_id=?1 OR source_session_id=?1
                     OR requested_origin_session_id=?1)
                AND phase NOT IN (SELECT value FROM json_each(?4)))"),
    ("settlement", "SELECT EXISTS(SELECT 1 FROM source_worktree_settlement_items
              WHERE session_id=?1 AND phase NOT IN (SELECT value FROM json_each(?2)))
              OR EXISTS(SELECT 1 FROM archive_cleanup_runs
              WHERE session_id=?1 AND phase NOT IN (SELECT value FROM json_each(?3)))
              OR EXISTS(SELECT 1 FROM closure_source_sessions cs
                JOIN closure_sources c ON c.id=cs.source_id
              WHERE cs.session_id=?1
                AND c.state NOT IN (SELECT value FROM json_each(?4)))"),
    // H14: a rotation successor that is not settled.
    ("lineage", "SELECT EXISTS(SELECT 1 FROM sessions c WHERE c.continued_from=?1
                AND c.status NOT IN (SELECT value FROM json_each(?2)))"),
    ("tracker_dispatch", "SELECT EXISTS(SELECT 1 FROM issue_tracker_dispatches
              WHERE session_id=?1 AND terminal_state IS NULL)"),
    // H18: the session leads a container (any kind, any status). Health rows read the
    // lead of every scoped Epic, archived or not.
    ("container_lead", "SELECT EXISTS(SELECT 1 FROM sessions c WHERE c.lead_session_id=?1)"),
    // H19: the session is the predecessor of an open rotation intent, or the successor its
    // `successor_reserved` event names (the static equivalent of `latest_open_rotation_intent`:
    // the newest rotation, entered, with none of the closing events).
    ("open_rotation_intent", "SELECT EXISTS(
                SELECT 1 FROM rotation_events latest
                WHERE latest.id=(SELECT MAX(id) FROM rotation_events WHERE session_id=?1)
                  AND NOT EXISTS (SELECT 1 FROM rotation_events c
                       WHERE c.session_id=?1 AND c.rotation_id=latest.rotation_id
                         AND (c.event_type IN ('completed','suppressed_final_handoff','aborted_empty_session')
                              OR c.event_type LIKE 'refused:%' OR c.phase='depth_limit_hit'))
                  AND EXISTS (SELECT 1 FROM rotation_events en
                       WHERE en.session_id=?1 AND en.rotation_id=latest.rotation_id
                         AND en.event_type='entered'
                         AND en.phase IN ('writing_handoff','pending_interrupt','completed_trigger')))
              OR EXISTS(
                SELECT 1 FROM sessions s
                JOIN rotation_events reserved ON reserved.session_id=s.continued_from
                WHERE s.id=?1 AND reserved.event_type='successor_reserved'
                  AND json_valid(reserved.metadata)
                  AND json_extract(reserved.metadata,'$.successor_id')=?1
                  AND reserved.rotation_id=(SELECT rotation_id FROM rotation_events
                       WHERE id=(SELECT MAX(id) FROM rotation_events WHERE session_id=reserved.session_id))
                  AND NOT EXISTS (SELECT 1 FROM rotation_events c
                       WHERE c.session_id=reserved.session_id AND c.rotation_id=reserved.rotation_id
                         AND (c.event_type IN ('completed','suppressed_final_handoff','aborted_empty_session')
                              OR c.event_type LIKE 'refused:%' OR c.phase='depth_limit_hit'))
                  AND EXISTS (SELECT 1 FROM rotation_events en
                       WHERE en.session_id=reserved.session_id AND en.rotation_id=reserved.rotation_id
                         AND en.event_type='entered'
                         AND en.phase IN ('writing_handoff','pending_interrupt','completed_trigger')))"),
];

/// The pending-question hold (H16): the session still owes the operator an
/// answer.
pub const PENDING_QUESTION_SQL: &str =
    "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1 AND pending_question_json IS NOT NULL)";
/// H20: the candidate predicate of `codex_usage_limit_hold`
/// (`store/provider_exhaustion.rs`) for this session. `?2` is the usage-limit
/// `stop_reason`, `?3` the day prefix of `now - HOLD_LOOKBACK`.
pub const USAGE_LIMIT_WINDOW_SQL: &str = "SELECT EXISTS(SELECT 1 FROM sessions
              WHERE id=?1 AND stop_reason=?2 AND provider IN ('Codex','CodexAppServer')
                AND updated_at>=?3)";
/// Reason code of H16.
pub const PENDING_QUESTION_CODE: &str = "pending_question";
/// Reason code of H17.
pub const RECENTLY_RESTORED_CODE: &str = "recently_restored";
/// Reason code of H20.
pub const USAGE_LIMIT_WINDOW_CODE: &str = "usage_limit_window";
/// Skip reason (not a hold) of a session whose age the age proof could not prove.
pub const AGE_UNPROVEN_CODE: &str = "age_unproven";

/// Every hold reason code: the [`HOLDS`] in order, then H16, H20 and H17.
pub const HOLD_CODES: [&str; 21] = [
    "wake",
    "wake_lineage",
    "agent_job",
    "queue_entry",
    "topology",
    "manager_seat",
    "succession",
    "review",
    "watch",
    "manager_operation",
    "agent_mail",
    "spawn_restart_watch",
    "custody_authority",
    "settlement",
    "lineage",
    "tracker_dispatch",
    "container_lead",
    "open_rotation_intent",
    PENDING_QUESTION_CODE,
    USAGE_LIMIT_WINDOW_CODE,
    RECENTLY_RESTORED_CODE,
];

/// What a hold evaluation needs besides the session: the clock.
#[derive(Debug, Clone, Copy)]
pub struct HoldContext {
    pub now: DateTime<Utc>,
}

/// The settled states of `hold`, as the JSON arrays its SQL binds, in `?2...`
/// order.
fn settled_params(hold: &str) -> Vec<String> {
    let mut classes: Vec<&StateClass> = HOLD_STATE_CLASSES
        .iter()
        .filter(|class| class.hold == hold)
        .collect();
    classes.sort_by_key(|class| class.param);
    classes
        .iter()
        .map(|class| serde_json::to_string(class.settled).unwrap_or_else(|_| "[]".to_string()))
        .collect()
}

/// The first hold that keeps `session_id` live, if any.
pub fn session_hold(
    conn: &Connection,
    session_id: &str,
    context: HoldContext,
) -> ColdResult<Option<&'static str>> {
    for (code, sql) in HOLDS {
        let mut params = vec![session_id.to_string()];
        params.extend(settled_params(code));
        let mut stmt = conn.prepare_cached(sql)?;
        let held: bool = stmt.query_row(rusqlite::params_from_iter(params), |row| row.get(0))?;
        if held {
            return Ok(Some(code));
        }
    }
    let mut stmt = conn.prepare_cached(PENDING_QUESTION_SQL)?;
    if stmt.query_row([session_id], |row| row.get::<_, bool>(0))? {
        return Ok(Some(PENDING_QUESTION_CODE));
    }
    usage_limit_window_hold(conn, session_id, context.now)
        .map(|held| held.then_some(USAGE_LIMIT_WINDOW_CODE))
}

/// H20: the session is inside the Codex usage-limit hold's look-back window.
pub fn usage_limit_window_hold(
    conn: &Connection,
    session_id: &str,
    now: DateTime<Utc>,
) -> ColdResult<bool> {
    let since = crate::store::provider_exhaustion::day_prefix(
        now - crate::store::provider_exhaustion::HOLD_LOOKBACK,
    );
    let mut stmt = conn.prepare_cached(USAGE_LIMIT_WINDOW_SQL)?;
    Ok(stmt.query_row(
        rusqlite::params![
            session_id,
            crate::store_support::provider_defaults::CODEX_USAGE_LIMIT_STOP_REASON,
            since
        ],
        |row| row.get(0),
    )?)
}

/// H17: the operator restored this session after `cutoff` (restore is a signal
/// that someone is using it). An unreadable timestamp holds the session.
pub fn recently_restored(
    conn: &Connection,
    session_id: &str,
    cutoff: DateTime<Utc>,
) -> ColdResult<bool> {
    let restored_at: Option<Option<String>> = conn
        .query_row(
            "SELECT restored_at FROM cold_archive_sessions
             WHERE session_id = ?1 AND state = 'restored'",
            [session_id],
            |row| row.get(0),
        )
        .optional()?;
    match restored_at.flatten() {
        None => Ok(false),
        Some(raw) => Ok(parse_timestamp(&raw).map_or(true, |restored| restored > cutoff)),
    }
}

/// The age proof (plan §2.2): every `created_at` of the session is `TEXT`, parses
/// as strict RFC 3339 (the rule `delegated_last_activity` uses), and the newest
/// instant is at or before `cutoff`. `false` means the session is skipped as
/// [`AGE_UNPROVEN_CODE`].
pub fn prove_age(conn: &Connection, session_id: &str, cutoff: DateTime<Utc>) -> ColdResult<bool> {
    let mut stmt = conn.prepare_cached(SESSION_CREATED_AT_SQL)?;
    let mut rows = stmt.query([session_id])?;
    let mut newest: Option<DateTime<Utc>> = None;
    while let Some(row) = rows.next()? {
        let ValueRef::Text(raw) = row.get_ref(0)? else {
            return Ok(false);
        };
        let Some(at) = std::str::from_utf8(raw)
            .ok()
            .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
        else {
            return Ok(false);
        };
        let at = at.with_timezone(&Utc);
        newest = Some(newest.map_or(at, |current| current.max(at)));
    }
    Ok(newest.is_some_and(|newest| newest <= cutoff))
}

/// The `SessionStatus` names this build knows. `SessionStatus` is
/// `#[non_exhaustive]` (another crate cannot match it exhaustively), so a variant
/// added later is not listed here; [`check_hold_classification`] therefore also
/// reads the statuses actually stored in `sessions`, and the settled-state
/// exclusion holds any status it does not list.
#[must_use]
pub fn session_status_names() -> Vec<&'static str> {
    const ALL: [SessionStatus; 8] = [
        SessionStatus::Starting,
        SessionStatus::Running,
        SessionStatus::WaitingApproval,
        SessionStatus::Completed,
        SessionStatus::Failed,
        SessionStatus::Interrupted,
        SessionStatus::Archived,
        SessionStatus::Deleted,
    ];
    ALL.iter()
        .map(|status| match status {
            SessionStatus::Starting => "Starting",
            SessionStatus::Running => "Running",
            SessionStatus::WaitingApproval => "WaitingApproval",
            SessionStatus::Completed => "Completed",
            SessionStatus::Failed => "Failed",
            SessionStatus::Interrupted => "Interrupted",
            SessionStatus::Archived => "Archived",
            SessionStatus::Deleted => "Deleted",
            _ => "unlisted",
        })
        .collect()
}

/// The quoted values of the first `column IN ('a','b',...)` in `table_sql`.
fn first_in_list(table_sql: &str, column: &str) -> Option<Vec<String>> {
    for (start, _) in table_sql.match_indices(column) {
        let before_ok = table_sql[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
        let rest = table_sql[start + column.len()..].trim_start();
        let Some(rest) = rest
            .get(..2)
            .filter(|word| word.eq_ignore_ascii_case("in"))
            .and_then(|_| rest.get(2..))
        else {
            continue;
        };
        let rest = rest.trim_start();
        if !before_ok || !rest.starts_with('(') {
            continue;
        }
        let body = &rest[1..];
        let end = body.find(')')?;
        let values: Vec<String> = body[..end]
            .split(',')
            .map(|item| item.trim().trim_matches('\'').to_string())
            .collect();
        if values.iter().all(|value| !value.is_empty()) {
            return Some(values);
        }
    }
    None
}

/// Proves the hold classification against the schema of `conn`: for every
/// [`HOLD_STATE_CLASSES`] entry the column is `NOT NULL`, live and settled are
/// disjoint, and live plus settled is exactly the admitted set (the first
/// `column IN (...)` of the table's `CHECK`, or the `SessionStatus` names). The
/// pass runs it before it archives anything: a migration that adds a state must
/// classify it first, and an unclassified schema refuses the pass with
/// `cold_archive_hold_schema_unclassified`.
pub fn check_hold_classification(conn: &Connection) -> ColdResult<()> {
    let unclassified =
        |detail: String| ColdError::new("cold_archive_hold_schema_unclassified", detail);
    for class in &HOLD_STATE_CLASSES {
        let StateClass {
            hold,
            table,
            column,
            live,
            settled,
            admitted,
            ..
        } = *class;
        if let Some(shared) = live.iter().find(|state| settled.contains(state)) {
            return Err(unclassified(format!(
                "{hold}: {table}.{column} state {shared:?} is both live and settled"
            )));
        }
        let not_null: Option<bool> = conn
            .query_row(
                "SELECT \"notnull\" <> 0 FROM pragma_table_info(?1) WHERE name = ?2",
                [table, column],
                |row| row.get(0),
            )
            .optional()?;
        match not_null {
            Some(true) => {}
            Some(false) => {
                return Err(unclassified(format!(
                    "{hold}: {table}.{column} is nullable; classify its NULL and write it into the hold"
                )));
            }
            None => {
                return Err(unclassified(format!(
                    "{hold}: {table}.{column} does not exist"
                )));
            }
        }
        let mut expected: Vec<String> = live
            .iter()
            .chain(settled.iter())
            .map(|state| (*state).to_string())
            .collect();
        expected.sort();
        let mut actual: Vec<String> = match admitted {
            Admitted::SessionStatus => {
                let mut names: Vec<String> = session_status_names()
                    .into_iter()
                    .map(str::to_string)
                    .collect();
                let mut stmt = conn.prepare("SELECT DISTINCT status FROM sessions")?;
                let stored = stmt.query_map([], |row| row.get::<_, String>(0))?;
                for status in stored {
                    let status = status?;
                    if !names.contains(&status) {
                        names.push(status);
                    }
                }
                names
            }
            Admitted::CheckList => {
                let sql: Option<String> = conn
                    .query_row(
                        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
                        [table],
                        |row| row.get(0),
                    )
                    .optional()?;
                let Some(sql) = sql else {
                    return Err(unclassified(format!(
                        "{hold}: table {table} does not exist"
                    )));
                };
                first_in_list(&sql, column).ok_or_else(|| {
                    unclassified(format!(
                        "{hold}: {table}.{column} has no CHECK ... IN (...) list"
                    ))
                })?
            }
        };
        actual.sort();
        if actual != expected {
            return Err(unclassified(format!(
                "{hold}: {table}.{column} admits {actual:?} but the classification covers {expected:?}"
            )));
        }
    }
    for (code, sql) in HOLDS {
        let classes: Vec<&StateClass> = HOLD_STATE_CLASSES
            .iter()
            .filter(|class| class.hold == code)
            .collect();
        for class in &classes {
            if !sql.contains(&format!("json_each(?{})", class.param)) {
                return Err(unclassified(format!(
                    "{code}: the SQL does not bind the settled list of {}.{} as ?{}",
                    class.table, class.column, class.param
                )));
            }
        }
    }
    Ok(())
}

/// One session that passed the base predicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub session_id: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub last_event_at: DateTime<Utc>,
}

/// One page of candidates, walked by primary key.
#[derive(Debug, Default)]
pub struct CandidatePage {
    pub candidates: Vec<Candidate>,
    /// Pass this as `after` to read the next page; `None` when the walk is done.
    pub next_after: Option<String>,
    /// Sessions read from the table (before the age filter).
    pub examined: usize,
    /// Sessions skipped because a timestamp did not parse.
    pub unreadable_timestamp: usize,
}

/// Reads up to `limit` sessions after `after` (by id) and keeps those whose
/// newest event is at or before `cutoff`.
pub fn candidate_page(
    conn: &Connection,
    after: Option<&str>,
    limit: usize,
    cutoff: DateTime<Utc>,
) -> ColdResult<CandidatePage> {
    let limit_param = i64::try_from(limit).unwrap_or(i64::MAX);
    let mut stmt = conn.prepare_cached(
        "SELECT s.id, s.status, s.created_at,
                (SELECT e.created_at FROM conversation_events e
                  WHERE e.session_id = s.id
                  ORDER BY e.sequence DESC, e.id DESC LIMIT 1) AS last_event_at
         FROM sessions s
         WHERE s.status IN ('Archived','Deleted')
           AND (?1 IS NULL OR s.id > ?1)
           AND NOT EXISTS (SELECT 1 FROM cold_archive_sessions c
                            WHERE c.session_id = s.id
                              AND c.state IN ('cold','deleting','restoring'))
         ORDER BY s.id LIMIT ?2",
    )?;
    let rows = stmt.query_map(rusqlite::params![after, limit_param], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
        ))
    })?;
    let mut page = CandidatePage::default();
    let mut last_id = None;
    for row in rows {
        let (session_id, status, created_at, last_event_at) = row?;
        page.examined += 1;
        last_id = Some(session_id.clone());
        let Some(last_event_at) = last_event_at else {
            // No events at all: nothing to archive.
            continue;
        };
        let (Ok(created_at), Ok(last_event_at)) = (
            parse_timestamp(&created_at),
            parse_timestamp(&last_event_at),
        ) else {
            page.unreadable_timestamp += 1;
            continue;
        };
        if last_event_at <= cutoff {
            page.candidates.push(Candidate {
                session_id,
                status,
                created_at,
                last_event_at,
            });
        }
    }
    if page.examined == limit {
        page.next_after = last_id;
    }
    Ok(page)
}

/// Refuses to archive when `conversation_events` or its provenance table no
/// longer has exactly the columns the codec was written for: a column the
/// archive does not carry would be dropped from the live table by the move.
pub fn ensure_expected_columns(conn: &Connection) -> ColdResult<()> {
    for (table, expected) in [
        (EVENTS_TABLE, &EVENT_COLUMNS[..]),
        (PROVENANCE_TABLE, &PROVENANCE_COLUMNS[..]),
    ] {
        let actual = table_columns(conn, table)?;
        let names: Vec<&str> = actual.iter().map(|(name, _)| name.as_str()).collect();
        if names != expected {
            return Err(ColdError::new(
                "cold_archive_schema_drift",
                format!("{table} has columns {names:?}; the archive codec expects {expected:?}"),
            ));
        }
    }
    Ok(())
}
