//! The classification of every production reader of `conversation_events`
//! (#2384, slice S2; plan §4.1) and the scanner that keeps it honest.
//!
//! **Invariant.** Moving a session to cold storage, reading it and restoring it
//! must not change any reader's or gate's result. A site is correct for one of
//! these reasons (its [`Class`]), and each class carries its own proof:
//!
//! * **merge**: reads the transcript, or a window of it, through the archive
//!   merge (`Store::merge_cold_events` and the functions built on it).
//! * **logical aggregate**: a count or length over all rows, read as live rows
//!   plus the rows archived for a cold session in the same statement.
//! * **tail exact**: observes only rows the live tail always keeps (plan §2.4:
//!   `Message`, `System`, `Thinking`, `Plan`, `Compressed` rows, the newest row,
//!   the newest provider row, the two `MAX(created_at)` witness rows and every row
//!   another table points at), so its result is unchanged by construction. Its
//!   behavioural probe runs before the move, after it and after the restore.
//! * **status disjoint**: its SQL or its Rust caller requires a status other than
//!   `Archived`/`Deleted`; the `sessions_cold_status_guard` trigger (V176) proves
//!   such a session is never cold.
//! * **hold disjoint**: its targets satisfy a plan §2.3 hold, so they are never
//!   cold.
//! * **writer**: the V176 insert guard refuses a write into a cold session; the
//!   writer's target is status or hold disjoint, or it restores first.
//! * **cross session** and **ddl** (schema text, not a reader).
//!
//! `cold_archive_reader_classification` walks the production code of every crate
//! (test modules excluded) for string literals naming `conversation_events` and
//! requires each `file::function` to appear below, with a witness: the SQL
//! fragment that realises its rule must still be in the function (so a changed
//! statement cannot silently go stale), or the call that makes it a merge.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Why a reader is correct for a cold session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Merge,
    LogicalAggregate,
    TailExact,
    StatusDisjoint,
    HoldDisjoint,
    Writer,
    CrossSession,
    Ddl,
}

/// One classified site.
#[derive(Debug, Clone, Copy)]
pub struct ReaderClass {
    /// `<crate>/src/<path>::<function>` (`const:NAME` for a module constant).
    pub site: &'static str,
    pub class: Class,
    /// The rule or hold that makes the site correct.
    pub proof: &'static str,
    /// A fragment that must appear (whitespace-normalised) in the string
    /// literals of the function. Empty when the proof is a call.
    pub sql: &'static str,
    /// A call that must appear in the file (merge sites). Empty otherwise.
    pub calls: &'static str,
    /// The behavioural probe (merge, logical aggregate, tail exact). Empty
    /// otherwise.
    pub probe: &'static str,
}

const fn site(
    site: &'static str,
    class: Class,
    proof: &'static str,
    sql: &'static str,
    calls: &'static str,
    probe: &'static str,
) -> ReaderClass {
    ReaderClass {
        site,
        class,
        proof,
        sql,
        calls,
        probe,
    }
}

/// The probes the invariance test runs (`cold_archive_reader_invariance`).
pub const PROBE_NAMES: [&str; 19] = [
    "load_events",
    "load_events_since",
    "read_session_events_page",
    "event_watermark",
    "last_assistant_message_event",
    "last_provider_output_at",
    "program_gate_window",
    "retirement_witness",
    "max_sequence",
    "any_event",
    "sequence_after",
    "provider_after",
    "max_created_at",
    "max_created_at_non_system",
    "message_facts",
    "user_role_facts",
    "system_facts",
    "fk_rows",
    "bundle_event_count",
];

use Class::{
    CrossSession, Ddl, HoldDisjoint, LogicalAggregate, Merge, StatusDisjoint, TailExact, Writer,
};

