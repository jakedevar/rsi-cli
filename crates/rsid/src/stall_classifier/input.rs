//! Read-only `ClassificationInput` builder.
//!
//! Pure side-effect-free helper that snapshots the session state needed to
//! prompt the stall classifier. Designed to run on the classifier's
//! sibling task — it acquires the active read lock briefly, copies out
//! the per-session fields, then drops the lock before touching the store.
//!
//! The excerpt is formatted by `format_excerpt` which is also a pure fn so
//! its truncation + role-prefix behavior can be unit-tested without store
//! plumbing.

use crate::error::{DaemonError, Result};
use crate::session::types::TrackedSession;
use crate::store::Store;
use rsi_common::types::{ConversationEvent, EventType, Role, SessionStatus};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

use super::types::{ChildSummary, ClassificationInput};

/// Build a `ClassificationInput` for `session_id`. Read-only: acquires
/// the active read lock + the store mutex sequentially, copies out the
/// required fields, then returns. Caller is the classifier scheduler
/// (wired in Phase 4).
#[allow(dead_code)] // Phase 4 hooks this from the scheduler.
pub(crate) async fn build_classification_input(
    session_id: Uuid,
    active: &Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    store: &Arc<Mutex<Store>>,
    excerpt_event_limit: usize,
    excerpt_char_cap: usize,
) -> Result<ClassificationInput> {
    let snapshot = {
        let guard = active.read().await;
        let tracked = guard.get(&session_id).ok_or_else(|| {
            DaemonError::Process(format!(
                "Classifier input: session {session_id} not found in active map"
            ))
        })?;
        TrackedSnapshot {
            session_kind: tracked.session.session_kind,
            provider: tracked.session.provider,
            last_event_at: tracked.last_event_at,
            pending_question: tracked.pending_question_text(),
        }
    };

    let now = chrono::Utc::now();
    let idle_secs = now
        .signed_duration_since(snapshot.last_event_at)
        .num_seconds()
        .max(0) as u64;

    let (events, children) = {
        let store_guard = store.lock().await;
        let events = store_guard.load_events(session_id)?;
        let children = store_guard.list_children(Some(session_id))?;
        (events, children)
    };

    let excerpt = format_excerpt(&events, excerpt_event_limit, excerpt_char_cap);

    let child_summaries: Vec<ChildSummary> = children
        .into_iter()
        .map(|c| ChildSummary {
            session_id: c.id,
            kind: c.session_kind,
            status: c.status,
            // Children outside `active` lack a `last_event_at` (daemon-internal);
            // `updated_at` is the closest proxy on the persisted Session row.
            idle_secs: now.signed_duration_since(c.updated_at).num_seconds().max(0) as u64,
        })
        .collect();

    Ok(ClassificationInput {
        session_id,
        session_kind: snapshot.session_kind,
        provider: snapshot.provider,
        idle_secs,
        pending_question: snapshot.pending_question,
        excerpt,
        children: child_summaries,
    })
}

/// Lightweight bag of the fields copied out from `TrackedSession`. Held
/// across the store-lock acquisition so the active read lock can drop
/// first.
#[allow(dead_code)] // referenced only from `build_classification_input` (Phase 4 caller).
struct TrackedSnapshot {
    session_kind: rsi_common::types::SessionKind,
    provider: rsi_common::types::SessionProvider,
    last_event_at: chrono::DateTime<chrono::Utc>,
    pending_question: Option<String>,
}

