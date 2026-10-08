//! Operator-local Remote read of manager inbox and producer-bound targets.
//! Each bounded result belongs to one project/session. Pending publications
//! outside the inbox do not require a manager or inherit its policy fence.

use super::{ReadError, RemoteReadCompleted, RemoteReadLimiter, Result};
use crate::store::Store;
use rsi_common::harness_manager::HarnessManagerConfigV1;
use rsi_common::harness_manager_v2::{
    AgentManagerInspectRequestV2, MANAGER_V2_MAX_PAGE, ManagerInspectSectionV2, ManagerInspectionV2,
};
use rsi_common::remote_decision_targets::{
    DecisionTargetFenceV1, DecisionTargetItemV1, DecisionTargetKindV1, DecisionTargetsManagerV1,
    MAX_DECISION_TARGET_OPTIONS, MAX_DECISION_TARGETS, PendingDecisionTargetItemV1,
    RemoteGetDecisionTargetsResponseV1, RemoteGetDecisionTargetsV1,
};
use rsi_common::remote_read::{DecimalI64, Text};
use rsi_common::rpc::RpcRequest;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use uuid::Uuid;

/// Strict operator-local admission for this method. The socket boundary refuses
/// a tokened call before dispatch; the version and body checks fail closed for
/// the tokenless operator path that reaches here.
pub(super) fn parse_request(rpc: &RpcRequest) -> Result<RemoteGetDecisionTargetsV1> {
    if rpc.session_token.is_some() || rpc.jsonrpc != "2.0" {
        return Err(ReadError::InvalidSource);
    }
    serde_json::from_value::<RemoteGetDecisionTargetsV1>(rpc.params.clone())
        .map_err(|_| ReadError::InvalidSource)
}

/// Run one scoped read under the shared Remote permit. The blocking worker owns
/// the Store lock for the bounded page walk and releases it before transport.
pub(super) fn spawn_decision_targets_read(
    limiter: &RemoteReadLimiter,
    store: Arc<Mutex<Store>>,
    request: RemoteGetDecisionTargetsV1,
) -> Result<JoinHandle<RemoteReadCompleted<RemoteGetDecisionTargetsResponseV1>>> {
    limiter.spawn(move |budget| {
        budget.check()?;
        let guard = store.try_lock_owned().map_err(|_| ReadError::Busy)?;
        let response = decision_targets_response(&guard, &request)?;
        drop(guard);
        budget.check()?;
        Ok(response)
    })
}

/// At most four `Decisions` pages are examined before the response is marked
/// truncated, so one operator read can never scan an unbounded ledger.
const MAX_PAGES: usize = 4;

pub(super) fn decision_targets_response(
    store: &Store,
    request: &RemoteGetDecisionTargetsV1,
) -> Result<RemoteGetDecisionTargetsResponseV1> {
    let project =
        Uuid::parse_str(request.project_id.as_str()).map_err(|_| ReadError::InvalidSource)?;
    let session =
        Uuid::parse_str(request.session_id.as_str()).map_err(|_| ReadError::InvalidSource)?;
    let Some(config) = store
        .get_harness_manager(project)
        .map_err(|_| ReadError::SourceUnavailable)?
    else {
        let (pending_items, truncated) = pending_items(store, project, session, &[])?;
        return Ok(RemoteGetDecisionTargetsResponseV1 {
            manager: DecisionTargetsManagerV1::NotConfigured,
            fence: None,
            items: Vec::new(),
            pending_items,
            truncated,
        });
    };
    let mut items = Vec::new();
    let mut fence = None;
    let mut cursor: Option<String> = None;
    let mut truncated = false;
    let mut complete = false;
    for _ in 0..MAX_PAGES {
        let page = store
            .manager_v2_inspect_operator(
                project,
                &AgentManagerInspectRequestV2 {
                    section: ManagerInspectSectionV2::Decisions,
                    epic_id: None,
                    cursor: cursor.clone(),
                    limit: MANAGER_V2_MAX_PAGE,
                    project_id: None,
                },
            )
            .map_err(|_| ReadError::SourceUnavailable)?;
        if fence.is_none() {
            fence = current_fence(&page)?;
        }
        for row in &page.rows {
            if let Some(item) = decision_target_item(store, &config, row, session)? {
                if items.len() == MAX_DECISION_TARGETS {
                    truncated = true;
                    break;
                }
                items.push(item);
            }
        }
        complete = page.complete && page.next_cursor.is_none();
        if truncated || complete {
            break;
        }
        match page.next_cursor.clone() {
            Some(next) if Some(&next) != cursor.as_ref() => cursor = Some(next),
            _ => break,
        }
    }
    if !complete {
        truncated = true;
    }
    let (pending_items, pending_truncated) = pending_items(store, project, session, &items)?;
    Ok(RemoteGetDecisionTargetsResponseV1 {
        manager: DecisionTargetsManagerV1::Configured,
        fence,
        items,
        pending_items,
        truncated: truncated || pending_truncated,
    })
}