pub const COLD_READER_CLASSES: &[ReaderClass] = &[
    // --- merge ---------------------------------------------------------------
    site(
        "rsid-store/src/store/events.rs::load_events_since",
        Merge,
        "the one merge point of every transcript reader (GetConversation*, hydration, memory sync, dreamer, graph executions, launch)",
        "",
        "self.merge_cold_events(session_id, since_sequence, live)",
        "load_events_since",
    ),
    site(
        "rsid-store/src/store/events.rs::read_session_events_page",
        Merge,
        "AgentReadSessionEvents pages the merged transcript with the SQL's own rules",
        "",
        "self.cold_events_page(session_id, after_sequence, limit, event_types)",
        "read_session_events_page",
    ),
    site(
        "rsid-store/src/store_support/manager_gates.rs::check_manager_program_gate",
        Merge,
        "the assistant-role window after the last user message includes ToolUse rows; a cold target reads it through the merge. The last-user lookup is T1",
        "SELECT MAX(sequence) FROM conversation_events WHERE session_id=?1 AND role='User'",
        "store.cold_assistant_window(",
        "program_gate_window",
    ),
    // --- logical aggregate ---------------------------------------------------
    site(
        "rsid-store/src/store/events.rs::event_watermark",
        LogicalAggregate,
        "count and content length are live plus archived (LOGICAL_ARCHIVED_*_SQL); the maximum sequence is exact (T2)",
        "archived_event_count",
        "",
        "event_watermark",
    ),
    site(
        "rsid-store/src/store/manager_actions/recovery.rs::apply_manager_retire_continuations",
        LogicalAggregate,
        "the retirement witness records the logical count (LOGICAL_EVENT_COUNT_SQL); event_sequence is T2 and last_user_sequence is T1",
        "SELECT MAX(sequence) FROM conversation_events WHERE session_id=?1 AND role='User'",
        "",
        "retirement_witness",
    ),
    site(
        "rsid-store/src/store/manager_actions/recovery.rs::manager_lead_program_outcome_superseded",
        LogicalAggregate,
        "the predicate compares the witness with the same logical count (#2396)",
        "archived_event_count",
        "",
        "retirement_witness",
    ),
    // --- tail exact: T1 Message / role = 'User' ------------------------------
    site(
        "rsid-store/src/store/summaries.rs::count_assistant_messages",
        TailExact,
        "T1: every Message row stays live",
        "event_type = 'Message' AND role = 'Assistant'",
        "",
        "message_facts",
    ),
    site(
        "rsid-store/src/store/agent_jobs.rs::session_job_settled_since_last_turn",
        TailExact,
        "T1: every Message row stays live; MAX(created_at) of the owner's Assistant Messages is \
         the same before and after a move (#2476 parked-node settle check)",
        "e.event_type='Message' AND e.role='Assistant'",
        "",
        "last_assistant_message_event",
    ),
    site(
        "rsid-store/src/store/worker_no_result.rs::worker_no_result_nudged",
        TailExact,
        "T1: role = 'User' occurs only on Message rows",
        "role='User' AND instr(content, ?2)>0",
        "",
        "user_role_facts",
    ),
    site(
        "rsid-store/src/store/worker_no_result.rs::report_turn_contents",
        TailExact,
        "T1: Message rows",
        "event_type='Message' AND role IN ('User','Assistant')",
        "",
        "message_facts",
    ),
    site(
        "rsid-store/src/store/worker_no_result.rs::latest_turn_reported_in_declared_form",
        TailExact,
        "T1: role = 'User' occurs only on Message rows",
        "event_type='Message' AND role='User'",
        "",
        "message_facts",
    ),
    site(
        "rsid-store/src/store/worker_no_result.rs::worker_published_handoff_this_turn",
        TailExact,
        "T1: role = 'User' occurs only on Message rows",
        "event_type='Message' AND role='User'",
        "",
        "message_facts",
    ),
    site(
        "rsid-store/src/store/worker_no_result.rs::latest_handoff_after",
        TailExact,
        "T1: Message rows",
        "event_type='Message' AND role='Assistant'",
        "",
        "message_facts",
    ),
    site(
        "rsid-store/src/store/manager_question_stall.rs::manager_question_nudges",
        TailExact,
        "T1: role = 'User' occurs only on Message rows",
        "role='User' AND instr(content, ?2)>0",
        "",
        "user_role_facts",
    ),
    site(
        "rsid/src/topology/decision.rs::answer_turn_observed",
        TailExact,
        "T1: Message rows",
        "event_type='Message' AND role='User' AND instr(content,?2)>0",
        "",
        "user_role_facts",
    ),
    site(
        "rsid/src/topology/verify.rs::continuation_turn_observed",
        TailExact,
        "T1: role = 'User' occurs only on Message rows",
        "event_type='Message' AND role='User' AND instr(content,?2)>0",
        "",
        "user_role_facts",
    ),
    site(
        "rsid-store/src/store/harness_manager.rs::manager_progress_page",
        TailExact,
        "T1: Message rows",
        "role='Assistant' AND event_type='Message'",
        "",
        "message_facts",
    ),
    site(
        "rsid-store/src/store/manager_actions/issue_worker_evidence.rs::evidence_result_line",
        TailExact,
        "T1: Message rows",
        "event_type='Message' AND role='Assistant'",
        "",
        "message_facts",
    ),
    site(
        "rsid-store/src/store/efficiency_metrics.rs::load_user_messages",
        TailExact,
        "T1: role = 'User' occurs only on Message rows",
        "role = 'User' AND event_type = 'Message'",
        "",
        "user_role_facts",
    ),
    site(
        "rsid-store/src/store/pending_questions.rs::pending_question_target",
        TailExact,
        "T4: the published question's ToolUse row is referenced by pending_question_publications and stays; the later-user probe is T1",
        "e.event_type='ToolUse' AND e.tool_name='AskUserQuestion'",
        "",
        "fk_rows",
    ),
    site(
        "rsid-store/src/store/closure_kernel.rs::closure_terminal_capture_candidate",
        TailExact,
        "T1: Message rows with the provenance of kept rows",
        "e.event_type='Message' AND e.role='Assistant' AND trim(e.content)!=''",
        "",
        "message_facts",
    ),
    site(
        "rsid-store/src/store/closure_kernel.rs::closure_evidence_admission_context",
        TailExact,
        "T1: Message rows with the provenance of kept rows",
        "e.event_type='Message' AND e.role='Assistant'",
        "",
        "message_facts",
    ),
    site(
        "rsid-store/src/store/closure_kernel.rs::record_closure_evidence",
        TailExact,
        "T1: Message rows with the provenance of kept rows",
        "e.event_type='Message' AND e.role='Assistant'",
        "",
        "message_facts",
    ),
    site(
        "rsid-store/src/store/closure_kernel.rs::revalidate_capture_key",
        TailExact,
        "T1: a closure proposal names a handoff Message row",
        "WHERE e.id=?1",
        "",
        "message_facts",
    ),
    site(
        "rsid/src/session/manager_ledger.rs::observe_independent_evidence",
        TailExact,
        "T1: the reviewer's handoff is a Message row with provenance",
        "e.role='Assistant' AND e.event_type='Message'",
        "",
        "message_facts",
    ),
    site(
        "rsid-store/src/store/manager_ledger/mutations.rs::manager_v2_commit_update",
        StatusDisjoint,
        "the reviewer session must be Completed (never cold); the evidence row is a T1 Message",
        "s.status='Completed'",
        "",
        "",
    ),
    // --- tail exact: T1 System ----------------------------------------------
    site(
        "rsid-store/src/store/agent_coordination.rs::terminal_sandbox_worktree_dirty_on",
        TailExact,
        "T1: every System row stays",
        "e.event_type='System'",
        "",
        "system_facts",
    ),
    site(
        "rsid-store/src/store/transient_heal.rs::recent_error_texts",
        TailExact,
        "T1: every System row stays",
        "event_type='System'",
        "",
        "system_facts",
    ),
    // --- tail exact: T2 newest row / T3 newest provider row ------------------
    site(
        "rsid-store/src/store/agent_coordination.rs::continuation_cursor_tx",
        TailExact,
        "T2: the newest row (maximum sequence) stays",
        "SELECT COALESCE(MAX(sequence),0) FROM conversation_events WHERE session_id=?1",
        "",
        "max_sequence",
    ),
    site(
        "rsid-store/src/store/manager_actions.rs::manager_action_lead_fence",
        TailExact,
        "T2",
        "SELECT COALESCE(MAX(sequence),0) FROM conversation_events WHERE session_id=?1",
        "",
        "max_sequence",
    ),
    site(
        "rsid-store/src/store/manager_actions/fence.rs::conversation_max_sequence",
        TailExact,
        "T2",
        "SELECT COALESCE(MAX(sequence), -1) FROM conversation_events WHERE session_id=?1",
        "",
        "max_sequence",
    ),
    site(
        "rsid-store/src/store/manager_actions/fence.rs::restart_cut_fence",
        TailExact,
        "T2",
        "(SELECT COALESCE(MAX(sequence), -1) FROM conversation_events WHERE session_id=s.id)",
        "",
        "max_sequence",
    ),
    site(
        "rsid-store/src/store/manager_notices.rs::record_manager_session_notice",
        TailExact,
        "T2",
        "SELECT COALESCE(MAX(sequence),0) FROM conversation_events WHERE session_id=?1",
        "",
        "max_sequence",
    ),
    site(
        "rsid-store/src/store/manager_successions.rs::manager_succession_settlement_snapshot",
        TailExact,
        "T2",
        "SELECT COALESCE(MAX(sequence),0) FROM conversation_events WHERE session_id=?1",
        "",
        "max_sequence",
    ),
    site(
        "rsid-store/src/store/agent_child_relaunch_intents.rs::recover_child_relaunch_intent",
        TailExact,
        "T2: a row after the observed sequence implies the newest row is after it",
        "WHERE session_id=?1 AND sequence>?2",
        "",
        "sequence_after",
    ),
    site(
        "rsid-remote-read/src/selected_session.rs::selected_runtime_store_miss",
        TailExact,
        "T2: any-event EXISTS; the newest row stays",
        "SELECT 1 FROM conversation_events WHERE session_id=?1 LIMIT 1",
        "",
        "any_event",
    ),
    site(
        "rsid-store/src/store/events.rs::last_provider_output_at",
        TailExact,
        "T3: the newest provider row (Assistant role or Thinking) stays",
        "(role = 'Assistant' OR event_type = 'Thinking')",
        "",
        "last_provider_output_at",
    ),
    site(
        "rsid-store/src/store/events.rs::last_assistant_message_event",
        TailExact,
        "T1: Message rows",
        "event_type = 'Message' AND role = 'Assistant'",
        "",
        "last_assistant_message_event",
    ),
    // --- tail exact: T5 / T6 MAX(created_at) witnesses -----------------------
    site(
        "rsid-store/src/store/manager_intent.rs::manager_v2_lead_last_activity",
        TailExact,
        "T5: the row that attains the TEXT MAX(created_at) stays",
        "SELECT MAX(created_at) FROM conversation_events WHERE session_id=?1",
        "",
        "max_created_at",
    ),
    site(
        "rsid-store/src/store/manager_ledger/inspect.rs::manager_v2_worker",
        TailExact,
        "T2 and T5",
        "SELECT COALESCE(MAX(sequence),0),MAX(created_at) FROM conversation_events WHERE session_id=?1",
        "",
        "max_created_at",
    ),
    site(
        "rsid-store/src/store/agent_coordination.rs::progress_row",
        TailExact,
        "T2 and T5 (AgentGetProgress activity and cursor snapshot)",
        "SELECT COALESCE(MAX(sequence),0),MAX(created_at) FROM conversation_events WHERE session_id=?1",
        "",
        "max_created_at",
    ),
    site(
        "rsid-store/src/store/transient_heal.rs::transient_heal_epoch_over",
        TailExact,
        "T6: the row that attains the TEXT MAX(created_at) over non-System rows stays",
        "SELECT MAX(created_at) FROM conversation_events WHERE session_id=?1 AND event_type<>'System'",
        "",
        "max_created_at_non_system",
    ),
    // --- status disjoint (the V176 status guard) -----------------------------
    site(
        "rsid-store/src/store/remote_manager.rs::remote_manager_events_before",
        StatusDisjoint,
        "the SQL joins sessions and excludes Archived and Deleted ones (#2962)",
        "s.status NOT IN ('Archived','Deleted')",
        "",
        "",
    ),
    site(
        "rsid-store/src/store/manager_actions/operator_delegation.rs::delegated_last_activity",
        StatusDisjoint,
        "callers require Completed, or are retention candidates in Completed/Failed/Interrupted",
        "SELECT created_at FROM conversation_events WHERE session_id=?1",
        "",
        "",
    ),
    site(
        "rsid-store/src/store/pending_approvals.rs::appserver_approval_binding_valid",
        StatusDisjoint,
        "the SQL excludes Archived and Deleted sessions",
        "s.status NOT IN ('Archived','Deleted')",
        "",
        "",
    ),
    site(
        "rsid-store/src/store/manager_actions/issue_worker_capacity.rs::issue_worker_capacity_retry_state",
        StatusDisjoint,
        "reads a Failed worker",
        "event_type='ToolUse'",
        "",
        "",
    ),
    site(
        "rsid-store/src/store/manager_resources.rs::manager_v2_legacy_spend",
        StatusDisjoint,
        "reads a Starting session (an any-event probe, T2)",
        "SELECT EXISTS(SELECT 1 FROM conversation_events WHERE session_id=?1)",
        "",
        "",
    ),
    site(
        "rsid-store/src/store/manager_resources.rs::manager_v2_scoped_spend",
        StatusDisjoint,
        "the SQL requires status Starting",
        "s.status='Starting'",
        "",
        "",
    ),
    site(
        "rsid-store/src/store/rotation_abandon.rs::rotation_holder_never_started_on",
        StatusDisjoint,
        "a Failed holder; the other callers act inside an open rotation intent (hold H19)",
        "e.event_type='Message' AND e.role='User'",
        "",
        "",
    ),
    // --- hold disjoint (plan §2.3) -------------------------------------------
    site(
        "rsid-store/src/store/manager_ledger/health.rs::health_delivery_abandoned",
        HoldDisjoint,
        "H18: the session is the lead of a scoped Epic",
        "WHERE session_id=?1 AND sequence>?2",
        "",
        "",
    ),
    site(
        "rsid-store/src/store/manager_ledger/health.rs::poll_turn_is_effect_free",
        HoldDisjoint,
        "H18: the session is the lead of a scoped Epic",
        "event_type='ToolUse'",
        "",
        "",
    ),
    site(
        "rsid/src/session/rotation/recovery.rs::bind_recovered_own_handoff",
        HoldDisjoint,
        "H19: the predecessor of an open rotation intent",
        "instr(COALESCE(tool_input,''),?2)>0",
        "",
        "",
    ),
    site(
        "rsid-store/src/store/provider_exhaustion.rs::latest_usage_limit_failure",
        HoldDisjoint,
        "H20: the usage-limit window",
        "content LIKE '**Process Error%'",
        "",
        "",
    ),
    site(
        "rsid-store/src/store/provider_exhaustion.rs::recent_usage_limit_failures",
        HoldDisjoint,
        "H20: the usage-limit window",
        "content LIKE '**Process Error%'",
        "",
        "",
    ),
    // --- writers (Guard 1 refuses a write into a cold session) ---------------
    site(
        "rsid-store/src/store/events.rs::insert_event_in_transaction",
        Writer,
        "the one writer of conversation_events; Guard 1 turns a write into a cold session into cold_session_restore_required",
        "INSERT INTO conversation_events",
        "",
        "",
    ),
    site(
        "rsid-store/src/store/events.rs::update_event_content",
        Writer,
        "requires exactly one live row: an archived id is an error, not a silent no-op",
        "UPDATE conversation_events SET content = ?1 WHERE id = ?2",
        "",
        "",
    ),
    site(
        "rsid-store/src/store/agent_coordination.rs::acknowledge_agent_message_with_evidence",
        Writer,
        "the ack event targets a live mail target (hold H10)",
        "INSERT INTO conversation_events",
        "",
        "",
    ),
    site(
        "rsid-store/src/store/agent_coordination.rs::record_failure_cause_in_tx",
        Writer,
        "its UPDATE requires status Failed/Interrupted (never Archived/Deleted, so never cold); the next sequence is MAX(sequence) (T2)",
        "SELECT COALESCE(MAX(sequence), 0) + 1 FROM conversation_events",
        "",
        "",
    ),
    site(
        "rsid-store/src/store/agent_coordination.rs::record_terminal_sandbox_worktree",
        Writer,
        "targets a session that just finished or a startup orphan, never Archived/Deleted",
        "SELECT COALESCE(MAX(sequence), 0) + 1 FROM conversation_events",
        "",
        "",
    ),
    site(
        "rsid-store/src/store/manager_notices.rs::abandon_unconsumed_watch",
        Writer,
        "appends a System fact to the lineage tip of an enabled wake (hold H1b); its idempotency read is a T1 System row",
        "json_extract(metadata,'$.health_fact')='delivery_abandoned'",
        "",
        "",
    ),
    site(
        "rsid-store/src/store/operator_messages.rs::deliver_operator_message_with_transcript",
        Writer,
        "delivers at a tool boundary to a Running session",
        "SELECT COALESCE(MAX(sequence),0)+1 FROM conversation_events",
        "",
        "",
    ),
    site(
        "rsid-store/src/store/friction.rs::note_repeated_tool_error",
        Writer,
        "runs on the insert path of a hot session; it reads the ToolUse row of a just-inserted ToolResult",
        "event_type='ToolUse' AND tool_use_id=?2 AND id<?3",
        "",
        "",
    ),
    site(
        "rsid-store/src/store/sessions.rs::purge_session",
        Writer,
        "refuses a cold session with session_cold_archived",
        "DELETE FROM conversation_events WHERE session_id = ?1",
        "",
        "",
    ),
    // --- cross session -------------------------------------------------------
    site(
        "rsid-remote-read/src/history.rs::tool_pair_ambiguous",
        CrossSession,
        "only same-session rows decide ambiguity (the session is not Archived/Deleted, so none of its rows is cold); other sessions' rows only fill the 65-row cap, so a move can only turn SourceUnavailable into the exact answer (pinned by rsid remote_read::cold_tests)",
        "INDEXED BY idx_events_tool_use_id",
        "",
        "",
    ),
    site(
        "rsid-store/src/store/manager_intent/idle.rs::manager_intent_input_pin",
        CrossSession,
        "#2939: MAX(id) of the whole table is the newest row of its session, which the live tail always keeps (plan §2.4); a move commits on another connection, so it also changes data_version and invalidates every pin",
        "SELECT COALESCE(MAX(id),0) FROM conversation_events",
        "",
        "",
    ),
    site(
        "rsid-store/src/store/manager_intent/idle.rs::manager_intent_events_matter",
        CrossSession,
        "#2939: reads only rows inserted after a pin taken on this daemon; the V176 insert guard refuses an insert into a cold session, so each such row is live, and a move changes data_version, which invalidates the pin before this runs",
        "WHERE e.id>?1 AND (s.status='Starting'",
        "",
        "",
    ),
    // --- Remote V1 history and detail (status disjoint) ----------------------
    //
    // Remote V1 serves only sessions that are not Archived or Deleted
    // (`require_session_project`, `status NOT IN ('Archived','Deleted')`: the
    // phone never lists or opens an archived session). Only Archived/Deleted
    // sessions are ever cold (the candidate query and the V176 status guard), so
    // each of these is refused `NotFound` before it reads an event, for a cold
    // session exactly as for the same session before it moved. The
    // `remote_read::cold_tests` of the daemon pin both halves with a really moved
    // session.
    site(
        "rsid-remote-read/src/history.rs::history_page",
        StatusDisjoint,
        "require_session_project refuses Archived/Deleted sessions, the only ones that can be cold (slice S3)",
        "",
        "require_session_project(conn, project, session)",
        "",
    ),
    site(
        "rsid-remote-read/src/history.rs::observed_history_page",
        StatusDisjoint,
        "require_session_project refuses Archived/Deleted sessions before the head or the interval is read",
        "",
        "require_session_project(conn, project, session)",
        "",
    ),
    site(
        "rsid-remote-read/src/detail.rs::session_detail_source",
        StatusDisjoint,
        "require_session_project refuses Archived/Deleted sessions before the saved head is read",
        "",
        "require_session_project(conn, project, session)",
        "",
    ),
    // --- portable bundle (logical aggregate) ---------------------------------
    site(
        "rsid-store/src/store/portable_bundle.rs::const:LEFT_BEHIND_TABLES",
        LogicalAggregate,
        "the manifest records the table's row count as live plus archived (LOGICAL_ARCHIVED_TOTAL_COUNT_SQL)",
        "",
        "LOGICAL_ARCHIVED_TOTAL_COUNT_SQL",
        "bundle_event_count",
    ),
    // --- ddl (schema text, not a reader) -------------------------------------
    site(
        "rsid-store/src/store/manager_intent/idle.rs::const:EVENTS_TABLE",
        Ddl,
        "#2939: the table name the update hook compares; it reads nothing",
        "conversation_events",
        "",
        "",
    ),
    site(
        "rsid-store/src/store/agent_coordination.rs::agent_message_delivery_attempts_ddl",
        Ddl,
        "schema text",
        "",
        "",
        "",
    ),
    site(
        "rsid-store/src/store/agent_coordination.rs::install_v81_effect_tables",
        Ddl,
        "schema text",
        "",
        "",
        "",
    ),
    site(
        "rsid-store/src/store/agent_coordination.rs::install_v81_effect_triggers",
        Ddl,
        "schema text; the trigger checks a row that is an FK target and so stays live (T4)",
        "",
        "",
        "",
    ),
    site(
        "rsid-store/src/store/agent_coordination.rs::install_v81_triggers",
        Ddl,
        "schema text; the trigger checks a row that is an FK target and so stays live (T4)",
        "",
        "",
        "",
    ),
];

