//! Operator-local reads and the one write behind RSI Remote API v2 (#2962):
//! the manager seats of exposed projects, a seat's conversation newest-first
//! and a message to a seat.
//!
//! Like the other Remote methods these are tokenless operator calls on the
//! daemon socket; the `rsi-remote` gateway authorizes the device, session,
//! project scope and the `allow_messages` switch before it calls them. The
//! daemon does not trust the gateway's scope claim for the seat: every call
//! proves the named session is the project's current manager seat, or the
//! portfolio seat covering it, from the Store.

use super::{ReadError, RemoteReadCompleted, RemoteReadLimiter, Result};
use crate::error::DaemonError;
use crate::store::Store;
use rsi_common::harness_manager_v2::{
    AgentManagerInspectRequestV2, MANAGER_V2_MAX_PAGE, ManagerInspectSectionV2,
};
use rsi_common::remote_manager_v2::{
    CONVERSATION_PAGE_LIMIT, CONVERSATION_TEXT_BYTES, ConversationItemV2, ConversationRequestV2,
    ConversationResponseV2, IDEMPOTENCY_PREFIX, ListManagerSeatsRequestV2,
    ListManagerSeatsResponseV2, ManagerSeatV2, ProjectSeatsV2, RECENT_OPERATOR_MESSAGES,
    REFUSAL_IDEMPOTENCY_CONFLICT, REFUSAL_INVALID_MESSAGE, REFUSAL_SEAT_NOT_LIVE,
    REFUSAL_SEAT_OUT_OF_SCOPE, SeatRoleV2, SendMessageReceiptV2, SendMessageRequestV2, SpeakerV2,
    valid_message_text,
};
use rsi_common::rpc::RpcRequest;
use std::collections::BTreeSet;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use uuid::Uuid;

/// Decision-ledger pages scanned for one seat's pending count.
const MAX_DECISION_PAGES: usize = 4;

fn strict<T: serde::de::DeserializeOwned>(rpc: &RpcRequest) -> Result<T> {
    if rpc.session_token.is_some() || rpc.jsonrpc != "2.0" {
        return Err(ReadError::InvalidSource);
    }
    serde_json::from_value(rpc.params.clone()).map_err(|_| ReadError::InvalidSource)
}

pub(super) fn parse_list(rpc: &RpcRequest) -> Result<ListManagerSeatsRequestV2> {
    let request: ListManagerSeatsRequestV2 = strict(rpc)?;
    if request.project_ids.is_empty() || request.project_ids.len() > 32 {
        return Err(ReadError::InvalidSource);
    }
    Ok(request)
}

pub(super) fn parse_conversation(rpc: &RpcRequest) -> Result<ConversationRequestV2> {
    let request: ConversationRequestV2 = strict(rpc)?;
    if request.limit == 0 || request.limit > CONVERSATION_PAGE_LIMIT {
        return Err(ReadError::InvalidSource);
    }
    if request.before.is_some_and(|anchor| anchor.id <= 0) {
        return Err(ReadError::InvalidSource);
    }
    Ok(request)
}

pub(super) fn spawn_list_seats_read(
    limiter: &RemoteReadLimiter,
    store: Arc<Mutex<Store>>,
    request: ListManagerSeatsRequestV2,
) -> Result<JoinHandle<RemoteReadCompleted<ListManagerSeatsResponseV2>>> {
    limiter.spawn(move |budget| {
        budget.check()?;
        let guard = super::permit::acquire_store(&store, budget)?;
        let response = list_seats(&guard, &request, &|| budget.check())?;
        drop(guard);
        budget.check()?;
        Ok(response)
    })
}

pub(super) fn spawn_conversation_read(
    limiter: &RemoteReadLimiter,
    store: Arc<Mutex<Store>>,
    request: ConversationRequestV2,
) -> Result<JoinHandle<RemoteReadCompleted<ConversationResponseV2>>> {
    limiter.spawn(move |budget| {
        budget.check()?;
        let guard = super::permit::acquire_store(&store, budget)?;
        let response = conversation(&guard, &request)?;
        drop(guard);
        budget.check()?;
        Ok(response)
    })
}

