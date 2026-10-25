//! `AgentReadSessionEvents` (#1041): a typed, scoped, byte-bounded read of a
//! session's conversation events.
//!
//! Scope is exactly `AgentGetStatus`' (`authorize_agent_target`): the caller's
//! own session, a direct child, a child of an Epic it leads, or a session in
//! the current manager's live scope, or (#2531) an earlier seat of the
//! caller's own project manager slot (its own succession lineage, or #118 a
//! displaced earlier appointment's), or (#173) for a live Issue-bound worker
//! the same-project session that created its bound Issue (excluding manager
//! seats and their continuations, #471), or (#466) a
//! same-project non-manager session cited by full lowercase UUID in that Issue's
//! body, launch brief or handoffs authored outside the reader's continuation
//! lineage. An unknown target and an out-of-scope target produce the identical refusal, so a refusal never reveals whether a
//! session exists. Read-only: no writes, no journal rows.

use super::agent_verbs::AgentControlHandle;
use crate::error::{DaemonError, Result};
use rsi_common::agent_session_events::{
    AGENT_READ_EVENTS_SCOPE_DENIED, AgentEventsStorageV1, AgentFinalMessageV1,
    AgentReadSessionEventsRequestV1, AgentReadSessionEventsResultV1, AgentSessionEventV1,
    FINAL_MESSAGE_FULL_MAX_CHARS, FINAL_MESSAGE_MAX_CHARS, clip_chars,
    is_provider_diagnostic_metadata,
};
use rsi_common::types::{ConversationEvent, SessionStatus};
use uuid::Uuid;

/// An unknown or out-of-scope target, as `authorize_agent_target` reports it.
fn is_scope_denial(error: &DaemonError) -> bool {
    match error {
        DaemonError::SessionNotFound(_) => true,
        DaemonError::InvalidParam(message) => message.starts_with(AGENT_READ_EVENTS_SCOPE_DENIED),
        _ => false,
    }
}

fn scope_denied(caller: Uuid, target: Uuid) -> DaemonError {
    DaemonError::InvalidParam(format!(
        "{AGENT_READ_EVENTS_SCOPE_DENIED}: session {caller} may not read events of {target}"
    ))
}

/// Clip a JSON value to the per-field cap; an oversized value is replaced by
/// the clipped text of its serialization.
fn clip_json(
    value: Option<Box<serde_json::Value>>,
    max_chars: usize,
) -> (Option<serde_json::Value>, bool) {
    let Some(value) = value else {
        return (None, false);
    };
    let text = value.to_string();
    let (clipped, was_clipped) = clip_chars(&text, max_chars);
    if was_clipped {
        (Some(serde_json::Value::String(clipped)), true)
    } else {
        (Some(*value), false)
    }
}

fn project_event(event: ConversationEvent, max_chars: usize) -> AgentSessionEventV1 {
    let (content, content_clipped) = clip_chars(&event.content, max_chars);
    let (tool_input, input_clipped) = clip_json(event.tool_input, max_chars);
    let provider_diagnostic = is_provider_diagnostic_metadata(event.metadata.as_deref());
    let (metadata, metadata_clipped) = clip_json(event.metadata, max_chars);
    AgentSessionEventV1 {
        sequence: event.sequence,
        event_type: event.event_type,
        role: event.role,
        tool_name: event.tool_name,
        tool_use_id: event.tool_use_id,
        content,
        truncated: content_clipped || input_clipped || metadata_clipped,
        tool_input,
        metadata,
        created_at: event.created_at,
        provider_diagnostic,
    }
}

fn is_terminal(status: SessionStatus) -> bool {
    matches!(
        status,
        SessionStatus::Completed
            | SessionStatus::Failed
            | SessionStatus::Interrupted
            | SessionStatus::Archived
            | SessionStatus::Deleted
    )
}