// ---------------------------------------------------------------------------
// The scanner
// ---------------------------------------------------------------------------

/// True when an attribute list gates an item to tests (`#[test]`,
/// `#[tokio::test]`, `#[cfg(test)]`, `#[cfg(any(test, ...))]`, but not
/// `#[cfg(not(test))]`).
fn test_gated(attrs: &[syn::Attribute]) -> bool {
    fn mentions_test(stream: &proc_macro2::TokenStream) -> bool {
        stream.clone().into_iter().any(|tree| match tree {
            proc_macro2::TokenTree::Ident(ident) => ident == "test",
            proc_macro2::TokenTree::Group(group) => mentions_test(&group.stream()),
            _ => false,
        })
    }
    attrs.iter().any(|attr| {
        let path = attr.path();
        if path
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "test")
        {
            return true;
        }
        if path.is_ident("cfg")
            && let syn::Meta::List(list) = &attr.meta
        {
            let text = list.tokens.to_string().replace(' ', "");
            return mentions_test(&list.tokens) && !text.contains("not(test)");
        }
        false
    })
}

/// Collects, per named function (or module constant), the text of every string
/// literal inside it.
#[derive(Default)]
struct Literals {
    stack: Vec<String>,
    by_site: BTreeMap<String, String>,
}

impl Literals {
    fn note(&mut self, text: &str) {
        if let Some(site) = self.stack.last() {
            let entry = self.by_site.entry(site.clone()).or_default();
            entry.push_str(text);
            entry.push('\n');
        }
    }

