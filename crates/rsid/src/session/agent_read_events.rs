//! `AgentReadSessionEvents` (#1041): a typed, scoped, byte-bounded read of a
//! session's conversation events.
//!
//! Scope is exactly `AgentGetStatus`' (`authorize_agent_target`): the caller's
//! own session, a direct child, a child of an Epic it leads, or a session in
//! the current manager's live scope. An unknown target and an out-of-scope
//! target produce the identical refusal, so a refusal never reveals whether a
//! session exists. Read-only: no writes, no journal rows.

use super::agent_verbs::AgentControlHandle;
use crate::error::{DaemonError, Result};
use rsi_common::agent_session_events::{
    AGENT_READ_EVENT_FIELD_MAX_CHARS, AGENT_READ_EVENTS_SCOPE_DENIED, AgentFinalMessageV1,
    AgentReadSessionEventsRequestV1, AgentReadSessionEventsResultV1, AgentSessionEventV1,
    FINAL_MESSAGE_FULL_MAX_CHARS, FINAL_MESSAGE_MAX_CHARS, clip_chars,
    is_provider_diagnostic_metadata,
};
use rsi_common::types::{ConversationEvent, SessionStatus};
use uuid::Uuid;

fn scope_denied(caller: Uuid, target: Uuid) -> DaemonError {
    DaemonError::InvalidParam(format!(
        "{AGENT_READ_EVENTS_SCOPE_DENIED}: session {caller} may not read events of {target}"
    ))
}

/// Clip a JSON value to the per-field cap; an oversized value is replaced by
/// the clipped text of its serialization.
fn clip_json(value: Option<Box<serde_json::Value>>) -> (Option<serde_json::Value>, bool) {
    let Some(value) = value else {
        return (None, false);
    };
    let text = value.to_string();
    let (clipped, was_clipped) = clip_chars(&text, AGENT_READ_EVENT_FIELD_MAX_CHARS);
    if was_clipped {
        (Some(serde_json::Value::String(clipped)), true)
    } else {
        (Some(*value), false)
    }
}

fn project_event(event: ConversationEvent) -> AgentSessionEventV1 {
    let (content, content_clipped) = clip_chars(&event.content, AGENT_READ_EVENT_FIELD_MAX_CHARS);
    let (tool_input, input_clipped) = clip_json(event.tool_input);
    let provider_diagnostic = is_provider_diagnostic_metadata(event.metadata.as_deref());
    let (metadata, metadata_clipped) = clip_json(event.metadata);
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
            Err(DaemonError::SessionNotFound(_)) => {
                return Err(scope_denied(caller_session_id, target));
            }
            Err(DaemonError::InvalidParam(message))
                if message.starts_with(AGENT_READ_EVENTS_SCOPE_DENIED) =>
            {
                return Err(scope_denied(caller_session_id, target));
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

        let mut projected: Vec<AgentSessionEventV1> =
            events.into_iter().map(project_event).collect();
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
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::agent_verbs::tests::{control_handle_with_store, test_session};
    use super::*;
    use crate::store::Store;
    use chrono::Utc;
    use rsi_common::types::{EventType, Role, SessionKind};
    use std::sync::Arc;

    type SharedStore = Arc<tokio::sync::Mutex<Store>>;

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
}