fn source<T>(result: crate::error::Result<T>) -> Result<T> {
    result.map_err(|_| ReadError::SourceUnavailable)
}

/// Waiting question/approval decisions in the project's manager ledger, and
/// whether the scan stopped at its page cap.
fn pending_decisions(store: &Store, project: Uuid) -> (u32, bool) {
    let mut count = 0u32;
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_DECISION_PAGES {
        let Ok(page) = store.manager_v2_inspect_operator(
            project,
            &AgentManagerInspectRequestV2 {
                include_history: false,
                section: ManagerInspectSectionV2::Decisions,
                epic_id: None,
                cursor: cursor.clone(),
                limit: MANAGER_V2_MAX_PAGE,
                project_id: None,
            },
        ) else {
            return (count, false);
        };
        for row in &page.rows {
            let key = row["key"].as_str().unwrap_or_default();
            if (key.starts_with("question:") || key.starts_with("approval:"))
                && row["status"].as_str() == Some("pending")
            {
                count = count.saturating_add(1);
            }
        }
        if page.complete && page.next_cursor.is_none() {
            return (count, false);
        }
        match page.next_cursor {
            Some(next) if Some(&next) != cursor.as_ref() => cursor = Some(next),
            _ => return (count, false),
        }
    }
    (count, true)
}

fn seat_summary(
    store: &Store,
    project: Uuid,
    role: SeatRoleV2,
    session: Uuid,
) -> Result<Option<ManagerSeatV2>> {
    let Some(session) = source(store.get_session(session))? else {
        return Ok(None);
    };
    let (pending, truncated) = match role {
        SeatRoleV2::ProjectManager => {
            let (count, truncated) = pending_decisions(store, project);
            (Some(count), truncated)
        }
        SeatRoleV2::GlobalManager => (None, false),
    };
    let status = serde_json::to_value(session.status)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default();
    let provider = serde_json::to_value(session.provider)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default();
    Ok(Some(ManagerSeatV2 {
        session_id: session.id,
        role,
        status,
        provider,
        model: session.model.clone(),
        last_activity_at: session
            .updated_at
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
        pending_decisions: pending,
        pending_decisions_truncated: truncated,
    }))
}

/// Every configured project that exists, in id order, with its live manager
/// seats. A project without a manager or a covering global seat reports
/// `null` for it.
pub(crate) fn list_seats(
    store: &Store,
    request: &ListManagerSeatsRequestV2,
    check: &dyn Fn() -> Result<()>,
) -> Result<ListManagerSeatsResponseV2> {
    let ids: BTreeSet<Uuid> = request.project_ids.iter().copied().collect();
    let mut projects = Vec::with_capacity(ids.len());
    for project_id in ids {
        // The Store lock is held for the whole walk: stop at the read deadline
        // (the phone retries) instead of holding it for 32 slow projects.
        check()?;
        let Some(project) = source(store.get_project(project_id))? else {
            continue;
        };
        let project_manager = match source(store.remote_project_manager_seat(project_id))? {
            Some(seat) => seat_summary(store, project_id, SeatRoleV2::ProjectManager, seat)?,
            None => None,
        };
        let global_manager = match source(store.remote_global_manager_seat(project_id))? {
            Some(seat) => seat_summary(store, project_id, SeatRoleV2::GlobalManager, seat)?,
            None => None,
        };
        projects.push(ProjectSeatsV2 {
            project_id,
            name: project.name.chars().take(512).collect(),
            project_manager,
            global_manager,
        });
    }
    Ok(ListManagerSeatsResponseV2 { projects })
}

/// `text` cut to [`CONVERSATION_TEXT_BYTES`] at a char boundary; `cut` is true
/// when the store already dropped the tail of a longer message.
fn bounded(text: &str, cut: bool) -> (String, bool) {
    if text.len() <= CONVERSATION_TEXT_BYTES {
        return (text.to_owned(), cut);
    }
    let mut end = CONVERSATION_TEXT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), true)
}