fn pending_items(
    store: &Store,
    project: Uuid,
    session: Uuid,
    manager_items: &[DecisionTargetItemV1],
) -> Result<(Vec<PendingDecisionTargetItemV1>, bool)> {
    let targets = store
        .remote_pending_decision_targets(project, session, MAX_DECISION_TARGETS + 1)
        .map_err(|_| ReadError::SourceUnavailable)?;
    let mut items = Vec::new();
    for (id, target) in targets {
        let approval = target["kind"] == "appserver_approval";
        let inbox_key = if approval {
            format!(
                "approval:{}",
                target["publication_id"].as_str().unwrap_or_default()
            )
        } else {
            format!("question:{session}")
        };
        if manager_items
            .iter()
            .any(|item| item.decision_key.as_str() == inbox_key)
        {
            continue;
        }
        if approval {
            let publication =
                Uuid::parse_str(target["publication_id"].as_str().unwrap_or_default())
                    .map_err(|_| ReadError::InvalidSource)?;
            let witness = crate::session::remote_selected_native_approval(session, publication)?;
            match witness.state {
                super::NativeRuntimeApprovalState::Present(row)
                    if row.writer_live
                        && !row.resolution_observed
                        && row.incarnation_id.to_string()
                            == target["incarnation_id"].as_str().unwrap_or_default() => {}
                _ => continue,
            }
        }
        let digest = crate::store::harness_manager_v2::fingerprint(&target)
            .map_err(|_| ReadError::SourceUnavailable)?;
        let question = serde_json::from_value::<rsi_common::types::PendingQuestion>(
            target["question"].clone(),
        )
        .ok();
        let title = if approval {
            target["method"].as_str().unwrap_or("Approval").to_owned()
        } else {
            question
                .as_ref()
                .map(|q| {
                    q.questions
                        .iter()
                        .map(|i| i.question.as_str())
                        .collect::<Vec<_>>()
                        .join("; ")
                })
                .unwrap_or_else(|| "Question".into())
        };
        let options = if approval {
            vec!["approve".into(), "deny".into()]
        } else {
            question
                .into_iter()
                .flat_map(|q| q.questions)
                .flat_map(|i| i.options)
                .map(|o| o.label)
                .collect()
        };
        items.push(PendingDecisionTargetItemV1 {
            receipt: store
                .remote_answer_receipt_for_target(session, &id, &digest)
                .map_err(|_| ReadError::SourceUnavailable)?,
            decision_id: Text::new(id).map_err(|_| ReadError::InvalidSource)?,
            target_digest: Text::new(digest).map_err(|_| ReadError::InvalidSource)?,
            kind: if approval {
                DecisionTargetKindV1::Approval
            } else {
                DecisionTargetKindV1::Question
            },
            title: wire_text(&title)?,
            detail: Text::new(String::new()).map_err(|_| ReadError::InvalidSource)?,
            options: bounded_options(options),
        });
    }
    let truncated = items.len() > MAX_DECISION_TARGETS;
    items.truncate(MAX_DECISION_TARGETS);
    Ok((items, truncated))
}