    fn note_tokens(&mut self, stream: &proc_macro2::TokenStream) {
        for tree in stream.clone() {
            match tree {
                proc_macro2::TokenTree::Literal(literal) => {
                    let text = literal.to_string();
                    if text.starts_with('"') || text.starts_with('r') {
                        match syn::parse_str::<syn::LitStr>(&text) {
                            Ok(parsed) => self.note(&parsed.value()),
                            Err(_) => self.note(&text),
                        }
                    }
                }
                proc_macro2::TokenTree::Group(group) => self.note_tokens(&group.stream()),
                _ => {}
            }
        }
    }
}

impl<'ast> syn::visit::Visit<'ast> for Literals {
    fn visit_attribute(&mut self, _: &'ast syn::Attribute) {}

    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        if !test_gated(&node.attrs) {
            syn::visit::visit_item_mod(self, node);
        }
    }

    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        if !test_gated(&node.attrs) {
            syn::visit::visit_item_impl(self, node);
        }
    }

    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        if test_gated(&node.attrs) {
            return;
        }
        self.stack.push(node.sig.ident.to_string());
        syn::visit::visit_item_fn(self, node);
        self.stack.pop();
    }

    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        if test_gated(&node.attrs) {
            return;
        }
        self.stack.push(node.sig.ident.to_string());
        syn::visit::visit_impl_item_fn(self, node);
        self.stack.pop();
    }

    fn visit_item_const(&mut self, node: &'ast syn::ItemConst) {
        if test_gated(&node.attrs) {
            return;
        }
        self.stack.push(format!("const:{}", node.ident));
        syn::visit::visit_item_const(self, node);
        self.stack.pop();
    }

    fn visit_item_static(&mut self, node: &'ast syn::ItemStatic) {
        if test_gated(&node.attrs) {
            return;
        }
        self.stack.push(format!("const:{}", node.ident));
        syn::visit::visit_item_static(self, node);
        self.stack.pop();
    }

    fn visit_lit_str(&mut self, node: &'ast syn::LitStr) {
        self.note(&node.value());
    }

    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        self.note_tokens(&node.tokens);
        syn::visit::visit_macro(self, node);
    }
}