/// One newest-first page of a seat's conversation. The seat must be the
/// project's current manager seat or the covering global seat, else
/// `NotFound`.
pub(crate) fn conversation(
    store: &Store,
    request: &ConversationRequestV2,
) -> Result<ConversationResponseV2> {
    let role = source(store.remote_manager_seat_role(request.project_id, request.seat_session_id))?
        .ok_or(ReadError::NotFound)?;
    let seat = seat_summary(store, request.project_id, role, request.seat_session_id)?
        .ok_or(ReadError::NotFound)?;
    let (rows, has_more) = source(store.remote_manager_events_before(
        request.seat_session_id,
        request.before.map(|anchor| (anchor.sequence, anchor.id)),
        request.limit,
    ))?;
    let next_before = if has_more {
        rows.last()
            .map(|row| format!("{}:{}", row.sequence, row.id))
    } else {
        None
    };
    let items = rows
        .into_iter()
        .map(|row| {
            let (text, truncated) = bounded(&row.content, row.content_cut);
            ConversationItemV2 {
                id: row.id.to_string(),
                sequence: row.sequence,
                speaker: if row.operator {
                    SpeakerV2::Operator
                } else if row.assistant {
                    SpeakerV2::Manager
                } else {
                    SpeakerV2::System
                },
                text,
                truncated,
                created_at: row.created_at,
                operator_message_id: row.operator_message_id,
            }
        })
        .collect();
    let operator_messages = if request.before.is_none() {
        source(
            store.recent_operator_message_states(request.seat_session_id, RECENT_OPERATOR_MESSAGES),
        )?
    } else {
        Vec::new()
    };
    Ok(ConversationResponseV2 {
        seat,
        items,
        next_before,
        has_more,
        operator_messages,
    })
}

fn refusal(code: &str) -> DaemonError {
    DaemonError::PolicyDenied(code.to_owned())
}

/// Queue the operator message for a seat exactly as the TUI does
/// (`Store::queue_operator_message`: durable `queued` row, delivered at the
/// seat's next tool boundary, at most once). The key is namespaced so a
/// gateway retry replays the stored row and a different text under the same
/// key is refused.
///
/// # Errors
///
/// A [`DaemonError::PolicyDenied`] whose message is one of the
/// `REFUSAL_*` codes when the request is invalid, the session is not a
/// manager seat of the project, or the key was used for other text.
pub(crate) fn send_message(
    store: &mut Store,
    request: &SendMessageRequestV2,
) -> crate::error::Result<SendMessageReceiptV2> {
    if !valid_message_text(&request.text) {
        return Err(refusal(REFUSAL_INVALID_MESSAGE));
    }
    if store
        .remote_manager_seat_role(request.project_id, request.seat_session_id)?
        .is_none()
    {
        return Err(refusal(REFUSAL_SEAT_OUT_OF_SCOPE));
    }
    let key = format!("{IDEMPOTENCY_PREFIX}{}", request.idempotency_key);
    let started = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let message = store
        .queue_operator_message(request.seat_session_id, &request.text, &key)
        .map_err(|error| match error {
            DaemonError::InvalidParam(text) if text.contains("reused with different content") => {
                refusal(REFUSAL_IDEMPOTENCY_CONFLICT)
            }
            DaemonError::InvalidParam(_) => refusal(REFUSAL_SEAT_NOT_LIVE),
            other => other,
        })?;
    let value = serde_json::to_value(&message)?;
    let created_at = value["created_at"].as_str().unwrap_or_default().to_owned();
    Ok(SendMessageReceiptV2 {
        message_id: message.id,
        seat_session_id: request.seat_session_id,
        state: message.state,
        deduplicated: created_at.as_str() < started.as_str(),
        created_at,
    })
}