/// A fence is usable only while the project's current grant is live and covers
/// the observed scope version, exactly as the TUI board requires.
fn current_fence(page: &ManagerInspectionV2) -> Result<Option<DecisionTargetFenceV1>> {
    let Some(policy) = page.policy.as_ref().filter(|policy| {
        !policy.revoked && policy.row_version > 0 && policy.scope_version == page.scope_version
    }) else {
        return Ok(None);
    };
    Ok(Some(DecisionTargetFenceV1 {
        policy_version: DecimalI64::new(policy.row_version.to_string())
            .map_err(|_| ReadError::InvalidSource)?,
        scope_version: DecimalI64::new(page.scope_version.to_string())
            .map_err(|_| ReadError::InvalidSource)?,
    }))
}

fn decision_target_item(
    store: &Store,
    config: &HarnessManagerConfigV1,
    row: &Value,
    session: Uuid,
) -> Result<Option<DecisionTargetItemV1>> {
    let Some(key) = row["key"].as_str() else {
        return Ok(None);
    };
    let kind = if key.starts_with("question:") {
        DecisionTargetKindV1::Question
    } else if key.starts_with("approval:") {
        DecisionTargetKindV1::Approval
    } else {
        return Ok(None);
    };
    if row["session_id"]
        .as_str()
        .and_then(|raw| Uuid::parse_str(raw).ok())
        != Some(session)
    {
        return Ok(None);
    }
    if row["status"].as_str() != Some("pending") {
        return Ok(None);
    }
    let Some(version) = row["row_version"].as_i64().filter(|value| *value > 0) else {
        return Ok(None);
    };
    let Some(digest) = row["target_digest"]
        .as_str()
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    let title = row["question"].as_str().unwrap_or(key);
    let detail = row["next_action"].as_str().unwrap_or_default();
    let mut options = row["available_answers"]
        .as_array()
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str().map(str::to_owned))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if options.is_empty()
        && kind == DecisionTargetKindV1::Question
        && let Some(record) = store
            .manager_v2_record(config, "decision_target", key)
            .map_err(|_| ReadError::SourceUnavailable)?
        && let Ok(question) = serde_json::from_value::<rsi_common::types::PendingQuestion>(
            record.payload["question"].clone(),
        )
    {
        for item in question.questions {
            options.extend(item.options.into_iter().map(|option| option.label));
        }
    }
    // Identities are echoed back exactly or not at all: a key or digest that
    // does not fit the wire caps is skipped, never truncated.
    let (Ok(decision_key), Ok(target_digest)) = (
        Text::<160>::new(key.to_owned()),
        Text::<128>::new(digest.to_owned()),
    ) else {
        return Ok(None);
    };
    Ok(Some(DecisionTargetItemV1 {
        decision_key,
        row_version: DecimalI64::new(version.to_string()).map_err(|_| ReadError::InvalidSource)?,
        target_digest,
        kind,
        title: wire_text::<512>(title)?,
        detail: Text::<2048, false>::new(bound_text(detail, 2048))
            .map_err(|_| ReadError::InvalidSource)?,
        options: bounded_options(options),
    }))
}

fn wire_text<const MAX: usize>(value: &str) -> Result<Text<MAX>> {
    Text::<MAX>::new(bound_text(value, MAX)).map_err(|_| ReadError::InvalidSource)
}