impl AgentControlHandle {
    /// # Errors
    /// `agent_verb_scope_denied` for an unknown or out-of-scope target
    /// (indistinguishable), an invalid-params code, or a persistence error.
    pub async fn agent_read_session_events(
        &self,
        caller_session_id: Uuid,
        request: AgentReadSessionEventsRequestV1,
    ) -> Result<AgentReadSessionEventsResultV1> {
        request
            .validate()
            .map_err(|code| DaemonError::InvalidParam(code.into()))?;
        let target = request.session_id;
        match self.authorize_agent_target(caller_session_id, target).await {
            Ok(_) => {}
            Err(error) if is_scope_denial(&error) => {
                // #2531: the live holder of a project manager slot reads the
                // tail of the earlier seats of the same slot (read-only), so a
                // context-cap successor can see what its predecessor said and
                // reported last; #118 extends it to the seats of an earlier
                // appointment an operator reappointment displaced. Any other
                // denial stays the one refusal.
                // #173: a live Issue-bound worker reads the session that
                // created its bound Issue (same project, excluding manager seats
                // and their continuations, #471), read-only, so a bug
                // report about that session is checked against its record.
                // #466: the same read-only admission for explicitly cited
                // same-project non-manager evidence sessions, while the binding is
                // live. Handoffs by the reader or its continuation lineage grant
                // no reach (#466 r2).
                let admitted = {
                    let store = self.store.lock().await;
                    store.manager_seat_reads_predecessor(caller_session_id, target)?
                        || store.issue_worker_reads_reporter(caller_session_id, target)?
                        || store.issue_worker_reads_cited_session(caller_session_id, target)?
                };
                if !admitted {
                    return Err(scope_denied(caller_session_id, target));
                }
            }
            Err(other) => return Err(other),
        }

        let limit = request.effective_limit();
        let max_bytes = request.effective_max_bytes() as usize;
        let store = self.store.lock().await;
        let tip_id = store.session_lineage_tip(target)?;
        let tip = store
            .get_session(tip_id)?
            .ok_or_else(|| scope_denied(caller_session_id, target))?;
        // #2384: a cold session reads through the archive merge; the page is the
        // one the unmoved transcript gives, and `storage` says where it came from.
        let storage = if store.cold_ref(&target.to_string())?.is_some() {
            AgentEventsStorageV1::Cold
        } else {
            AgentEventsStorageV1::Live
        };
        let (events, mut has_more, mut has_earlier) = store.read_session_events_page(
            target,
            request.after_sequence,
            limit,
            request.event_types.as_deref(),
        )?;
        let mut final_event = store.last_assistant_message_event(tip_id)?;
        if let Some(event) = final_event.as_mut()
            && let Some(report) =
                super::worker_result_guard::result_sha::normalized_report(&store, event)?
        {
            event.content = report;
        }
        drop(store);

        let field_max_chars = request.field_max_chars();
        let mut projected: Vec<AgentSessionEventV1> = events
            .into_iter()
            .map(|event| project_event(event, field_max_chars))
            .collect();
        let mut truncated = projected.iter().any(|event| event.truncated);

        // Enforce the page byte budget. A forward page keeps the oldest
        // events; a tail page keeps the newest. At least one event is always
        // returned so a caller can make progress.
        let sizes: Vec<usize> = projected
            .iter()
            .map(|event| serde_json::to_string(event).map_or(0, |text| text.len()))
            .collect();
        let total: usize = sizes.iter().sum();
        if total > max_bytes && projected.len() > 1 {
            truncated = true;
            if request.after_sequence.is_some() {
                let mut keep = 0;
                let mut used = 0;
                for size in &sizes {
                    if keep > 0 && used + size > max_bytes {
                        break;
                    }
                    used += size;
                    keep += 1;
                }
                projected.truncate(keep);
                has_more = true;
            } else {
                let mut start = sizes.len() - 1;
                let mut used = sizes[start];
                while start > 0 && used + sizes[start - 1] <= max_bytes {
                    start -= 1;
                    used += sizes[start];
                }
                projected.drain(..start);
                has_earlier = true;
            }
        }

        let final_message = final_event.map(|event| {
            let max_chars = if request.final_message_full {
                FINAL_MESSAGE_FULL_MAX_CHARS
            } else {
                FINAL_MESSAGE_MAX_CHARS
            };
            let (content, clipped) = clip_chars(&event.content, max_chars);
            truncated |= clipped;
            AgentFinalMessageV1 {
                sequence: event.sequence,
                content,
                truncated: clipped,
                created_at: event.created_at,
            }
        });
        let next_after_sequence = projected.last().map(|event| event.sequence);
        Ok(AgentReadSessionEventsResultV1 {
            session_id: target,
            tip_session_id: tip_id,
            status: tip.status,
            terminal_reason: is_terminal(tip.status)
                .then(|| tip.stop_reason.clone())
                .flatten(),
            events: projected,
            next_after_sequence,
            has_more,
            has_earlier,
            truncated,
            final_message,
            storage,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::agent_verbs::tests::{control_handle_with_store, test_session};
    use super::*;
    use crate::store::Store;
    use chrono::Utc;
    use rsi_common::agent_session_events::AGENT_READ_EVENT_FIELD_FULL_MAX_CHARS;
    use rsi_common::types::{EventType, Role, SessionKind};
    use std::sync::Arc;

    type SharedStore = Arc<rsid_core::store_lock::Mutex<Store>>;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn result_sha_final_message_projects_verified_hash_and_keeps_raw_event() {
        let (control, store, parent, child) = worker_with_events(2).await;
        let raw = "RESULT 1234567 status=green";
        let full = "1234567890123456789012345678901234567890";
        {
            let store = store.lock().await;
            let event = store.last_assistant_message_event(child).unwrap().unwrap();
            store.update_event_content(event.id, raw).unwrap();
            store.insert_session_diagnostic(&rsi_common::types::NewSessionDiagnosticV1 {
                session_id: child,
                timestamp: Utc::now(),
                level: rsi_common::types::SessionDiagnosticLevelV1::Warn,
                message: "worker_result_sha_validation".into(),
                fields: Some(serde_json::json!({"event_sequence":event.sequence,"replacements":[{"reported":"1234567","resolved":full}]})),
            }).unwrap();
        }
        let page = control
            .agent_read_session_events(parent, req(child))
            .await
            .unwrap();
        assert_eq!(
            page.final_message.unwrap().content,
            format!("RESULT {full} status=green")
        );
        assert_eq!(page.events[1].content, raw);
    }

    async fn insert(
        store: &SharedStore,
        id: Uuid,
        kind: SessionKind,
        parent: Option<Uuid>,
        lead: Option<Uuid>,
    ) {
        let mut row = test_session(id, std::path::PathBuf::from("/tmp"));
        row.session_kind = kind;
        row.status = SessionStatus::Running;
        row.parent_id = parent;
        row.lead_session_id = lead;
        store.lock().await.insert_session(&row).expect("insert");
    }

    async fn event(
        store: &SharedStore,
        session: Uuid,
        sequence: i32,
        event_type: EventType,
        role: Option<Role>,
        content: &str,
    ) {
        let event = ConversationEvent {
            id: 0,
            session_id: session,
            sequence,
            event_type,
            role,
            created_at: Utc::now(),
            content: content.to_string(),
            tool_name: None,
            tool_input: None,
            offload_id: None,
            tool_use_id: None,
            metadata: Some(Box::new(serde_json::json!({"n": sequence}))),
        };
        store.lock().await.insert_event(&event).expect("event");
    }

    fn req(session_id: Uuid) -> AgentReadSessionEventsRequestV1 {
        AgentReadSessionEventsRequestV1 {
            session_id,
            after_sequence: None,
            limit: None,
            event_types: None,
            max_bytes: None,
            final_message_full: false,
            full_fields: false,
        }
    }

    async fn worker_with_events(
        n: i32,
    ) -> (
        crate::session::agent_verbs::AgentControlHandle,
        SharedStore,
        Uuid,
        Uuid,
    ) {
        let (control, store) = control_handle_with_store();
        let parent = Uuid::new_v4();
        let child = Uuid::new_v4();
        insert(&store, parent, SessionKind::Task, None, None).await;
        insert(&store, child, SessionKind::Task, Some(parent), None).await;
        for seq in 1..=n {
            let (kind, role) = if seq % 2 == 0 {
                (EventType::Message, Some(Role::Assistant))
            } else {
                (EventType::ToolUse, Some(Role::Assistant))
            };
            event(&store, child, seq, kind, role, &format!("event {seq}")).await;
        }
        (control, store, parent, child)
    }

    /// #2384: a cold session reads exactly like a hot one and says where it came
    /// from. The rows are archived by the real stage pass and moved by the
    /// test-only move seam.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn a_cold_session_reads_identically_and_reports_its_storage() {
        use crate::store::cold_archive::{
            ColdMode, ColdSettings, DirectStoreAccess, MoveOutcome, PassLimits, PassParams,
            open_read_connection, run_stage_pass,
        };
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("rsi.db");
        let (control, store) = super::super::agent_verbs::tests::control_handle_for_file_store(
            Store::open_test_fixture(&database).unwrap(),
        );
        let parent = Uuid::new_v4();
        let child = Uuid::new_v4();
        insert(&store, parent, SessionKind::Task, None, None).await;
        insert(&store, child, SessionKind::Task, Some(parent), None).await;
        let old = (Utc::now() - chrono::Duration::days(61))
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        {
            let guard = store.lock().await;
            for (sequence, event_type, role) in [
                (1, "Message", Some("User")),
                (2, "ToolUse", Some("Assistant")),
                (3, "ToolResult", None),
                (4, "ToolUse", Some("Assistant")),
                (5, "ToolResult", None),
                (6, "Message", Some("Assistant")),
            ] {
                guard
                    .conn
                    .execute(
                        "INSERT INTO conversation_events(session_id, sequence, event_type, role,
                            content, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        rusqlite::params![
                            child.to_string(),
                            sequence,
                            event_type,
                            role,
                            format!("{event_type} {sequence}"),
                            old
                        ],
                    )
                    .unwrap();
            }
            guard
                .update_session_status(child, SessionStatus::Archived)
                .unwrap();
        }
        let pages = |after: Option<i32>, types: Option<Vec<EventType>>| {
            let control = &control;
            async move {
                let mut request = req(child);
                request.after_sequence = after;
                request.limit = Some(3);
                request.event_types = types;
                control
                    .agent_read_session_events(parent, request)
                    .await
                    .unwrap()
            }
        };
        let mut before = Vec::new();
        for (after, types) in [
            (None, None),
            (Some(1), None),
            (Some(0), Some(vec![EventType::ToolResult])),
            (None, Some(vec![EventType::ToolUse])),
        ] {
            before.push(pages(after, types).await);
        }
        assert!(
            before
                .iter()
                .all(|page| page.storage == AgentEventsStorageV1::Live)
        );

        {
            let guard = store.lock().await;
            let read = open_read_connection(&database).unwrap();
            let settings = ColdSettings {
                enabled: true,
                after_days: 14,
                dir: directory.path().join("archive"),
                forbidden_roots: Vec::new(),
            };
            let run_id = guard.cold_begin_run("run_now", ColdMode::Stage).unwrap();
            let params = PassParams {
                run_id,
                settings: &settings,
                limits: PassLimits {
                    batch_max_sessions: 32,
                    batch_max_raw_bytes: 64 * 1024 * 1024,
                    wall_budget: std::time::Duration::from_secs(120),
                    batch_sleep: std::time::Duration::ZERO,
                    gate_poll: std::time::Duration::ZERO,
                },
                batch_gate: None,
                cancel: None,
            };
            let report = run_stage_pass(&DirectStoreAccess(&guard), &read, &params).unwrap();
            assert_eq!(report.sessions_staged, 1, "{report:?}");
            assert!(matches!(
                guard.cold_move_session_for_test(child).unwrap(),
                MoveOutcome::Moved(_)
            ));
        }

        let mut after = Vec::new();
        for (after_sequence, types) in [
            (None, None),
            (Some(1), None),
            (Some(0), Some(vec![EventType::ToolResult])),
            (None, Some(vec![EventType::ToolUse])),
        ] {
            after.push(pages(after_sequence, types).await);
        }
        for (cold, hot) in after.into_iter().zip(before) {
            assert_eq!(cold.storage, AgentEventsStorageV1::Cold);
            let mut cold = cold;
            cold.storage = AgentEventsStorageV1::Live;
            assert_eq!(cold, hot, "the cold page equals the pre-move page");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn refuses_unknown_and_out_of_scope_targets_identically() {
        let (control, store, _parent, child) = worker_with_events(2).await;
        let stranger = Uuid::new_v4();
        insert(&store, stranger, SessionKind::Task, None, None).await;
        let missing = Uuid::new_v4();

        let denied = control
            .agent_read_session_events(stranger, req(child))
            .await
            .expect_err("a stranger may not read another session's events");
        let unknown = control
            .agent_read_session_events(stranger, req(missing))
            .await
            .expect_err("an unknown session is refused");
        assert_eq!(
            denied.to_string(),
            scope_denied(stranger, child).to_string()
        );
        assert_eq!(
            unknown.to_string(),
            scope_denied(stranger, missing).to_string()
        );
        // Same shape: only the caller-supplied id differs.
        assert_eq!(
            denied.to_string().replace(&child.to_string(), "T"),
            unknown.to_string().replace(&missing.to_string(), "T")
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn final_message_skips_provider_diagnostics_and_flags_them() {
        let (control, store, parent, child) = worker_with_events(2).await;
        // The agent's verdict is event 2; a stderr diagnostic lands after it.
        let diagnostic = ConversationEvent {
            id: 0,
            session_id: child,
            sequence: 3,
            event_type: EventType::Message,
            role: Some(Role::Assistant),
            created_at: Utc::now(),
            content: "**Provider diagnostic (stderr; 26 records)**\n```\nstale rollout\n```"
                .to_string(),
            tool_name: None,
            tool_input: None,
            offload_id: None,
            tool_use_id: None,
            metadata: Some(Box::new(
                rsi_common::agent_session_events::provider_diagnostic_metadata("stderr", false),
            )),
        };
        store
            .lock()
            .await
            .insert_event(&diagnostic)
            .expect("diagnostic");

        let page = control
            .agent_read_session_events(parent, req(child))
            .await
            .expect("read");
        let final_message = page.final_message.expect("final message");
        assert_eq!(final_message.sequence, 2);
        assert_eq!(final_message.content, "event 2");
        let flagged: Vec<(i32, bool)> = page
            .events
            .iter()
            .map(|event| (event.sequence, event.provider_diagnostic))
            .collect();
        assert_eq!(flagged, vec![(1, false), (2, false), (3, true)]);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn parent_epic_lead_and_self_read_and_events_are_bounded() {
        let (control, store, parent, child) = worker_with_events(5).await;
        let page = control
            .agent_read_session_events(parent, req(child))
            .await
            .expect("a parent reads its child");
        assert_eq!(page.events.len(), 5);
        assert_eq!(page.tip_session_id, child);
        assert_eq!(
            page.final_message.as_ref().map(|m| m.content.as_str()),
            Some("event 4")
        );
        control
            .agent_read_session_events(child, req(child))
            .await
            .expect("self read matches AgentGetStatus scope");

        // A lead reads a child of the Epic it leads.
        let lead = Uuid::new_v4();
        let epic = Uuid::new_v4();
        insert(&store, lead, SessionKind::Task, Some(epic), None).await;
        insert(&store, epic, SessionKind::Epic, None, Some(lead)).await;
        let sibling = Uuid::new_v4();
        insert(&store, sibling, SessionKind::Task, Some(epic), None).await;
        event(
            &store,
            sibling,
            1,
            EventType::Message,
            Some(Role::Assistant),
            "done",
        )
        .await;
        let led = control
            .agent_read_session_events(lead, req(sibling))
            .await
            .expect("a lead reads a child of its Epic");
        assert_eq!(led.final_message.unwrap().content, "done");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn pages_tail_forward_and_filters_types() {
        let (control, _store, parent, child) = worker_with_events(5).await;
        let mut tail = req(child);
        tail.limit = Some(2);
        let page = control
            .agent_read_session_events(parent, tail)
            .await
            .unwrap();
        assert_eq!(
            page.events.iter().map(|e| e.sequence).collect::<Vec<_>>(),
            [4, 5]
        );
        assert!(page.has_earlier && !page.has_more);
        assert_eq!(page.next_after_sequence, Some(5));

        let mut forward = req(child);
        forward.after_sequence = Some(1);
        forward.limit = Some(2);
        let page = control
            .agent_read_session_events(parent, forward)
            .await
            .unwrap();
        assert_eq!(
            page.events.iter().map(|e| e.sequence).collect::<Vec<_>>(),
            [2, 3]
        );
        assert!(page.has_more && page.has_earlier);
        assert_eq!(page.next_after_sequence, Some(3));

        let mut last = req(child);
        last.after_sequence = Some(3);
        last.limit = Some(10);
        let page = control
            .agent_read_session_events(parent, last)
            .await
            .unwrap();
        assert_eq!(page.events.len(), 2);
        assert!(!page.has_more);

        let mut messages = req(child);
        messages.event_types = Some(vec![EventType::Message]);
        let page = control
            .agent_read_session_events(parent, messages)
            .await
            .unwrap();
        assert_eq!(
            page.events.iter().map(|e| e.sequence).collect::<Vec<_>>(),
            [2, 4]
        );
        assert!(page.events.iter().all(|e| e.metadata.is_some()));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn clips_fields_and_enforces_the_page_byte_budget() {
        let (control, store, parent, child) = worker_with_events(0).await;
        let long = "x".repeat(5_000);
        for seq in 1..=4 {
            event(
                &store,
                child,
                seq,
                EventType::Message,
                Some(Role::Assistant),
                &long,
            )
            .await;
        }
        let page = control
            .agent_read_session_events(parent, req(child))
            .await
            .unwrap();
        assert!(page.truncated);
        assert!(
            page.events
                .iter()
                .all(|e| e.truncated && e.content.chars().count() == 2_000)
        );

        let mut small = req(child);
        small.max_bytes = Some(3_000);
        let page = control
            .agent_read_session_events(parent, small)
            .await
            .unwrap();
        assert_eq!(page.events.len(), 1, "a 3KB budget fits one clipped event");
        assert!(page.truncated && page.has_earlier);
        assert_eq!(page.events[0].sequence, 4, "the tail keeps the newest");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn full_fields_opt_in_exports_a_long_tool_call_whole() {
        let (control, store, parent, child) = worker_with_events(0).await;
        let body = "line of notes\n".repeat(800); // about 11 KB
        let tool_input = serde_json::json!({"file_path": "notes.md", "content": body});
        let tool_event = ConversationEvent {
            id: 0,
            session_id: child,
            sequence: 1,
            event_type: EventType::ToolUse,
            role: Some(Role::Assistant),
            created_at: Utc::now(),
            content: String::new(),
            tool_name: Some("Write".into()),
            tool_input: Some(Box::new(tool_input.clone())),
            offload_id: None,
            tool_use_id: Some("toolu_1".into()),
            metadata: None,
        };
        store
            .lock()
            .await
            .insert_event(&tool_event)
            .expect("tool event");

        // Default: the tool input is clipped to the 2,000-character cap.
        let mut one = req(child);
        one.limit = Some(1);
        let page = control
            .agent_read_session_events(parent, one.clone())
            .await
            .unwrap();
        assert!(page.events[0].truncated);
        let clipped = page.events[0].tool_input.as_ref().unwrap();
        assert_eq!(
            clipped.as_str().map(|text| text.chars().count()),
            Some(2_000)
        );

        // Opt-in: the whole input comes back, unclipped.
        one.full_fields = true;
        one.max_bytes = Some(262_144);
        let page = control
            .agent_read_session_events(parent, one)
            .await
            .unwrap();
        assert!(!page.events[0].truncated);
        assert!(!page.truncated);
        assert_eq!(page.events[0].tool_input.as_ref(), Some(&tool_input));

        // An event above even the raised cap stays truncated.
        let huge = "y".repeat(AGENT_READ_EVENT_FIELD_FULL_MAX_CHARS + 10);
        event(
            &store,
            child,
            2,
            EventType::Message,
            Some(Role::Assistant),
            &huge,
        )
        .await;
        let mut tail = req(child);
        tail.full_fields = true;
        tail.limit = Some(1);
        tail.max_bytes = Some(262_144);
        let page = control
            .agent_read_session_events(parent, tail)
            .await
            .unwrap();
        assert!(page.events[0].truncated);
        assert_eq!(
            page.events[0].content.chars().count(),
            AGENT_READ_EVENT_FIELD_FULL_MAX_CHARS
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn full_final_message_opt_in_raises_the_content_cap() {
        let (control, store, parent, child) = worker_with_events(0).await;
        let content = "handoff detail ".repeat(1_000);
        event(
            &store,
            child,
            1,
            EventType::Message,
            Some(Role::Assistant),
            &content,
        )
        .await;

        let default_page = control
            .agent_read_session_events(parent, req(child))
            .await
            .unwrap();
        let default_message = default_page.final_message.unwrap();
        assert_eq!(
            default_message.content.chars().count(),
            FINAL_MESSAGE_MAX_CHARS
        );
        assert!(default_message.truncated);

        let mut full = req(child);
        full.final_message_full = true;
        let full_page = control
            .agent_read_session_events(parent, full)
            .await
            .unwrap();
        let full_message = full_page.final_message.unwrap();
        assert_eq!(full_message.content, content);
        assert!(!full_message.truncated);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn final_message_and_terminal_reason_follow_the_rotation_tip() {
        let (control, store, parent, child) = worker_with_events(2).await;
        let tip = Uuid::new_v4();
        let mut row = test_session(tip, std::path::PathBuf::from("/tmp"));
        row.session_kind = SessionKind::Task;
        row.status = SessionStatus::Failed;
        row.stop_reason = Some("provider_error:boom".into());
        row.parent_id = Some(parent);
        row.continued_from = Some(child);
        store.lock().await.insert_session(&row).unwrap();
        event(
            &store,
            tip,
            1,
            EventType::Message,
            Some(Role::Assistant),
            "tip result",
        )
        .await;

        let page = control
            .agent_read_session_events(parent, req(child))
            .await
            .unwrap();
        assert_eq!(page.tip_session_id, tip);
        assert_eq!(page.status, SessionStatus::Failed);
        assert_eq!(page.terminal_reason.as_deref(), Some("provider_error:boom"));
        assert_eq!(page.final_message.unwrap().content, "tip result");
        assert_eq!(page.events.len(), 2, "events come from the named session");
    }

    /// #2531: a context-cap successor of a project manager reads the tail of
    /// the seat it replaced (and the one before), while a stranger, a sibling
    /// project's seat and a later seat stay refused with the one denial.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn a_pm_successor_reads_its_predecessors_tail_and_nobody_elses_2531() {
        use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
        use rsi_common::types::Project;
        let (control, store) = control_handle_with_store();
        let (project, other_project) = (Uuid::new_v4(), Uuid::new_v4());
        let (first, stranger, foreign) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        {
            let guard = store.lock().await;
            for id in [project, other_project] {
                let now = Utc::now();
                guard
                    .insert_project(&Project {
                        id,
                        name: format!("P {id}"),
                        path: None,
                        description: None,
                        color: Project::DEFAULT_COLOR.into(),
                        context_files: None,
                        created_at: now,
                        updated_at: now,
                    })
                    .unwrap();
            }
            for (id, project_id) in [
                (first, project),
                (stranger, project),
                (foreign, other_project),
            ] {
                let mut row = test_session(id, std::path::PathBuf::from("/tmp"));
                row.project_id = Some(project_id);
                row.status = SessionStatus::Running;
                guard.insert_session(&row).unwrap();
            }
            guard
                .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                    project_id: project,
                    session_id: first,
                    epic_ids: None,
                    group_ids: vec![],
                    expected_row_version: 0,
                })
                .unwrap();
        }
        event(
            &store,
            first,
            1,
            EventType::Message,
            Some(Role::Assistant),
            "first seat tail",
        )
        .await;
        let mut previous = first;
        let mut seats = vec![first];
        for _ in 0..2 {
            let next = Uuid::new_v4();
            {
                let guard = store.lock().await;
                let mut row = test_session(next, std::path::PathBuf::from("/tmp"));
                row.project_id = Some(project);
                row.continued_from = Some(previous);
                row.rotation_depth =
                    guard.get_session(previous).unwrap().unwrap().rotation_depth + 1;
                row.status = SessionStatus::Running;
                guard.insert_session(&row).unwrap();
                guard
                    .conn
                    .execute(
                        "UPDATE sessions SET status='Archived' WHERE id=?1",
                        [previous.to_string()],
                    )
                    .unwrap();
                assert!(
                    guard
                        .record_harness_manager_rotation(previous, next)
                        .unwrap()
                );
            }
            event(
                &store,
                next,
                1,
                EventType::Message,
                Some(Role::Assistant),
                "later seat tail",
            )
            .await;
            seats.push(next);
            previous = next;
        }
        let (retired_first, retired_second, live) = (seats[0], seats[1], seats[2]);

        for predecessor in [retired_second, retired_first] {
            let page = control
                .agent_read_session_events(live, req(predecessor))
                .await
                .unwrap();
            assert_eq!(page.session_id, predecessor);
            assert!(!page.events.is_empty());
        }
        let page = control
            .agent_read_session_events(live, req(retired_first))
            .await
            .unwrap();
        assert_eq!(page.events[0].content, "first seat tail");

        for (caller, target) in [
            (retired_first, live),
            (retired_second, live),
            (stranger, retired_first),
            (live, foreign),
            (live, Uuid::new_v4()),
        ] {
            let error = control
                .agent_read_session_events(caller, req(target))
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(AGENT_READ_EVENTS_SCOPE_DENIED),
                "{caller} -> {target}: {error}"
            );
        }
    }

    /// #118: the operator reappoints a fresh, parentless session to pick up
    /// from the old manager (no `continued_from`, a new appointment anchor).
    /// The new seat reads every seat of the displaced appointment's lineage,
    /// proven by the durable appointment retirement record; a prior seat of
    /// another project, a non-manager, the new seat's own worker and a
    /// displaced seat stay refused with the one denial.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn a_reappointed_pm_reads_the_displaced_seats_of_its_own_project_only_118() {
        use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
        use rsi_common::types::Project;
        let (control, store) = control_handle_with_store();
        let (project, other_project) = (Uuid::new_v4(), Uuid::new_v4());
        let [old, old_next, seat, other_old, other_seat, stranger, worker] =
            std::array::from_fn(|_| Uuid::new_v4());
        let appoint = |guard: &crate::store::Store, project_id: Uuid, session_id: Uuid| {
            let expected_row_version = guard
                .get_harness_manager(project_id)
                .unwrap()
                .map_or(0, |config| config.row_version);
            guard
                .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                    project_id,
                    session_id,
                    epic_ids: None,
                    group_ids: vec![],
                    expected_row_version,
                })
                .unwrap();
        };
        {
            let guard = store.lock().await;
            for id in [project, other_project] {
                let now = Utc::now();
                guard
                    .insert_project(&Project {
                        id,
                        name: format!("P {id}"),
                        path: None,
                        description: None,
                        color: Project::DEFAULT_COLOR.into(),
                        context_files: None,
                        created_at: now,
                        updated_at: now,
                    })
                    .unwrap();
            }
            for (id, project_id) in [
                (old, project),
                (other_old, other_project),
                (stranger, project),
            ] {
                let mut row = test_session(id, std::path::PathBuf::from("/tmp"));
                row.project_id = Some(project_id);
                row.status = SessionStatus::Running;
                guard.insert_session(&row).unwrap();
            }
            appoint(&guard, project, old);
            appoint(&guard, other_project, other_old);
            // The old appointment rotated once on its own (context cap).
            let mut row = test_session(old_next, std::path::PathBuf::from("/tmp"));
            row.project_id = Some(project);
            row.continued_from = Some(old);
            row.rotation_depth = 1;
            row.status = SessionStatus::Running;
            guard.insert_session(&row).unwrap();
            guard
                .conn
                .execute(
                    "UPDATE sessions SET status='Archived' WHERE id=?1",
                    [old.to_string()],
                )
                .unwrap();
            assert!(
                guard
                    .record_harness_manager_rotation(old, old_next)
                    .unwrap()
            );
            // The operator seats fresh, parentless sessions in both projects.
            for (id, project_id) in [(seat, project), (other_seat, other_project)] {
                let mut row = test_session(id, std::path::PathBuf::from("/tmp"));
                row.project_id = Some(project_id);
                row.status = SessionStatus::Running;
                guard.insert_session(&row).unwrap();
                appoint(&guard, project_id, id);
            }
            let mut row = test_session(worker, std::path::PathBuf::from("/tmp"));
            row.project_id = Some(project);
            row.parent_id = Some(seat);
            row.status = SessionStatus::Running;
            guard.insert_session(&row).unwrap();
        }
        for (id, text) in [
            (old, "old anchor tail"),
            (old_next, "old seat tail"),
            (other_old, "other project tail"),
        ] {
            event(
                &store,
                id,
                1,
                EventType::Message,
                Some(Role::Assistant),
                text,
            )
            .await;
        }

        for (predecessor, text) in [(old_next, "old seat tail"), (old, "old anchor tail")] {
            let page = control
                .agent_read_session_events(seat, req(predecessor))
                .await
                .unwrap();
            assert_eq!(page.session_id, predecessor);
            assert_eq!(page.events[0].content, text);
        }
        let page = control
            .agent_read_session_events(other_seat, req(other_old))
            .await
            .unwrap();
        assert_eq!(page.events[0].content, "other project tail");

        for (caller, target) in [
            (seat, other_old),
            (other_seat, old_next),
            (stranger, old_next),
            (worker, old_next),
            (worker, old),
            (old_next, seat),
        ] {
            let error = control
                .agent_read_session_events(caller, req(target))
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(AGENT_READ_EVENTS_SCOPE_DENIED),
                "{caller} -> {target}: {error}"
            );
        }
    }

    /// #466: evidence citations grant only bounded event reads, only inside
    /// the binding's project and lifetime, and only for full lowercase UUIDs.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn an_issue_bound_worker_reads_cited_evidence_sessions_466() {
        use rsi_common::harness_manager::{
            ConfigureHarnessManagerRequestV1, HarnessManagerConfigV1,
        };
        use rsi_common::harness_manager_v2::{
            ManagerActionStateV2, ManagerActionV2, ManagerLaunchChoiceV2,
        };
        use rsi_common::types::{NewIssue, Project, SessionProvider};

        let (control, store) = control_handle_with_store();
        let (project, other_project) = (Uuid::new_v4(), Uuid::new_v4());
        let [
            pm,
            worker,
            successor,
            body,
            handoff,
            brief,
            foreign,
            uncited,
            prefix,
            extended,
            predecessor,
            other_worker,
            self_cited,
            predecessor_cited,
            successor_cited,
        ] = std::array::from_fn(|_| Uuid::new_v4());
        let upper = Uuid::from_u128(u128::MAX);
        let config = HarnessManagerConfigV1 {
            project_id: project,
            manager_session_id: pm,
            current_session_id: Some(pm),
            epic_ids: vec![],
            scope_mode: Default::default(),
            selected_epic_ids: None,
            group_ids: vec![],
            row_version: 1,
            updated_at: Utc::now(),
        };
        let bind = |guard: &Store, issue: Uuid, target: Uuid| {
            let operation = guard
                .seed_manager_action_with_context_for_test(
                    &config,
                    ManagerActionV2::CreateSession {
                        parent_id: pm,
                        kind: SessionKind::Task,
                        query: format!("Inspect `{brief}`, `{foreign}` and manager `{pm}`."),
                        launch: ManagerLaunchChoiceV2 {
                            provider: SessionProvider::Codex,
                            model: "test".into(),
                            effort: None,
                        },
                        sandbox_source: None,
                    },
                    ManagerActionStateV2::Succeeded,
                    Some(target),
                    pm,
                )
                .unwrap();
            // Match admission's binding beside the real operation/context.
            guard
                .conn
                .execute(
                    "UPDATE harness_manager_v2_operations
                 SET payload_json=json_set(payload_json,'$.issue_binding',json(?2)) WHERE id=?1",
                    rusqlite::params![
                        operation.to_string(),
                        serde_json::json!({"issue_id":issue}).to_string()
                    ],
                )
                .unwrap();
        };
        let issue = {
            let guard = store.lock().await;
            for id in [project, other_project] {
                guard
                    .insert_project(&Project {
                        id,
                        name: format!("P {id}"),
                        path: None,
                        description: None,
                        color: Project::DEFAULT_COLOR.into(),
                        context_files: None,
                        created_at: Utc::now(),
                        updated_at: Utc::now(),
                    })
                    .unwrap();
            }
            for id in [
                pm,
                predecessor,
                worker,
                successor,
                body,
                handoff,
                brief,
                foreign,
                uncited,
                prefix,
                upper,
                extended,
                other_worker,
                self_cited,
                predecessor_cited,
                successor_cited,
            ] {
                let mut row = test_session(id, std::path::PathBuf::from("/tmp"));
                row.project_id = Some(if id == foreign {
                    other_project
                } else {
                    project
                });
                row.status = SessionStatus::Running;
                row.continued_from = if id == worker {
                    Some(predecessor)
                } else if id == successor {
                    Some(worker)
                } else {
                    None
                };
                guard.insert_session(&row).unwrap();
            }
            guard
                .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                    project_id: project,
                    session_id: pm,
                    epic_ids: None,
                    group_ids: vec![],
                    expected_row_version: 0,
                })
                .unwrap();
            let issue = guard.create_issue(&NewIssue {
                project_id: project,
                title: "evidence report".into(),
                body: format!(
                    "Inspect ({body}). Manager: {pm}. Foreign: {foreign}. Prefix: {}. Upper: {}. Extended: x{extended} {extended}a {extended}-extra {extended}_extra.",
                    &prefix.to_string()[..8], upper.to_string().to_uppercase(),
                ),
                priority: None,
                labels: vec![],
                created_by_session_id: None,
                assignee: None,
                idea_id: None,
                source_event_id: None,
                source_finding_ref: None,
            }).unwrap().id;
            issue
        };
        // Only the outside worker's handoff grants citation reach. Its binding
        // is superseded before the reading worker is admitted.
        for (author, citation, key) in [
            (other_worker, handoff, "outside-evidence"),
            (predecessor, predecessor_cited, "predecessor-evidence"),
            (successor, successor_cited, "successor-evidence"),
            (worker, self_cited, "own-evidence"),
        ] {
            let guard = store.lock().await;
            bind(&guard, issue, author);
            let mut request = rsi_common::rpc::AgentUpdateIssueRequestV1::new(1, key.into());
            request.issue_id = Some(issue);
            request.handoff = Some(format!("Check [{citation}], manager {pm} and {foreign}"));
            guard.agent_append_issue_handoff(author, &request).unwrap();
        }
        for target in [body, handoff, brief] {
            event(
                &store,
                target,
                1,
                EventType::Message,
                Some(Role::Assistant),
                "evidence tail",
            )
            .await;
            let page = control
                .agent_read_session_events(worker, req(target))
                .await
                .unwrap();
            assert_eq!(page.session_id, target);
            assert_eq!(page.events[0].content, "evidence tail");
            assert_eq!(page.final_message.unwrap().content, "evidence tail");
            let error = control
                .agent_get_status(worker, target)
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(AGENT_READ_EVENTS_SCOPE_DENIED),
                "another verb: {error}"
            );
        }
        for target in [
            foreign,
            uncited,
            prefix,
            upper,
            extended,
            pm,
            self_cited,
            predecessor_cited,
            successor_cited,
            Uuid::new_v4(),
        ] {
            let error = control
                .agent_read_session_events(worker, req(target))
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(AGENT_READ_EVENTS_SCOPE_DENIED),
                "{target}: {error}"
            );
        }
        // A later admitted launch retires the old worker's citation grant.
        bind(&*store.lock().await, issue, successor);
        let error = control
            .agent_read_session_events(worker, req(body))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(AGENT_READ_EVENTS_SCOPE_DENIED),
            "superseded: {error}"
        );
        control
            .agent_read_session_events(successor, req(body))
            .await
            .unwrap();
        control
            .agent_read_session_events(successor, req(handoff))
            .await
            .unwrap();
        for target in [self_cited, predecessor_cited, successor_cited] {
            let error = control
                .agent_read_session_events(successor, req(target))
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(AGENT_READ_EVENTS_SCOPE_DENIED),
                "rotated self-grant: {error}"
            );
        }
        store
            .lock()
            .await
            .conn
            .execute(
                "UPDATE sessions SET status='Completed' WHERE id=?1",
                [successor.to_string()],
            )
            .unwrap();
        for target in [body, handoff, brief] {
            let error = control
                .agent_read_session_events(successor, req(target))
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(AGENT_READ_EVENTS_SCOPE_DENIED),
                "ended binding: {error}"
            );
        }
    }

    /// #471 / hub #3342: reporter reach never exposes a manager transcript,
    /// including former seats and continuations. Ordinary reporters still work.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn issue_bound_reads_refuse_manager_seat_reporters_471() {
        use crate::store::portfolio_nodes::PortfolioGrantor;
        use rsi_common::harness_manager::{
            ConfigureHarnessManagerRequestV1, HarnessManagerConfigV1,
        };
        use rsi_common::harness_manager_v2::{
            ManagerActionStateV2, ManagerActionV2, ManagerLaunchChoiceV2,
        };
        use rsi_common::portfolio_nodes::ConfigurePortfolioNodeRequestV1;
        use rsi_common::types::{NewIssue, Project, SessionProvider};

        for tier in ["project", "portfolio", "global"] {
            let (control, store) = control_handle_with_store();
            let project = Uuid::new_v4();
            let [precursor, seat, rotation, replacement, ordinary, worker] =
                std::array::from_fn(|_| Uuid::new_v4());
            let config = {
                let guard = store.lock().await;
                guard
                    .insert_project(&Project {
                        id: project,
                        name: format!("P {project}"),
                        path: None,
                        description: None,
                        color: Project::DEFAULT_COLOR.into(),
                        context_files: None,
                        created_at: Utc::now(),
                        updated_at: Utc::now(),
                    })
                    .unwrap();
                for id in [precursor, seat, rotation, replacement, ordinary, worker] {
                    let mut row = test_session(id, std::path::PathBuf::from("/tmp"));
                    row.project_id = Some(project);
                    row.status = SessionStatus::Running;
                    row.continued_from = if id == seat {
                        Some(precursor)
                    } else if id == rotation {
                        Some(seat)
                    } else {
                        None
                    };
                    guard.insert_session(&row).unwrap();
                }
                if tier == "project" {
                    for (session_id, expected_row_version) in [(seat, 0), (replacement, 1)] {
                        guard
                            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                                project_id: project,
                                session_id,
                                epic_ids: None,
                                group_ids: vec![],
                                expected_row_version,
                            })
                            .unwrap();
                    }
                } else {
                    let mut request = ConfigurePortfolioNodeRequestV1 {
                        node_id: None,
                        parent_node_id: None,
                        adopt_node_ids: vec![],
                        expected_parent_grant_version: None,
                        tier_label: tier.into(),
                        seat_session_id: seat,
                        project_ids: vec![project],
                        allowed_launches: vec![ManagerLaunchChoiceV2 {
                            provider: SessionProvider::Codex,
                            model: "test".into(),
                            effort: None,
                        }],
                        policy: Default::default(),
                        child_policy: None,
                        max_direct_reports: 5,
                        expected_node_grant_version: 0,
                        expected_authority_epoch: 0,
                        idempotency_key: "reporter-seat".into(),
                    };
                    let node = guard
                        .configure_portfolio_node(
                            &request,
                            PortfolioGrantor::Operator,
                            "operator:test",
                        )
                        .unwrap();
                    request.node_id = Some(node.node_id);
                    request.seat_session_id = replacement;
                    request.expected_node_grant_version = node.grant.grant_version;
                    request.expected_authority_epoch = node.authority_epoch;
                    request.idempotency_key = "reporter-replacement".into();
                    guard
                        .configure_portfolio_node(
                            &request,
                            PortfolioGrantor::Operator,
                            "operator:test",
                        )
                        .unwrap();
                }
                HarnessManagerConfigV1 {
                    project_id: project,
                    manager_session_id: replacement,
                    current_session_id: Some(replacement),
                    epic_ids: vec![],
                    scope_mode: Default::default(),
                    selected_epic_ids: None,
                    group_ids: vec![],
                    row_version: 1,
                    updated_at: Utc::now(),
                }
            };
            for (target, via_reporter) in [precursor, seat, rotation, replacement, ordinary]
                .into_iter()
                .flat_map(|target| [(target, true), (target, false)])
            {
                {
                    let guard = store.lock().await;
                    let issue = guard
                        .create_issue(&NewIssue {
                            project_id: project,
                            title: "reporter privacy".into(),
                            body: if via_reporter {
                                String::new()
                            } else {
                                format!("Evidence {target}")
                            },
                            priority: None,
                            labels: vec![],
                            created_by_session_id: via_reporter.then_some(target),
                            assignee: None,
                            idea_id: None,
                            source_event_id: None,
                            source_finding_ref: None,
                        })
                        .unwrap()
                        .id;
                    let operation = guard
                        .seed_manager_action_with_context_for_test(
                            &config,
                            ManagerActionV2::CreateSession {
                                parent_id: replacement,
                                kind: SessionKind::Task,
                                query: if via_reporter {
                                    "inspect reporter".into()
                                } else {
                                    format!("Inspect {target}")
                                },
                                launch: ManagerLaunchChoiceV2 {
                                    provider: SessionProvider::Codex,
                                    model: "test".into(),
                                    effort: None,
                                },
                                sandbox_source: None,
                            },
                            ManagerActionStateV2::Succeeded,
                            Some(worker),
                            replacement,
                        )
                        .unwrap();
                    guard.conn.execute(
                        "UPDATE harness_manager_v2_operations SET payload_json=json_set(payload_json,'$.issue_binding',json(?2)) WHERE id=?1",
                        rusqlite::params![operation.to_string(), serde_json::json!({"issue_id":issue}).to_string()],
                    ).unwrap();
                }
                let result = control.agent_read_session_events(worker, req(target)).await;
                if target == ordinary {
                    assert_eq!(
                        result.unwrap().session_id,
                        ordinary,
                        "{tier}: ordinary reporter"
                    );
                } else {
                    let error = result.unwrap_err().to_string();
                    assert!(
                        error.contains(AGENT_READ_EVENTS_SCOPE_DENIED),
                        "{tier} {target}, reporter={via_reporter}: {error}"
                    );
                }
            }
        }
    }

    /// #173: a live Issue-bound worker reads, read-only, the events of the
    /// same-project session that created its bound Issue. A non-creator, a
    /// creator in another project, an ended or superseded binding and any
    /// other verb (`AgentGetStatus`) stay refused with the one denial.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn an_issue_bound_worker_reads_the_session_that_filed_its_issue_173() {
        use rsi_common::types::{NewIssue, Project};
        let (control, store) = control_handle_with_store();
        let (project, other_project) = (Uuid::new_v4(), Uuid::new_v4());
        let [
            pm,
            reporter,
            worker,
            stranger,
            foreign,
            foreign_worker,
            successor,
        ] = std::array::from_fn(|_| Uuid::new_v4());
        let bind = |guard: &Store, issue: Uuid, target: Uuid| {
            let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
            let payload = serde_json::json!({"issue_binding": {"issue_id": issue.to_string()}});
            guard
                .conn
                .execute(
                    "INSERT INTO harness_manager_v2_operations(id,project_id,manager_session_id,
                        scope_version,policy_version,idempotency_key,fingerprint,kind,payload_json,
                        state,target_session_id,outcome_json,not_before,created_at,updated_at)
                     VALUES(?1,?2,?3,1,1,?1,'fixture','lifecycle_action',?4,'succeeded',?5,
                        '{}',?6,?6,?6)",
                    rusqlite::params![
                        Uuid::new_v4().to_string(),
                        project.to_string(),
                        pm.to_string(),
                        payload.to_string(),
                        target.to_string(),
                        now
                    ],
                )
                .unwrap();
        };
        let file_issue = |guard: &Store, creator: Uuid| {
            guard
                .create_issue(&NewIssue {
                    project_id: project,
                    title: "report".into(),
                    body: String::new(),
                    priority: None,
                    labels: vec![],
                    created_by_session_id: Some(creator),
                    assignee: None,
                    idea_id: None,
                    source_event_id: None,
                    source_finding_ref: None,
                })
                .unwrap()
                .id
        };
        let issue = {
            let guard = store.lock().await;
            for id in [project, other_project] {
                let now = Utc::now();
                guard
                    .insert_project(&Project {
                        id,
                        name: format!("P {id}"),
                        path: None,
                        description: None,
                        color: Project::DEFAULT_COLOR.into(),
                        context_files: None,
                        created_at: now,
                        updated_at: now,
                    })
                    .unwrap();
            }
            for (id, project_id) in [
                (pm, project),
                (reporter, project),
                (worker, project),
                (stranger, project),
                (foreign, other_project),
                (foreign_worker, project),
                (successor, project),
            ] {
                let mut row = test_session(id, std::path::PathBuf::from("/tmp"));
                row.project_id = Some(project_id);
                row.status = SessionStatus::Running;
                guard.insert_session(&row).unwrap();
            }
            let issue = file_issue(&guard, reporter);
            bind(&guard, issue, worker);
            // An Issue filed from another project's session (#173: the
            // creator must be in the binding's project).
            let foreign_issue = file_issue(&guard, foreign);
            bind(&guard, foreign_issue, foreign_worker);
            issue
        };
        for (session, content) in [(reporter, "reporter tail"), (foreign, "foreign tail")] {
            event(
                &store,
                session,
                1,
                EventType::Message,
                Some(Role::Assistant),
                content,
            )
            .await;
        }

        let page = control
            .agent_read_session_events(worker, req(reporter))
            .await
            .unwrap();
        assert_eq!(page.session_id, reporter);
        assert_eq!(page.events[0].content, "reporter tail");
        assert_eq!(page.final_message.unwrap().content, "reporter tail");

        let denied = |result: Result<String>, label: &str| {
            let error = result.unwrap_err().to_string();
            assert!(
                error.contains(AGENT_READ_EVENTS_SCOPE_DENIED),
                "{label}: {error}"
            );
        };
        for (caller, target, label) in [
            (worker, stranger, "non-creator"),
            (worker, Uuid::new_v4(), "unknown session"),
            (stranger, reporter, "unbound caller"),
            (foreign_worker, foreign, "creator in another project"),
        ] {
            denied(
                control
                    .agent_read_session_events(caller, req(target))
                    .await
                    .map(|page| page.session_id.to_string()),
                label,
            );
        }
        denied(
            control
                .agent_get_status(worker, reporter)
                .await
                .map(|session| session.id.to_string()),
            "another verb",
        );

        // A later launch of the same Issue supersedes the binding.
        bind(&*store.lock().await, issue, successor);
        denied(
            control
                .agent_read_session_events(worker, req(reporter))
                .await
                .map(|page| page.session_id.to_string()),
            "superseded binding",
        );
        control
            .agent_read_session_events(successor, req(reporter))
            .await
            .unwrap();
        // The binding ends with the worker session.
        store
            .lock()
            .await
            .conn
            .execute(
                "UPDATE sessions SET status='Completed' WHERE id=?1",
                [successor.to_string()],
            )
            .unwrap();
        denied(
            control
                .agent_read_session_events(successor, req(reporter))
                .await
                .map(|page| page.session_id.to_string()),
            "ended binding",
        );
    }
}