/// Pure formatter: take the last `limit` Message + ToolUse events from
/// `events`, prefix with the role tag (`USER:` / `ASSISTANT:` /
/// `TOOL_USE:` / `TOOL_RESULT:`), and truncate each block to
/// `excerpt_char_cap` chars. Blocks are joined by `\n---\n` so the model
/// can parse boundaries unambiguously.
///
/// `events` is assumed to be in ascending-sequence order (what
/// `Store::load_events` produces). The trailing `limit` events are kept.
#[allow(dead_code)] // Phase 4 calls this via `build_classification_input`.
pub(crate) fn format_excerpt(
    events: &[ConversationEvent],
    limit: usize,
    excerpt_char_cap: usize,
) -> String {
    let mut filtered: Vec<&ConversationEvent> = events
        .iter()
        .filter(|e| {
            matches!(
                e.event_type,
                EventType::Message | EventType::ToolUse | EventType::ToolResult
            )
        })
        .collect();
    if filtered.len() > limit {
        let drop_n = filtered.len() - limit;
        filtered.drain(..drop_n);
    }

    if filtered.is_empty() {
        return String::new();
    }

    let mut blocks: Vec<String> = Vec::with_capacity(filtered.len());
    for e in filtered {
        let role_tag = match e.event_type {
            EventType::Message => match e.role {
                Some(Role::User) => "USER",
                Some(Role::Assistant) => "ASSISTANT",
                _ => "MESSAGE",
            },
            EventType::ToolUse => "TOOL_USE",
            EventType::ToolResult => "TOOL_RESULT",
            _ => continue, // filtered above; defensive
        };

        let mut content = e.content.replace('\r', " ");
        if content.len() > excerpt_char_cap {
            // Truncate on a UTF-8 boundary to avoid panics on multibyte chars.
            let mut end = excerpt_char_cap;
            while end > 0 && !content.is_char_boundary(end) {
                end -= 1;
            }
            content.truncate(end);
            content.push('…');
        }
        let tool = e
            .tool_name
            .as_deref()
            .map(|t| format!(" tool={}", t))
            .unwrap_or_default();
        blocks.push(format!("{role_tag}{tool}: {content}"));
    }
    blocks.join("\n---\n")
}

/// Children summary helper, factored out so callers writing their own
/// child arrays (tests) can reuse the idle-secs formula.
#[allow(dead_code)]
pub(crate) fn child_idle_secs(
    now: chrono::DateTime<chrono::Utc>,
    child_updated_at: chrono::DateTime<chrono::Utc>,
) -> u64 {
    now.signed_duration_since(child_updated_at)
        .num_seconds()
        .max(0) as u64
}