/// Cut on a UTF-8 boundary so a truncation never splits a character.
fn bound_text(value: &str, cap: usize) -> String {
    if value.len() <= cap {
        return value.to_owned();
    }
    let mut end = cap;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn bounded_options(values: Vec<String>) -> Vec<Text<128>> {
    let mut options: Vec<Text<128>> = Vec::new();
    for value in values {
        if options.len() == MAX_DECISION_TARGET_OPTIONS {
            break;
        }
        let value = bound_text(&value, 128);
        if value.is_empty() {
            continue;
        }
        if let Ok(option) = Text::<128>::new(value)
            && !options.contains(&option)
        {
            options.push(option);
        }
    }
    options
}

#[cfg(all(
    test,
    any(not(feature = "test-shard-mode"), feature = "test-shard-store-04")
))]
mod tests {
    use super::*;
    use crate::session::agent_verbs::tests::test_session;
    use chrono::Utc;
    use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
    use rsi_common::harness_manager_v2::{ConfigureHarnessManagerPolicyRequestV2, ManagerPolicyV2};
    use rsi_common::remote_read::WireUuid;
    use rsi_common::types::{
        ConversationEvent, EventType, PendingQuestion, Project, QuestionItem, Session, SessionKind,
        SessionStatus,
    };
    use serde_json::json;
    use std::path::PathBuf;

    struct Fixture {
        config: HarnessManagerConfigV1,
        lead: Session,
        epic: Uuid,
    }