/// A test file or a test-only directory, judged on the path *under the crates
/// directory* with a leading `/` (see [`is_scanned_source`]).
fn is_test_file(anchored: &str) -> bool {
    let name = anchored.rsplit('/').next().unwrap_or_default();
    name == "tests.rs"
        || name.ends_with("_tests.rs")
        || name.starts_with("tests_")
        || name == "test_support.rs"
        || anchored.contains("/tests/")
        || anchored.contains("/e2e_tests/")
        || anchored.contains("/store_split_tests/")
        || anchored.contains("/migrations/")
        || anchored.contains("/store/cold_archive/")
}

/// Production source the scanner reads: a `.rs` file under some crate's `src/`
/// that is not a test file. `relative` is the path under the crates directory
/// with `/` separators. **Only that path decides**: where the checkout lives
/// never does. The merge queue gates a candidate in a private repository under
/// `~/.rsi/queue/target/rsi-rolling-land-*/repo`, so an absolute path holds a
/// `target` directory, and a check on the absolute path skipped every file there
/// (the first gate of slice S2 failed with every classified site "stale").
fn is_scanned_source(relative: &str) -> bool {
    let anchored = format!("/{relative}");
    anchored.ends_with(".rs")
        && anchored.contains("/src/")
        && !anchored.contains("/target/")
        && !is_test_file(&anchored)
}