/// Marker to discourage callers from misinterpreting a `Completed` child
/// idle time as evidence of stalling — only Running / WaitingApproval
/// children count toward "team stuck" reasoning. Currently unused by the
/// builder (the model decides), kept for documentation.
#[allow(dead_code)]
pub(crate) const ACTIVE_CHILD_STATUSES: &[SessionStatus] =
    &[SessionStatus::Running, SessionStatus::WaitingApproval];

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::types::{Role, SessionKind, SessionProvider, SessionStatus};

    fn ev(seq: i32, et: EventType, role: Option<Role>, content: &str) -> ConversationEvent {
        ConversationEvent {
            id: seq as i64,
            session_id: Uuid::nil(),
            sequence: seq,
            event_type: et,
            role,
            created_at: chrono::Utc::now(),
            content: content.to_string(),
            tool_name: None,
            tool_input: None,
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        }
    }

    #[test]
    fn format_excerpt_returns_empty_for_no_events() {
        assert_eq!(format_excerpt(&[], 5, 100), "");
    }

    #[test]
    fn format_excerpt_drops_system_and_thinking_events() {
        let events = vec![
            ev(1, EventType::System, None, "boot"),
            ev(2, EventType::Thinking, Some(Role::Assistant), "deep"),
            ev(3, EventType::Message, Some(Role::User), "ping"),
        ];
        let s = format_excerpt(&events, 10, 100);
        assert!(s.contains("USER: ping"));
        assert!(!s.contains("boot"));
        assert!(!s.contains("deep"));
    }

    #[test]
    fn format_excerpt_uses_role_prefix() {
        let events = vec![
            ev(1, EventType::Message, Some(Role::User), "hi"),
            ev(2, EventType::Message, Some(Role::Assistant), "hello"),
            ev(3, EventType::ToolUse, Some(Role::Assistant), "{cmd}"),
            ev(4, EventType::ToolResult, None, "ok"),
        ];
        let s = format_excerpt(&events, 10, 200);
        assert!(s.contains("USER: hi"));
        assert!(s.contains("ASSISTANT: hello"));
        assert!(s.contains("TOOL_USE: {cmd}"));
        assert!(s.contains("TOOL_RESULT: ok"));
        assert!(s.contains("\n---\n"));
    }

    #[test]
    fn format_excerpt_truncates_long_content_with_ellipsis() {
        let long = "x".repeat(800);
        let events = vec![ev(1, EventType::Message, Some(Role::User), &long)];
        let s = format_excerpt(&events, 10, 100);
        // 100 char body + 'USER: ' prefix + trailing '…'.
        assert!(s.starts_with("USER: "));
        assert!(s.ends_with('…'));
        assert!(s.len() < long.len());
    }

    #[test]
    fn format_excerpt_keeps_only_last_limit_events() {
        let events = (0..10)
            .map(|i| ev(i, EventType::Message, Some(Role::User), &format!("m{i}")))
            .collect::<Vec<_>>();
        let s = format_excerpt(&events, 3, 50);
        // Should contain the last three (m7, m8, m9) and not m0..m6.
        assert!(s.contains("m7"));
        assert!(s.contains("m8"));
        assert!(s.contains("m9"));
        assert!(!s.contains("m6"));
    }

    #[test]
    fn format_excerpt_truncate_respects_utf8_boundaries() {
        let multi = "é".repeat(200); // each 'é' is 2 bytes
        let events = vec![ev(1, EventType::Message, Some(Role::User), &multi)];
        let s = format_excerpt(&events, 10, 51); // odd cap forces snap-back
        // Successful execution + ends-with-ellipsis is the assertion. No
        // panic means truncate found a valid char boundary.
        assert!(s.ends_with('…'));
    }

    #[test]
    fn child_idle_secs_zeroes_when_future_timestamp() {
        let now = chrono::Utc::now();
        let future = now + chrono::Duration::seconds(10);
        assert_eq!(child_idle_secs(now, future), 0);
    }

    #[test]
    fn child_idle_secs_computes_diff_in_seconds() {
        let now = chrono::Utc::now();
        let past = now - chrono::Duration::seconds(42);
        assert_eq!(child_idle_secs(now, past), 42);
    }

    #[test]
    fn active_child_statuses_excludes_terminal_states() {
        for s in [
            SessionStatus::Completed,
            SessionStatus::Failed,
            SessionStatus::Interrupted,
            SessionStatus::Archived,
        ] {
            assert!(!ACTIVE_CHILD_STATUSES.contains(&s));
        }
    }

    // --- ClassificationInput plumbing smoke tests ---
    // Full integration test uses an in-memory Store with seeded events +
    // children; lives in `tests/` once Phase 4's scheduler hooks the
    // pipeline end-to-end. Here we exercise the helpers in isolation.

    #[test]
    fn classification_input_fields_assemble() {
        let id = Uuid::new_v4();
        let inp = ClassificationInput {
            session_id: id,
            session_kind: SessionKind::Standard,
            provider: SessionProvider::Claude,
            idle_secs: 700,
            pending_question: Some("auth method".to_string()),
            excerpt: "USER: hi\n---\nASSISTANT: hello".to_string(),
            children: vec![ChildSummary {
                session_id: Uuid::new_v4(),
                kind: SessionKind::Task,
                status: SessionStatus::Running,
                idle_secs: 60,
            }],
        };
        assert_eq!(inp.session_id, id);
        assert_eq!(inp.children.len(), 1);
    }

    // --- Plumbing integration tests for build_classification_input ---

    use crate::store::Store;
    use rsi_common::types::Session;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn seeded_session(id: Uuid, kind: SessionKind, parent: Option<Uuid>) -> Session {
        // Mirror the field laundry from `store_worker::tests::test_session`
        // so the row passes `Store::insert_session` SQL validation.
        Session {
            context_fill_pct: None,
            id,
            provider: SessionProvider::Claude,
            claude_session_id: None,
            query: format!("test-{id}"),
            title: None,
            agent_role: None,
            epic_spawn_ordinal: None,
            description: None,
            short_summary: None,
            pending_question: None,
            pending_archive: false,
            working_dir: PathBuf::from("/tmp"),
            git_branch: None,
            status: SessionStatus::Running,
            project_id: None,
            pinned_at: None,
            testing_needed_at: None,
            rotation_disabled_at: None,
            session_kind: kind,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now() - chrono::Duration::seconds(30),
            cost_usd: None,
            duration_ms: None,
            num_turns: None,
            model: None,
            input_tokens: None,
            output_tokens: None,
            context_window: None,
            resolved_context_budget: None,
            total_input_tokens: None,
            total_output_tokens: None,
            total_cache_creation_tokens: None,
            total_cache_read_tokens: None,
            stop_reason: None,
            continued_from: None,
            context_usage_confidence: rsi_common::types::ContextUsageConfidence::Missing,
            daemon_input_tokens: None,
            daemon_output_tokens: None,
            handoff_filepath: None,
            active_task: None,
            group_id: None,
            pipeline_artifact: None,
            workflow_id: None,
            workflow_id_override: None,
            rotation_depth: 0,
            retry_attempt: None,
            max_retries: None,
            effort: None,
            issue_identifier: None,
            issue_url: None,
            issue_tracker_id: None,
            scheduled_job_id: None,
            rating: None,
            harness_version_hash: None,
            test_passed: None,
            clippy_passed: None,
            turn_count: None,
            retry_count: None,
            approval_wait_ms: None,
            work_time_ms: None,
            approval_started_at: None,
            sandbox_kind: None,
            sandbox_root: None,
            sandbox_branch: None,
            sandbox_cleanup_state: None,
            tag: String::new(),
            tags: Vec::new(),
            parent_id: parent,
            lead_session_id: None,
            is_eval: false,
            capability_class: None,
            topology_node_id: None,
            topology_iteration: 0,
            provider_cli_version: None,
            provider_capabilities: Vec::new(),
            thinking_tokens: None,
            service_tier: None,
            cache_creation_1h_tokens: None,
            cache_creation_5m_tokens: None,
            permission_denial_count: None,
            subagent_stats_json: None,
            queued_turn_count: None,
            terminal_reason: None,
        }
    }

    fn db_event(
        session_id: Uuid,
        seq: i32,
        et: EventType,
        role: Option<Role>,
        content: &str,
    ) -> ConversationEvent {
        ConversationEvent {
            id: 0,
            session_id,
            sequence: seq,
            event_type: et,
            role,
            created_at: chrono::Utc::now(),
            content: content.to_string(),
            tool_name: None,
            tool_input: None,
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        }
    }

    #[tokio::test]
    async fn build_classification_input_happy_path() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let parent_id = Uuid::new_v4();
        let parent = seeded_session(parent_id, SessionKind::Standard, None);
        store.insert_session(&parent).unwrap();
        store
            .insert_event(&db_event(
                parent_id,
                1,
                EventType::Message,
                Some(Role::User),
                "go",
            ))
            .unwrap();
        store
            .insert_event(&db_event(
                parent_id,
                2,
                EventType::Message,
                Some(Role::Assistant),
                "starting",
            ))
            .unwrap();

        let store = Arc::new(Mutex::new(store));
        let mut active = HashMap::new();
        let tracked = TrackedSession::new_for_test(parent.clone());
        active.insert(parent_id, tracked);
        let active = Arc::new(RwLock::new(active));

        let out = build_classification_input(parent_id, &active, &store, 10, 200)
            .await
            .unwrap();
        assert_eq!(out.session_id, parent_id);
        assert_eq!(out.session_kind, SessionKind::Standard);
        assert_eq!(out.provider, SessionProvider::Claude);
        assert!(out.pending_question.is_none());
        assert!(out.excerpt.contains("USER: go"));
        assert!(out.excerpt.contains("ASSISTANT: starting"));
        assert!(out.children.is_empty());
    }

    #[tokio::test]
    async fn build_classification_input_with_children() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let parent_id = Uuid::new_v4();
        let parent = seeded_session(parent_id, SessionKind::Standard, None);
        store.insert_session(&parent).unwrap();

        let child_a = seeded_session(Uuid::new_v4(), SessionKind::Task, Some(parent_id));
        let child_b = seeded_session(Uuid::new_v4(), SessionKind::Task, Some(parent_id));
        store.insert_session(&child_a).unwrap();
        store.insert_session(&child_b).unwrap();

        let store = Arc::new(Mutex::new(store));
        let mut active = HashMap::new();
        active.insert(parent_id, TrackedSession::new_for_test(parent.clone()));
        let active = Arc::new(RwLock::new(active));

        let out = build_classification_input(parent_id, &active, &store, 10, 200)
            .await
            .unwrap();
        assert_eq!(out.children.len(), 2);
        for c in &out.children {
            assert_eq!(c.kind, SessionKind::Task);
        }
    }

    #[tokio::test]
    async fn build_classification_input_missing_session_errors() {
        let store = Arc::new(Mutex::new(
            Store::open(std::path::Path::new(":memory:")).unwrap(),
        ));
        let active: Arc<RwLock<HashMap<Uuid, TrackedSession>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let r = build_classification_input(Uuid::new_v4(), &active, &store, 10, 200).await;
        assert!(r.is_err());
    }
}