/// Parse the write's params: tokenless, JSON-RPC 2.0, strict fields.
pub(crate) fn parse_send(rpc: &RpcRequest) -> crate::error::Result<SendMessageRequestV2> {
    if rpc.session_token.is_some()
        || rpc.jsonrpc != "2.0"
        || serde_json::to_vec(&rpc.params)?.len() > 16 * 1024
    {
        return Err(DaemonError::InvalidParam("remote_message_invalid".into()));
    }
    serde_json::from_value(rpc.params.clone())
        .map_err(|_| DaemonError::InvalidParam("remote_message_invalid".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::remote_manager_v2::ConversationAnchorV2;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn bounded_text_cuts_at_a_char_boundary() {
        let (text, truncated) = bounded(&"é".repeat(CONVERSATION_TEXT_BYTES), false);
        assert!(truncated);
        assert!(text.len() <= CONVERSATION_TEXT_BYTES);
        assert!(text.chars().all(|c| c == 'é'));
        assert_eq!(bounded("short", false), ("short".to_owned(), false));
        assert_eq!(bounded("short", true), ("short".to_owned(), true));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn read_requests_are_strict() {
        let project = Uuid::new_v4();
        let seat = Uuid::new_v4();
        let ok = RpcRequest::new(
            "RemoteGetManagerConversationV2",
            serde_json::json!({"project_id": project, "seat_session_id": seat, "limit": 25}),
        );
        assert!(parse_conversation(&ok).is_ok());
        let mut tokened = ok.clone();
        tokened.session_token = Some("t".into());
        assert!(parse_conversation(&tokened).is_err());
        for params in [
            serde_json::json!({"project_id": project, "seat_session_id": seat, "limit": 0}),
            serde_json::json!({"project_id": project, "seat_session_id": seat, "limit": 26}),
            serde_json::json!({"project_id": project, "seat_session_id": seat, "limit": 5, "x": 1}),
            serde_json::json!({"project_id": project, "seat_session_id": seat, "limit": 5,
                "before": {"sequence": 3, "id": 0}}),
        ] {
            let request = RpcRequest::new("RemoteGetManagerConversationV2", params);
            assert!(parse_conversation(&request).is_err());
        }
        let anchored = ConversationAnchorV2 { sequence: 3, id: 9 };
        assert_eq!(anchored.id, 9);
        let empty = RpcRequest::new(
            "RemoteListManagerSeatsV2",
            serde_json::json!({"project_ids": []}),
        );
        assert!(parse_list(&empty).is_err());
    }
}

#[cfg(all(
    test,
    any(not(feature = "test-shard-mode"), feature = "test-shard-store-04")
))]
mod seat_tests {
    use super::*;
    use crate::session::agent_verbs::tests::test_session;
    use chrono::Utc;
    use rsi_common::global_manager::ConfigureGlobalManagerRequestV1;
    use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
    use rsi_common::harness_manager_v2::{
        ConfigureHarnessManagerPolicyRequestV2, ManagerLaunchChoiceV2, ManagerPolicyV2,
    };
    use rsi_common::remote_manager_v2::ConversationAnchorV2;
    use rsi_common::types::{
        ConversationEvent, EventType, Project, Role, Session, SessionKind, SessionProvider,
        SessionStatus,
    };
    use std::path::PathBuf;

    struct Fixture {
        project: Uuid,
        manager: Uuid,
    }

    fn insert_project(store: &Store, name: &str) -> Uuid {
        let stamp = Utc::now();
        let id = Uuid::new_v4();
        store
            .insert_project(&Project {
                id,
                name: name.into(),
                path: None,
                description: None,
                color: Project::DEFAULT_COLOR.into(),
                context_files: None,
                created_at: stamp,
                updated_at: stamp,
            })
            .unwrap();
        id
    }

    fn standard(store: &Store, project: Option<Uuid>, status: SessionStatus) -> Session {
        let mut session = test_session(Uuid::new_v4(), PathBuf::from("/var/tmp/remote-v2"));
        session.session_kind = SessionKind::Standard;
        session.project_id = project;
        session.status = status;
        session.provider = SessionProvider::Claude;
        store.insert_session(&session).unwrap();
        session
    }