fn normalized(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The crates the classification names (`rsid-store`, `rsid`, `rsid-remote-read`).
fn classified_crates() -> BTreeSet<&'static str> {
    COLD_READER_CLASSES
        .iter()
        .filter_map(|entry| entry.site.split('/').next())
        .collect()
}

/// `Err` naming what was not found when the scan did not read a single source
/// file of a crate the classification names: a wrong crates directory or a
/// filter that skips a whole crate must fail loudly here, not surface as a
/// stale classification.
fn check_every_classified_crate_was_scanned(
    crates: &Path,
    scanned: &BTreeMap<String, usize>,
) -> Result<(), String> {
    let missing: Vec<&str> = classified_crates()
        .into_iter()
        .filter(|name| scanned.get(*name).copied().unwrap_or(0) == 0)
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(format!(
        "the reader scan read no production source file of {missing:?} under {} \
         (source files read per crate: {scanned:?}); the scan must find the whole workspace",
        crates.display()
    ))
}

/// Every `site -> (literal text, file text)` naming `conversation_events` under
/// `crates`, with the number of source files read per crate.
fn scan_sources_under(
    crates: &Path,
) -> (BTreeMap<String, (String, String)>, BTreeMap<String, usize>) {
    let mut found = BTreeMap::new();
    let mut scanned: BTreeMap<String, usize> = BTreeMap::new();
    for entry in walkdir::WalkDir::new(crates)
        .into_iter()
        .filter_entry(|entry| entry.file_name() != "target" && entry.file_name() != ".git")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
    {
        let path = entry.path();
        let Ok(relative) = path.strip_prefix(crates) else {
            continue;
        };
        let relative = relative.to_string_lossy().replace('\\', "/");
        if !is_scanned_source(&relative) {
            continue;
        }
        let crate_name = relative.split('/').next().unwrap_or_default().to_string();
        *scanned.entry(crate_name).or_default() += 1;
        let source = std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("{} must be readable: {error}", path.display()));
        if !source.contains("conversation_events") {
            continue;
        }
        let parsed = syn::parse_file(&source)
            .unwrap_or_else(|error| panic!("{} must parse: {error}", path.display()));
        let mut literals = Literals::default();
        syn::visit::Visit::visit_file(&mut literals, &parsed);
        for (function, text) in literals.by_site {
            if text.contains("conversation_events") {
                found.insert(format!("{relative}::{function}"), (text, source.clone()));
            }
        }
    }
    (found, scanned)
}