    /// A configured manager, one epic, and one feature lead under it. Mirrors
    /// the shared manager-coordinator fixture without reaching into another
    /// module's private test helpers.
    fn fixture(store: &Store) -> Fixture {
        let stamp = Utc::now();
        let project = Uuid::new_v4();
        store
            .insert_project(&Project {
                id: project,
                name: format!("Decision targets {project}"),
                path: None,
                description: None,
                color: Project::DEFAULT_COLOR.into(),
                context_files: None,
                created_at: stamp,
                updated_at: stamp,
            })
            .unwrap();
        let mut manager = test_session(Uuid::new_v4(), PathBuf::from("/var/tmp/decision-targets"));
        manager.session_kind = SessionKind::Standard;
        manager.project_id = Some(project);
        manager.status = SessionStatus::Completed;
        store.insert_session(&manager).unwrap();
        let mut group = manager.clone();
        group.id = Uuid::new_v4();
        group.session_kind = SessionKind::Group;
        store.insert_session(&group).unwrap();
        let mut epic = manager.clone();
        epic.id = Uuid::new_v4();
        epic.session_kind = SessionKind::Epic;
        epic.parent_id = Some(group.id);
        store.insert_session(&epic).unwrap();
        let mut lead = manager.clone();
        lead.id = Uuid::new_v4();
        lead.parent_id = Some(epic.id);
        lead.session_kind = SessionKind::Feature;
        store.insert_session(&lead).unwrap();
        store
            .conn
            .execute(
                "UPDATE sessions SET lead_session_id=?2 WHERE id=?1",
                rusqlite::params![epic.id.to_string(), lead.id.to_string()],
            )
            .unwrap();
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
                idempotency_key: "decision-targets-policy".into(),
                policy: ManagerPolicyV2::default(),
            })
            .unwrap();
        Fixture {
            config,
            lead,
            epic: epic.id,
        }
    }

    fn raise_question(store: &Store, session: Uuid, sequence: i64) {
        store
            .conn
            .execute(
                "UPDATE sessions SET provider='Claude' WHERE id=?1",
                [session.to_string()],
            )
            .unwrap();
        let question = PendingQuestion {
            questions: vec![QuestionItem {
                question: "Use the migration allocation?".into(),
                header: "Migration".into(),
                options: Vec::new(),
                multi_select: false,
            }],
        };
        store
            .publish_pending_question_event(
                &ConversationEvent {
                    id: 0,
                    session_id: session,
                    sequence: i32::try_from(sequence).unwrap(),
                    event_type: EventType::ToolUse,
                    role: None,
                    created_at: Utc::now(),
                    content: String::new(),
                    tool_name: Some("AskUserQuestion".into()),
                    tool_input: Some(Box::new(json!(question))),
                    offload_id: None,
                    tool_use_id: Some(format!("question-{sequence}")),
                    metadata: None,
                },
                None,
                &question,
            )
            .unwrap();
        store
            .update_session_status(session, SessionStatus::WaitingApproval)
            .unwrap();
    }

    fn targets(project: Uuid, session: Uuid) -> RemoteGetDecisionTargetsV1 {
        RemoteGetDecisionTargetsV1 {
            project_id: WireUuid::new(project.to_string()).unwrap(),
            session_id: WireUuid::new(session.to_string()).unwrap(),
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn pending_question_echoes_store_identity_and_fence() {
        let store = Store::open_in_memory().unwrap();
        let fixture = fixture(&store);
        raise_question(&store, fixture.lead.id, 1);
        let response =
            decision_targets_response(&store, &targets(fixture.config.project_id, fixture.lead.id))
                .unwrap();
        assert_eq!(response.manager, DecisionTargetsManagerV1::Configured);
        assert_eq!(response.items.len(), 1);
        assert!(!response.truncated);
        let key = format!("question:{}", fixture.lead.id);
        let stored = store
            .manager_v2_record(&fixture.config, "decision", &key)
            .unwrap()
            .unwrap();
        let item = &response.items[0];
        assert_eq!(item.decision_key.as_str(), key);
        assert_eq!(item.row_version.as_str(), stored.row_version.to_string());
        assert_eq!(
            item.target_digest.as_str(),
            stored.payload["target_digest"].as_str().unwrap()
        );
        assert_eq!(item.kind, DecisionTargetKindV1::Question);
        let fence = response.fence.expect("current policy fence");
        assert_eq!(
            fence.scope_version.as_str(),
            fixture.config.row_version.to_string()
        );
        assert_eq!(fence.policy_version.as_str(), "1");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn another_sessions_decisions_are_never_returned() {
        let store = Store::open_in_memory().unwrap();
        let fixture = fixture(&store);
        let mut other = fixture.lead.clone();
        other.id = Uuid::new_v4();
        other.title = Some("Second worker".into());
        store.insert_session(&other).unwrap();
        raise_question(&store, fixture.lead.id, 1);
        raise_question(&store, other.id, 1);
        let response =
            decision_targets_response(&store, &targets(fixture.config.project_id, fixture.lead.id))
                .unwrap();
        assert_eq!(response.items.len(), 1);
        assert_eq!(
            response.items[0].decision_key.as_str(),
            format!("question:{}", fixture.lead.id)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn non_pending_decision_is_skipped() {
        let store = Store::open_in_memory().unwrap();
        let fixture = fixture(&store);
        let key = format!("approval:{}", Uuid::new_v4());
        store
            .manager_v2_put_record(
                &fixture.config,
                "decision",
                &key,
                Some(fixture.epic),
                0,
                &json!({
                    "key": key,
                    "session_id": fixture.lead.id,
                    "question": "Approve the reservation?",
                    "status": "resolved",
                    "target_digest": "sha256:deadbeef",
                }),
            )
            .unwrap();
        let response =
            decision_targets_response(&store, &targets(fixture.config.project_id, fixture.lead.id))
                .unwrap();
        assert!(response.items.is_empty());
        assert!(!response.truncated);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn more_than_sixteen_pending_is_truncated() {
        let store = Store::open_in_memory().unwrap();
        let fixture = fixture(&store);
        for _ in 0..17 {
            let key = format!("approval:{}", Uuid::new_v4());
            store
                .manager_v2_put_record(
                    &fixture.config,
                    "decision",
                    &key,
                    Some(fixture.epic),
                    0,
                    &json!({
                        "key": key,
                        "session_id": fixture.lead.id,
                        "question": "Approve this occurrence?",
                        "status": "pending",
                        "target_digest": "sha256:deadbeef",
                    }),
                )
                .unwrap();
        }
        let response =
            decision_targets_response(&store, &targets(fixture.config.project_id, fixture.lead.id))
                .unwrap();
        assert_eq!(response.items.len(), MAX_DECISION_TARGETS);
        assert!(response.truncated);
        assert!(
            response
                .items
                .iter()
                .all(|item| item.kind == DecisionTargetKindV1::Approval)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn project_without_manager_is_not_configured() {
        let store = Store::open_in_memory().unwrap();
        let response =
            decision_targets_response(&store, &targets(Uuid::new_v4(), Uuid::new_v4())).unwrap();
        assert_eq!(response.manager, DecisionTargetsManagerV1::NotConfigured);
        assert!(response.items.is_empty());
        assert!(response.fence.is_none());
        assert!(!response.truncated);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn operator_dispatch_refuses_tokened_and_wrong_version_calls() {
        assert!(crate::remote_read::is_operator_read_method(
            "RemoteGetDecisionTargetsV1"
        ));
        let params = json!({
            "project_id": Uuid::new_v4().to_string(),
            "session_id": Uuid::new_v4().to_string(),
        });
        let mut tokened = RpcRequest::new("RemoteGetDecisionTargetsV1", params.clone());
        tokened.session_token = Some("test-token".into());
        assert!(matches!(
            parse_request(&tokened),
            Err(ReadError::InvalidSource)
        ));
        let mut wrong_version = RpcRequest::new("RemoteGetDecisionTargetsV1", params.clone());
        wrong_version.jsonrpc = "1.0".into();
        assert!(matches!(
            parse_request(&wrong_version),
            Err(ReadError::InvalidSource)
        ));
        let parsed = parse_request(&RpcRequest::new(
            "RemoteGetDecisionTargetsV1",
            params.clone(),
        ))
        .expect("tokenless operator read is admitted");
        assert_eq!(
            parsed.session_id.as_str(),
            params["session_id"].as_str().unwrap()
        );
        // The method stays outside the six V1 observation methods and their
        // closed wire enum.
        assert_eq!(rsi_common::remote_read::REQUIRED_CAPABILITIES.len(), 6);
        assert!(
            serde_json::from_value::<rsi_common::remote_read::ReadRequestV1>(json!({
                "method": "RemoteGetDecisionTargetsV1",
                "params": params,
            }))
            .is_err()
        );
    }
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn pending_questions_outside_inbox_have_exact_digest_and_receipt() {
        let store = Store::open_in_memory().unwrap();
        let fixture = fixture(&store);
        let mut leaf = fixture.lead.clone();
        leaf.id = Uuid::new_v4();
        leaf.parent_id = None;
        store.insert_session(&leaf).unwrap();
        raise_question(&store, leaf.id, 1);
        let response =
            decision_targets_response(&store, &targets(fixture.config.project_id, leaf.id))
                .unwrap();
        assert!(response.items.is_empty());
        assert_eq!(response.pending_items.len(), 1);
        let item = &response.pending_items[0];
        let target = store
            .remote_pending_decision_target(
                fixture.config.project_id,
                leaf.id,
                item.decision_id.as_str(),
            )
            .unwrap();
        assert_eq!(
            item.target_digest.as_str(),
            crate::store::harness_manager_v2::fingerprint(&target).unwrap()
        );
        let request: rsi_common::remote_pending_decisions::AnswerPendingDecisionV1 = serde_json::from_value(json!({
            "project_id":fixture.config.project_id, "session_id":leaf.id, "decision_id":item.decision_id,
            "expected_target_digest":item.target_digest, "answer":"yes", "idempotency_key":Uuid::new_v4(),
            "origin":{"kind":"remote", "client_node":"test-device", "gateway_epoch":Uuid::new_v4()}
        })).unwrap();
        let receipt = store.prepare_remote_answer(&request).unwrap();
        let again = decision_targets_response(&store, &targets(fixture.config.project_id, leaf.id))
            .unwrap();
        assert_eq!(again.pending_items[0].receipt.as_ref(), Some(&receipt));
        let foreign = decision_targets_response(&store, &targets(Uuid::new_v4(), leaf.id)).unwrap();
        assert!(foreign.pending_items.is_empty());
    }
}