    /// A project with an appointed, live manager seat (the same appointment
    /// the decision-targets fixture makes).
    fn project_with_manager(store: &Store, name: &str) -> Fixture {
        let project = insert_project(store, name);
        let manager = standard(store, Some(project), SessionStatus::Running);
        let mut group = manager.clone();
        group.id = Uuid::new_v4();
        group.session_kind = SessionKind::Group;
        store.insert_session(&group).unwrap();
        let mut epic = manager.clone();
        epic.id = Uuid::new_v4();
        epic.session_kind = SessionKind::Epic;
        epic.parent_id = Some(group.id);
        store.insert_session(&epic).unwrap();
        let config = store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                group_ids: Vec::new(),
                project_id: project,
                session_id: manager.id,
                epic_ids: Some(vec![epic.id]),
                expected_row_version: 0,
            })
            .unwrap();
        store
            .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                project_id: project,
                expected_scope_version: config.row_version,
                expected_policy_version: 0,
                idempotency_key: format!("remote-v2-policy-{project}"),
                policy: ManagerPolicyV2::default(),
            })
            .unwrap();
        Fixture {
            project,
            manager: manager.id,
        }
    }

    /// The portfolio ("global") seat covering `projects`.
    fn global_seat(store: &Store, projects: &[Uuid]) -> Uuid {
        let seat = standard(store, None, SessionStatus::Running);
        store
            .configure_global_manager(
                &ConfigureGlobalManagerRequestV1 {
                    session_id: seat.id,
                    project_ids: projects.to_vec(),
                    allowed_launches: vec![ManagerLaunchChoiceV2 {
                        provider: SessionProvider::Claude,
                        model: "claude-opus-5-5".into(),
                        effort: Some("high".into()),
                    }],
                    project_policy: ManagerPolicyV2::default(),
                    expected_grant_version: 0,
                    idempotency_key: format!("remote-v2-global-{}", seat.id),
                },
                "operator:test",
            )
            .unwrap();
        seat.id
    }

    fn message(
        store: &Store,
        session: Uuid,
        sequence: i32,
        role: Role,
        content: &str,
        metadata: Option<serde_json::Value>,
    ) -> i64 {
        store
            .insert_event(&ConversationEvent {
                id: 0,
                session_id: session,
                sequence,
                event_type: EventType::Message,
                role: Some(role),
                content: content.into(),
                tool_name: None,
                tool_input: None,
                created_at: Utc::now(),
                offload_id: None,
                tool_use_id: None,
                metadata: metadata.map(Box::new),
            })
            .unwrap()
    }

    fn list(store: &Store, projects: &[Uuid]) -> ListManagerSeatsResponseV2 {
        list_seats(
            store,
            &ListManagerSeatsRequestV2 {
                project_ids: projects.to_vec(),
            },
            &|| Ok(()),
        )
        .unwrap()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn project_list_reports_the_manager_and_the_covering_global_seat() {
        let store = Store::open_in_memory().unwrap();
        let alpha = project_with_manager(&store, "Alpha");
        let beta = project_with_manager(&store, "Beta");
        let unmanaged = insert_project(&store, "Gamma");

        // Without a global seat: the project manager only.
        let page = list(&store, &[alpha.project, beta.project, unmanaged]);
        assert_eq!(page.projects.len(), 3);
        let alpha_row = page
            .projects
            .iter()
            .find(|row| row.project_id == alpha.project)
            .unwrap();
        let seat = alpha_row.project_manager.as_ref().unwrap();
        assert_eq!(seat.session_id, alpha.manager);
        assert_eq!(seat.role, SeatRoleV2::ProjectManager);
        assert_eq!(seat.status, "Running");
        assert_eq!(seat.provider, "Claude");
        assert_eq!(seat.pending_decisions, Some(0));
        assert!(alpha_row.global_manager.is_none());
        let gamma = page
            .projects
            .iter()
            .find(|row| row.project_id == unmanaged)
            .unwrap();
        assert!(gamma.project_manager.is_none() && gamma.global_manager.is_none());

        // With a global seat covering alpha only.
        let global = global_seat(&store, &[alpha.project]);
        let page = list(&store, &[alpha.project, beta.project]);
        let alpha_row = page
            .projects
            .iter()
            .find(|row| row.project_id == alpha.project)
            .unwrap();
        let covering = alpha_row.global_manager.as_ref().unwrap();
        assert_eq!(covering.session_id, global);
        assert_eq!(covering.role, SeatRoleV2::GlobalManager);
        assert_eq!(covering.pending_decisions, None);
        let beta_row = page
            .projects
            .iter()
            .find(|row| row.project_id == beta.project)
            .unwrap();
        assert!(beta_row.global_manager.is_none(), "beta is not covered");

        // A configured id with no project row is absent, not an error.
        assert!(list(&store, &[Uuid::new_v4()]).projects.is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn conversation_pages_newest_first_and_the_cursor_walks_every_item_once() {
        let store = Store::open_in_memory().unwrap();
        let fixture = project_with_manager(&store, "Alpha");
        let seat = fixture.manager;
        let operator_id = Uuid::new_v4();
        for n in 1..=7 {
            let (role, content, metadata) = match n {
                2 => (
                    Role::User,
                    "please prioritize the migration".to_owned(),
                    Some(serde_json::json!({
                        "source": rsi_common::types::OPERATOR_EVENT_SOURCE,
                        "operator_message_id": operator_id,
                        "delivery": "tool_boundary",
                    })),
                ),
                3 => (Role::User, "wake prompt".to_owned(), None),
                n => (Role::Assistant, format!("manager {n}"), None),
            };
            message(&store, seat, n, role, &content, metadata);
        }
        // A provider diagnostic is not the manager's words.
        message(
            &store,
            seat,
            8,
            Role::Assistant,
            "provider hiccup",
            Some(serde_json::json!({"provider_diagnostic": 1})),
        );

        let request = |before: Option<ConversationAnchorV2>, limit| ConversationRequestV2 {
            project_id: fixture.project,
            seat_session_id: seat,
            before,
            limit,
        };
        let first = conversation(&store, &request(None, 3)).unwrap();
        assert_eq!(first.seat.session_id, seat);
        let sequences: Vec<_> = first.items.iter().map(|item| item.sequence).collect();
        assert_eq!(sequences, vec![7, 6, 5], "newest first, diagnostic hidden");
        assert!(first.has_more);
        assert!(
            first
                .next_before
                .as_deref()
                .is_some_and(|c| c.starts_with("5:"))
        );

        let mut seen = sequences;
        let mut next = first.next_before;
        let mut pages = 1;
        let mut items = first.items;
        while let Some(token) = next {
            let (sequence, id) = token.split_once(':').unwrap();
            let page = conversation(
                &store,
                &request(
                    Some(ConversationAnchorV2 {
                        sequence: sequence.parse().unwrap(),
                        id: id.parse().unwrap(),
                    }),
                    3,
                ),
            )
            .unwrap();
            assert!(page.operator_messages.is_empty(), "older pages carry none");
            seen.extend(page.items.iter().map(|item| item.sequence));
            items.extend(page.items);
            next = page.next_before;
            pages += 1;
            assert!(pages <= 4);
        }
        assert_eq!(seen, vec![7, 6, 5, 4, 3, 2, 1], "each item exactly once");
        assert_eq!(pages, 3);

        let speaker = |sequence: i32| {
            items
                .iter()
                .find(|item| item.sequence == sequence)
                .unwrap()
                .speaker
        };
        assert_eq!(speaker(2), SpeakerV2::Operator);
        assert_eq!(speaker(3), SpeakerV2::System);
        assert_eq!(speaker(4), SpeakerV2::Manager);
        assert_eq!(
            items
                .iter()
                .find(|item| item.sequence == 2)
                .unwrap()
                .operator_message_id,
            Some(operator_id)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn conversation_of_a_session_that_is_not_a_seat_of_the_project_is_not_found() {
        let store = Store::open_in_memory().unwrap();
        let alpha = project_with_manager(&store, "Alpha");
        let beta = project_with_manager(&store, "Beta");
        let worker = standard(&store, Some(alpha.project), SessionStatus::Running);
        message(
            &store,
            worker.id,
            1,
            Role::Assistant,
            "secret worker output",
            None,
        );
        message(&store, beta.manager, 1, Role::Assistant, "beta only", None);
        for (project, session) in [
            (alpha.project, worker.id),
            (alpha.project, beta.manager),
            (beta.project, alpha.manager),
            (alpha.project, Uuid::new_v4()),
        ] {
            let error = conversation(
                &store,
                &ConversationRequestV2 {
                    project_id: project,
                    seat_session_id: session,
                    before: None,
                    limit: 5,
                },
            )
            .unwrap_err();
            assert!(matches!(error, ReadError::NotFound));
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_huge_message_is_cut_and_flagged_and_the_walk_stops_at_the_deadline() {
        let store = Store::open_in_memory().unwrap();
        let fixture = project_with_manager(&store, "Alpha");
        message(
            &store,
            fixture.manager,
            1,
            Role::Assistant,
            &"x".repeat(3 * CONVERSATION_TEXT_BYTES),
            None,
        );
        message(&store, fixture.manager, 2, Role::Assistant, "short", None);
        let page = conversation(
            &store,
            &ConversationRequestV2 {
                project_id: fixture.project,
                seat_session_id: fixture.manager,
                before: None,
                limit: 5,
            },
        )
        .unwrap();
        assert!(!page.items[0].truncated);
        assert!(page.items[1].truncated);
        assert_eq!(page.items[1].text.len(), CONVERSATION_TEXT_BYTES);

        // A read past its deadline stops instead of holding the Store lock.
        let error = list_seats(
            &store,
            &ListManagerSeatsRequestV2 {
                project_ids: vec![fixture.project],
            },
            &|| Err(ReadError::Busy),
        )
        .unwrap_err();
        assert!(matches!(error, ReadError::Busy));
    }

    fn send(
        store: &mut Store,
        project: Uuid,
        seat: Uuid,
        text: &str,
        key: Uuid,
    ) -> crate::error::Result<SendMessageReceiptV2> {
        send_message(
            store,
            &SendMessageRequestV2 {
                project_id: project,
                seat_session_id: seat,
                text: text.into(),
                idempotency_key: key,
            },
        )
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_message_is_queued_through_queue_operator_message_and_replays_by_key() {
        let mut store = Store::open_in_memory().unwrap();
        let fixture = project_with_manager(&store, "Alpha");
        let key = Uuid::new_v4();
        let first = send(
            &mut store,
            fixture.project,
            fixture.manager,
            "focus on the gate",
            key,
        )
        .unwrap();
        assert_eq!(first.state, "queued");
        assert!(!first.deduplicated);
        assert_eq!(first.seat_session_id, fixture.manager);

        // The durable row is exactly what the TUI's QueueOperatorMessage makes.
        let rows = store.list_operator_messages(fixture.manager).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, first.message_id);
        assert_eq!(rows[0].content, "focus on the gate");
        assert_eq!(rows[0].state, "queued");

        // The same key and text replays the stored row.
        let replay = send(
            &mut store,
            fixture.project,
            fixture.manager,
            "focus on the gate",
            key,
        )
        .unwrap();
        assert_eq!(replay.message_id, first.message_id);
        assert!(replay.deduplicated);
        assert_eq!(
            store.list_operator_messages(fixture.manager).unwrap().len(),
            1
        );

        // Other text under the same key is refused and queues nothing.
        let error = send(
            &mut store,
            fixture.project,
            fixture.manager,
            "something else",
            key,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains(REFUSAL_IDEMPOTENCY_CONFLICT),
            "{error}"
        );
        assert_eq!(
            store.list_operator_messages(fixture.manager).unwrap().len(),
            1
        );

        // The delivery state is visible on the conversation's first page.
        let page = conversation(
            &store,
            &ConversationRequestV2 {
                project_id: fixture.project,
                seat_session_id: fixture.manager,
                before: None,
                limit: 5,
            },
        )
        .unwrap();
        assert_eq!(page.operator_messages.len(), 1);
        assert_eq!(page.operator_messages[0].message_id, first.message_id);
        assert_eq!(page.operator_messages[0].state, "queued");
        assert_eq!(page.operator_messages[0].text_len, 17);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_sent_message_is_delivered_at_the_boundary_and_reads_back_as_the_operators() {
        let mut store = Store::open_in_memory().unwrap();
        let fixture = project_with_manager(&store, "Alpha");
        let sent = send(
            &mut store,
            fixture.project,
            fixture.manager,
            "ship the migration",
            Uuid::new_v4(),
        )
        .unwrap();

        // The boundary delivery the TUI's messages take: claim, cross the
        // effect boundary, settle `delivered` with the operator-labelled
        // transcript event in one transaction.
        let claimed = store
            .claim_operator_message(fixture.manager)
            .unwrap()
            .unwrap();
        assert_eq!(claimed.id, sent.message_id);
        store
            .mark_operator_message_effect_possible(claimed.id)
            .unwrap();
        store
            .deliver_operator_message_with_transcript(claimed.id, None)
            .unwrap();

        let page = conversation(
            &store,
            &ConversationRequestV2 {
                project_id: fixture.project,
                seat_session_id: fixture.manager,
                before: None,
                limit: 5,
            },
        )
        .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].speaker, SpeakerV2::Operator);
        assert_eq!(page.items[0].text, "ship the migration");
        assert_eq!(page.items[0].operator_message_id, Some(sent.message_id));
        assert_eq!(page.operator_messages[0].state, "delivered");
        assert!(page.operator_messages[0].delivered_at.is_some());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_message_only_reaches_a_manager_seat_of_the_named_project() {
        let mut store = Store::open_in_memory().unwrap();
        let alpha = project_with_manager(&store, "Alpha");
        let beta = project_with_manager(&store, "Beta");
        let worker = standard(&store, Some(alpha.project), SessionStatus::Running);
        let global = global_seat(&store, &[alpha.project]);

        // The covering global seat is a legitimate target for its project.
        assert!(
            send(
                &mut store,
                alpha.project,
                global,
                "hello global",
                Uuid::new_v4()
            )
            .is_ok()
        );
        for (project, session, text) in [
            (alpha.project, worker.id, "to a worker"),
            (
                alpha.project,
                beta.manager,
                "to the wrong project's manager",
            ),
            (beta.project, global, "global does not cover beta"),
            (alpha.project, Uuid::new_v4(), "to nobody"),
        ] {
            let error = send(&mut store, project, session, text, Uuid::new_v4()).unwrap_err();
            assert!(
                error.to_string().contains(REFUSAL_SEAT_OUT_OF_SCOPE),
                "{text}: {error}"
            );
            // Only the one accepted "hello global" is queued anywhere.
            let queued = store.list_operator_messages(session).unwrap().len();
            assert_eq!(queued, usize::from(session == global), "{text}");
        }
        for text in ["", "   ", "nul\u{0}byte"] {
            let error = send(
                &mut store,
                alpha.project,
                alpha.manager,
                text,
                Uuid::new_v4(),
            )
            .unwrap_err();
            assert!(error.to_string().contains(REFUSAL_INVALID_MESSAGE));
        }
        let oversize = "x".repeat(rsi_common::remote_manager_v2::MAX_MESSAGE_BYTES + 1);
        assert!(
            send(
                &mut store,
                alpha.project,
                alpha.manager,
                &oversize,
                Uuid::new_v4()
            )
            .is_err()
        );
        assert!(
            store
                .list_operator_messages(alpha.manager)
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn the_write_params_are_tokenless_and_strict() {
        let params = serde_json::json!({
            "project_id": Uuid::new_v4(), "seat_session_id": Uuid::new_v4(),
            "text": "hi", "idempotency_key": Uuid::new_v4(),
        });
        let ok = RpcRequest::new("RemoteSendManagerMessageV2", params.clone());
        assert!(parse_send(&ok).is_ok());
        let mut tokened = ok.clone();
        tokened.session_token = Some("agent-token".into());
        assert!(parse_send(&tokened).is_err());
        let mut wrong_version = ok.clone();
        wrong_version.jsonrpc = "1.0".into();
        assert!(parse_send(&wrong_version).is_err());
        let mut extra = params;
        extra["origin"] = serde_json::json!({"kind": "agent"});
        assert!(parse_send(&RpcRequest::new("RemoteSendManagerMessageV2", extra)).is_err());
    }
}