/// Every `site -> (literal text, file text)` naming `conversation_events` in the
/// workspace this test runs in.
fn scan_production_sites() -> BTreeMap<String, (String, String)> {
    let manifest = PathBuf::from(rsi_common::manifest_dir!());
    let crates = manifest.join("..").canonicalize().unwrap_or_else(|error| {
        panic!(
            "cannot resolve the crates directory next to {}: {error}",
            manifest.display()
        )
    });
    let (found, scanned) = scan_sources_under(&crates);
    if let Err(message) = check_every_classified_crate_was_scanned(&crates, &scanned) {
        panic!("{message}");
    }
    found
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn the_scanner_reads_a_checkout_whatever_its_absolute_path_holds() {
    // Production source is judged by its path under the crates directory.
    assert!(is_scanned_source("rsid-remote-read/src/history.rs"));
    assert!(is_scanned_source("rsid-store/src/store/events.rs"));
    assert!(!is_scanned_source("rsid-store/src/store/tests.rs"));
    assert!(!is_scanned_source("rsid/tests/store_crate_seal.rs"));
    assert!(!is_scanned_source(
        "rsid-store/src/store/cold_archive/reader.rs"
    ));

    // A real scan of a checkout whose absolute path holds `target` and `tests`
    // directories (the merge queue's private repository layout) finds the same
    // sites as a plain one.
    let root = tempfile::tempdir().unwrap();
    let crates = root
        .path()
        .join("queue/target/rsi-rolling-land-X1/repo/tests/crates");
    let src = crates.join("rsid-remote-read/src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("history.rs"),
        "fn history_page() { let _ = \"SELECT id FROM conversation_events\"; }\n",
    )
    .unwrap();
    std::fs::create_dir_all(crates.join("rsid-remote-read/src/target")).unwrap();
    std::fs::write(
        crates.join("rsid-remote-read/src/target/skipped.rs"),
        "fn skipped() { let _ = \"conversation_events\"; }\n",
    )
    .unwrap();
    let (found, scanned) = scan_sources_under(&crates);
    assert_eq!(
        found.keys().cloned().collect::<Vec<_>>(),
        vec!["rsid-remote-read/src/history.rs::history_page".to_string()]
    );
    assert_eq!(scanned.get("rsid-remote-read"), Some(&1));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_scan_that_read_no_file_of_a_classified_crate_fails_loudly() {
    let crates = Path::new("/somewhere/crates");
    let mut scanned: BTreeMap<String, usize> = classified_crates()
        .into_iter()
        .map(|name| (name.to_string(), 3))
        .collect();
    assert_eq!(
        check_every_classified_crate_was_scanned(crates, &scanned),
        Ok(())
    );
    scanned.insert("rsid-remote-read".to_string(), 0);
    let message = check_every_classified_crate_was_scanned(crates, &scanned).unwrap_err();
    assert!(
        message.contains("rsid-remote-read") && message.contains("/somewhere/crates"),
        "{message}"
    );
    assert!(
        check_every_classified_crate_was_scanned(crates, &BTreeMap::new())
            .unwrap_err()
            .contains("rsid-store"),
        "an empty scan names every crate"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn cold_archive_reader_classification() {
    let found = scan_production_sites();
    let classified: BTreeSet<&str> = COLD_READER_CLASSES.iter().map(|entry| entry.site).collect();
    assert_eq!(
        classified.len(),
        COLD_READER_CLASSES.len(),
        "a site is classified once"
    );

    let unclassified: Vec<&String> = found
        .keys()
        .filter(|site| !classified.contains(site.as_str()))
        .collect();
    assert!(
        unclassified.is_empty(),
        "production code reads conversation_events at sites with no cold-archive class; \
         classify each in COLD_READER_CLASSES (plan §4.1): {unclassified:#?}"
    );
    let stale: Vec<&&str> = classified
        .iter()
        .filter(|site| !found.contains_key(**site))
        .collect();
    assert!(
        stale.is_empty(),
        "classified sites that no longer name conversation_events: {stale:#?}"
    );

    for entry in COLD_READER_CLASSES {
        let (literals, source) = &found[entry.site];
        if !entry.sql.is_empty() {
            assert!(
                normalized(literals).contains(&normalized(entry.sql)),
                "{}: the recorded {:?} rule no longer appears in the function's SQL: {:?}",
                entry.site,
                entry.class,
                entry.sql
            );
        }
        if !entry.calls.is_empty() {
            assert!(
                normalized(source).contains(&normalized(entry.calls)),
                "{}: the merge call is gone: {:?}",
                entry.site,
                entry.calls
            );
        }
        match entry.class {
            Class::Merge | Class::LogicalAggregate | Class::TailExact => assert!(
                PROBE_NAMES.contains(&entry.probe),
                "{}: a {:?} reader must name a behavioural probe (got {:?})",
                entry.site,
                entry.class,
                entry.probe
            ),
            _ => assert!(
                entry.probe.is_empty(),
                "{}: only merge, logical-aggregate and tail-exact readers name a probe",
                entry.site
            ),
        }
        assert!(
            !entry.proof.is_empty(),
            "{}: a class needs its proof",
            entry.site
        );
    }
}
