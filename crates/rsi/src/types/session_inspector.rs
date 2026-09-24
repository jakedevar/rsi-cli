//! Passive, state-adaptive selected-session inspector view model.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use rsi_common::types::{SessionKind, SessionProvider, SessionStatus};
use uuid::Uuid;

use crate::app::App;
use crate::types::{
    ContextBudgetViewModel, SessionFocusEntry, SessionFocusGroup, compute_context_budget_view,
    resolve_session_display_identity,
};

const INSPECTOR_TEXT_LIMIT: usize = 420;
const MAX_HIERARCHY_DEPTH: usize = 16;
const ACTIVE_DESCENDANT_LIMIT: usize = 3;

#[derive(Debug, Clone, PartialEq)]
pub struct SessionInspectorViewModel {
    pub session_id: Uuid,
    pub ordinal: usize,
    pub title: String,
    pub kind: Option<String>,
    pub status: SessionStatus,
    pub location: String,
    pub working_dir: String,
    pub sandbox_root: Option<String>,
    pub sandbox_branch: Option<String>,
    pub session_kind: SessionKind,
    /// Attention reasons and state flags as one deduplicated, typed list:
    /// attention first (most urgent first), then passive flags.
    pub signals: Vec<InspectorSignal>,
    pub runtime: SessionRuntimeFacts,
    pub body: SessionInspectorBody,
}

/// One typed inspector signal. Replaces the former parallel `attention` and
/// `flags` string lists, which repeated the same facts ("Unread output" and
/// "new message", "Session may be stalled" and "stalled").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectorSignal {
    /// Pending question or approval.
    NeedsInput,
    Failed,
    Retry {
        attempt: u8,
        max: u8,
    },
    Stalled,
    Unread,
    Pinned,
    TestingNeeded,
    RotationDisabled,
    PendingArchive,
    EpicLead,
}

impl InspectorSignal {
    /// Short operator-facing label shown beside the signal glyph.
    pub fn label(self) -> String {
        match self {
            Self::NeedsInput => "needs you".to_string(),
            Self::Failed => "failed".to_string(),
            Self::Retry { attempt, max } => format!("retry {attempt}/{max}"),
            Self::Stalled => "stalled".to_string(),
            Self::Unread => "unread".to_string(),
            Self::Pinned => "pinned".to_string(),
            Self::TestingNeeded => "testing needed".to_string(),
            Self::RotationDisabled => "rotation off".to_string(),
            Self::PendingArchive => "archiving".to_string(),
            Self::EpicLead => "epic lead".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionRuntimeFacts {
    pub provider: Option<SessionProvider>,
    pub model: Option<String>,
    pub turns: Option<u32>,
    pub context: ContextBudgetViewModel,
    pub cost_usd: Option<f64>,
    pub work_time_ms: Option<u64>,
    pub duration_ms: Option<u64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SessionInspectorBody {
    Waiting {
        requirement: WaitingRequirement,
        next_action: String,
        why: String,
        latest_state: Option<String>,
    },
    Running {
        current_work: String,
        /// `current_work` is the generic lifecycle placeholder ("Provider
        /// session is running."), not a reported task. Surfaces that already
        /// show the lifecycle status skip it; compact surfaces still use it.
        current_work_is_placeholder: bool,
        summary: Option<String>,
        description: Option<String>,
        latest_state: Option<String>,
        warning: Option<String>,
    },
    Failed {
        evidence: String,
        evidence_source: FailureEvidenceSource,
        retry: Option<RetryFacts>,
        summary: Option<String>,
        description: Option<String>,
        last_good_state: Option<String>,
        artifact: Option<String>,
    },
    Completed {
        outcome: String,
        summary: Option<String>,
        description: Option<String>,
        test_passed: Option<bool>,
        clippy_passed: Option<bool>,
        artifact: Option<String>,
        handoff: Option<String>,
        follow_up: Option<String>,
    },
    Container {
        rollup: SessionFocusEntry,
        next_descendant: Option<InspectorDescendant>,
        active_descendants: Vec<InspectorDescendant>,
        direct_children: usize,
        lead: Option<String>,
    },
    Inactive {
        reason: String,
        outcome: Option<String>,
        summary: Option<String>,
        description: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitingRequirement {
    Reply,
    Approval,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureEvidenceSource {
    StopReason,
    CachedEventHeuristic,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryFacts {
    pub attempt: u8,
    pub max: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectorDescendant {
    pub id: Uuid,
    pub title: String,
    pub status: SessionStatus,
    pub group: SessionFocusGroup,
}

pub fn compute_session_inspector(
    app: &App,
    session_id: Uuid,
    ordinal: usize,
) -> Option<SessionInspectorViewModel> {
    let state = app.sessions.get(&session_id)?;
    let session = &state.session;
    let identity = resolve_session_display_identity(session, &app.sessions);
    let base_title = identity.effective_title;
    let rotation_depth = identity.rotation_depth;
    let title = if rotation_depth == 0 {
        clean_text(&base_title)
    } else {
        format!("{} ↻{}", clean_text(&base_title), rotation_depth)
    };

    let body = if matches!(session.session_kind, SessionKind::Group | SessionKind::Epic) {
        compute_container_body(app, session_id)
    } else if session.pending_question.is_some() || session.status == SessionStatus::WaitingApproval
    {
        compute_waiting_body(state, &title)
    } else {
        match session.status {
            SessionStatus::Starting | SessionStatus::Running => compute_running_body(state, &title),
            SessionStatus::Failed => compute_failed_body(state, &title),
            SessionStatus::Completed => compute_completed_body(state, &title),
            SessionStatus::Interrupted | SessionStatus::Archived | SessionStatus::Deleted => {
                compute_inactive_body(state, &title)
            }
            _ => compute_inactive_body(state, &title),
        }
    };

    let runtime = SessionRuntimeFacts {
        provider: if matches!(session.session_kind, SessionKind::Group | SessionKind::Epic) {
            None
        } else {
            Some(session.provider)
        },
        model: if matches!(session.session_kind, SessionKind::Group | SessionKind::Epic) {
            None
        } else {
            session.model.clone()
        },
        turns: session.num_turns,
        context: compute_context_budget_view(state),
        cost_usd: session.cost_usd,
        work_time_ms: session.work_time_ms,
        duration_ms: session.duration_ms,
        created_at: session.created_at,
        updated_at: session.updated_at,
    };

    Some(SessionInspectorViewModel {
        session_id,
        ordinal,
        title,
        kind: kind_label(session.session_kind).map(str::to_string),
        status: session.status,
        location: session_location(app, session_id),
        working_dir: session.working_dir.display().to_string(),
        sandbox_root: session
            .sandbox_root
            .as_ref()
            .map(|path| path.display().to_string()),
        sandbox_branch: session.sandbox_branch.clone(),
        session_kind: session.session_kind,
        signals: inspector_signals(app, state),
        runtime,
        body,
    })
}

fn inspector_signals(app: &App, state: &crate::types::SessionState) -> Vec<InspectorSignal> {
    let session = &state.session;
    let mut signals = Vec::new();
    if session.pending_question.is_some() || session.status == SessionStatus::WaitingApproval {
        signals.push(InspectorSignal::NeedsInput);
    }
    if session.status == SessionStatus::Failed {
        signals.push(InspectorSignal::Failed);
    }
    if let Some((attempt, max)) = session.retry_attempt.zip(session.max_retries) {
        signals.push(InspectorSignal::Retry { attempt, max });
    }
    if state.is_stalled {
        signals.push(InspectorSignal::Stalled);
    }
    if state.events_generation > state.last_seen_events_generation {
        signals.push(InspectorSignal::Unread);
    }
    if session.pinned_at.is_some() {
        signals.push(InspectorSignal::Pinned);
    }
    if session.testing_needed_at.is_some() {
        signals.push(InspectorSignal::TestingNeeded);
    }
    if session.rotation_disabled_at.is_some() {
        signals.push(InspectorSignal::RotationDisabled);
    }
    if session.pending_archive {
        signals.push(InspectorSignal::PendingArchive);
    }
    if app
        .sessions
        .values()
        .any(|candidate| candidate.session.lead_session_id == Some(session.id))
    {
        signals.push(InspectorSignal::EpicLead);
    }
    signals
}

fn compute_waiting_body(state: &crate::types::SessionState, title: &str) -> SessionInspectorBody {
    let session = &state.session;
    let question = session
        .pending_question
        .as_ref()
        .and_then(|pending| pending.questions.first());
    let requirement = if question.is_some() {
        WaitingRequirement::Reply
    } else {
        WaitingRequirement::Approval
    };
    let next_action = question
        .map(|question| {
            let mut text = clean_text(&question.question);
            let extra = session
                .pending_question
                .as_ref()
                .map(|pending| pending.questions.len().saturating_sub(1))
                .unwrap_or(0);
            if extra > 0 {
                text.push_str(&format!(" (+{extra} more)"));
            }
            text
        })
        .unwrap_or_else(|| "Review and approve the pending request.".to_string());
    let why = question
        .filter(|question| !question.header.trim().is_empty())
        .map(|question| format!("{} requires your response.", clean_text(&question.header)))
        .unwrap_or_else(|| "The session cannot continue without operator approval.".to_string());
    let latest_state = distinct_state_text(state, title, &[next_action.as_str()]);

    SessionInspectorBody::Waiting {
        requirement,
        next_action,
        why,
        latest_state,
    }
}

fn compute_running_body(state: &crate::types::SessionState, title: &str) -> SessionInspectorBody {
    let (summary, description) = inspector_summary_description(state, title, &[]);
    let reported_work = first_distinct(
        [state.session.active_task.as_deref()],
        &inspector_value_exclusions(state, title, &[], &summary, &description),
    );
    let current_work_is_placeholder = reported_work.is_none();
    let current_work = reported_work.unwrap_or_else(|| {
        if state.session.status == SessionStatus::Starting {
            "Starting provider session.".to_string()
        } else {
            "Provider session is running.".to_string()
        }
    });
    let mut latest_exclusions =
        inspector_value_exclusions(state, title, &[], &summary, &description);
    latest_exclusions.push(current_work.as_str());
    let latest_state = distinct_state_text(state, title, &latest_exclusions);
    let warning = if state.is_stalled {
        Some("No recent output; the session may be stalled.".to_string())
    } else if let (Some(attempt), Some(max)) =
        (state.session.retry_attempt, state.session.max_retries)
    {
        Some(format!("Retry attempt {attempt} of {max}."))
    } else {
        None
    };

    SessionInspectorBody::Running {
        current_work,
        current_work_is_placeholder,
        summary,
        description,
        latest_state,
        warning,
    }
}

fn compute_failed_body(state: &crate::types::SessionState, title: &str) -> SessionInspectorBody {
    let (evidence, evidence_source) = if let Some(reason) = state
        .session
        .stop_reason
        .as_deref()
        .filter(|reason| !reason.trim().is_empty())
    {
        (clean_text(reason), FailureEvidenceSource::StopReason)
    } else if let Some(event) =
        latest_cached_event_text(state, &[title, state.session.query.as_str()])
    {
        (event, FailureEvidenceSource::CachedEventHeuristic)
    } else {
        (
            "No failure reason is available in cached session state.".to_string(),
            FailureEvidenceSource::Unavailable,
        )
    };
    let (summary, description) = inspector_summary_description(state, title, &[evidence.as_str()]);
    let retry = state
        .session
        .retry_attempt
        .zip(state.session.max_retries)
        .map(|(attempt, max)| RetryFacts { attempt, max });
    let last_good_state = first_distinct(
        [state.session.active_task.as_deref()],
        &inspector_value_exclusions(state, title, &[evidence.as_str()], &summary, &description),
    );

    SessionInspectorBody::Failed {
        evidence,
        evidence_source,
        retry,
        summary,
        description,
        last_good_state,
        artifact: state.session.pipeline_artifact.clone(),
    }
}

fn compute_completed_body(state: &crate::types::SessionState, title: &str) -> SessionInspectorBody {
    let (summary, description) = inspector_summary_description(state, title, &[]);
    let exclusions = inspector_value_exclusions(state, title, &[], &summary, &description);
    let outcome = first_distinct([state.session.active_task.as_deref()], &exclusions)
        .or_else(|| latest_cached_event_text(state, &exclusions))
        .unwrap_or_else(|| "Session completed; no outcome summary is cached.".to_string());
    let follow_up = if state.session.testing_needed_at.is_some() {
        Some("Manual testing is still required.".to_string())
    } else if state.session.pending_archive {
        Some("Ready for archive after review.".to_string())
    } else {
        None
    };

    SessionInspectorBody::Completed {
        outcome,
        summary,
        description,
        test_passed: state.session.test_passed,
        clippy_passed: state.session.clippy_passed,
        artifact: state.session.pipeline_artifact.clone(),
        handoff: state.session.handoff_filepath.clone(),
        follow_up,
    }
}

fn compute_container_body(app: &App, session_id: Uuid) -> SessionInspectorBody {
    let fallback_focus;
    let focus_index = if app
        .session_list_render
        .focus_index
        .contains_key(&session_id)
    {
        &app.session_list_render.focus_index
    } else {
        fallback_focus = crate::types::compute_session_focus_index(&app.sessions, Utc::now());
        &fallback_focus
    };
    let rollup = focus_index
        .get(&session_id)
        .copied()
        .unwrap_or(SessionFocusEntry {
            group: SessionFocusGroup::Quiet,
            attention_count: 0,
            active_count: 0,
            running_agent_count: 0,
            running_epic_count: 0,
            descendant_count: 0,
        });

    let mut descendants = Vec::new();
    collect_descendants(
        app,
        session_id,
        focus_index,
        0,
        &mut HashSet::new(),
        &mut descendants,
    );
    let next_descendant = descendants
        .iter()
        .filter(|descendant| descendant.group == SessionFocusGroup::NeedsYou)
        .min_by_key(|descendant| {
            app.filtered_session_order
                .iter()
                .position(|id| *id == descendant.id)
                .unwrap_or(usize::MAX)
        })
        .cloned();
    let active_descendants = descendants
        .iter()
        .filter(|descendant| descendant.group == SessionFocusGroup::InFlight)
        .take(ACTIVE_DESCENDANT_LIMIT)
        .cloned()
        .collect();
    let direct_children = app
        .children_by_parent
        .get(&Some(session_id))
        .map(Vec::len)
        .unwrap_or(0);
    let lead = app
        .sessions
        .get(&session_id)
        .and_then(|state| state.session.lead_session_id)
        .and_then(|lead_id| app.sessions.get(&lead_id))
        .map(|state| display_title(app, &state.session));

    SessionInspectorBody::Container {
        rollup,
        next_descendant,
        active_descendants,
        direct_children,
        lead,
    }
}

fn compute_inactive_body(state: &crate::types::SessionState, title: &str) -> SessionInspectorBody {
    let reason = match state.session.status {
        SessionStatus::Interrupted => state
            .session
            .stop_reason
            .as_deref()
            .map(clean_text)
            .unwrap_or_else(|| "Interrupted by the operator or provider.".to_string()),
        SessionStatus::Archived => "Archived; retained for reference.".to_string(),
        SessionStatus::Deleted => "Deleted; unavailable for active work.".to_string(),
        _ => "Inactive session state.".to_string(),
    };
    let (summary, description) = inspector_summary_description(state, title, &[reason.as_str()]);
    let outcome = first_distinct(
        [state.session.active_task.as_deref()],
        &inspector_value_exclusions(state, title, &[reason.as_str()], &summary, &description),
    );
    SessionInspectorBody::Inactive {
        reason,
        outcome,
        summary,
        description,
    }
}

fn inspector_summary_description(
    state: &crate::types::SessionState,
    title: &str,
    extra_exclusions: &[&str],
) -> (Option<String>, Option<String>) {
    let summary = first_distinct(
        [state.session.short_summary.as_deref()],
        &inspector_value_exclusions(state, title, extra_exclusions, &None, &None),
    );
    let description = first_distinct(
        [state.session.description.as_deref()],
        &inspector_value_exclusions(state, title, extra_exclusions, &summary, &None),
    );
    (summary, description)
}

fn inspector_value_exclusions<'a>(
    state: &'a crate::types::SessionState,
    title: &'a str,
    extra_exclusions: &[&'a str],
    summary: &'a Option<String>,
    description: &'a Option<String>,
) -> Vec<&'a str> {
    let mut exclusions = vec![title, state.session.query.as_str()];
    exclusions.extend_from_slice(extra_exclusions);
    exclusions.extend(summary.iter().map(String::as_str));
    exclusions.extend(description.iter().map(String::as_str));
    exclusions
}

fn collect_descendants(
    app: &App,
    parent_id: Uuid,
    focus_index: &std::collections::HashMap<Uuid, SessionFocusEntry>,
    depth: usize,
    seen: &mut HashSet<Uuid>,
    out: &mut Vec<InspectorDescendant>,
) {
    if depth >= MAX_HIERARCHY_DEPTH || !seen.insert(parent_id) {
        return;
    }
    if let Some(children) = app.children_by_parent.get(&Some(parent_id)) {
        for child_id in children {
            if let Some(state) = app.sessions.get(child_id) {
                let group = focus_index
                    .get(child_id)
                    .map(|entry| entry.group)
                    .unwrap_or(SessionFocusGroup::Quiet);
                out.push(InspectorDescendant {
                    id: *child_id,
                    title: display_title(app, &state.session),
                    status: state.session.status,
                    group,
                });
                collect_descendants(app, *child_id, focus_index, depth + 1, seen, out);
            }
        }
    }
    seen.remove(&parent_id);
}

fn distinct_state_text(
    state: &crate::types::SessionState,
    title: &str,
    extra_exclusions: &[&str],
) -> Option<String> {
    let mut excluded = vec![title, state.session.query.as_str()];
    excluded.extend_from_slice(extra_exclusions);
    first_distinct(
        [
            state.session.active_task.as_deref(),
            state.session.short_summary.as_deref(),
            state.session.description.as_deref(),
        ],
        &excluded,
    )
    .or_else(|| latest_cached_event_text(state, &excluded))
}

fn latest_cached_event_text(
    state: &crate::types::SessionState,
    exclusions: &[&str],
) -> Option<String> {
    state
        .events
        .iter()
        .rev()
        .filter(|event| !event.content.trim().is_empty())
        .find_map(|event| distinct_value(&event.content, exclusions))
}

fn first_distinct<'a>(
    candidates: impl IntoIterator<Item = Option<&'a str>>,
    exclusions: &[&str],
) -> Option<String> {
    candidates
        .into_iter()
        .flatten()
        .find_map(|value| distinct_value(value, exclusions))
}

fn distinct_value(value: &str, exclusions: &[&str]) -> Option<String> {
    let cleaned = clean_text(value);
    if cleaned.is_empty()
        || exclusions
            .iter()
            .any(|excluded| equivalent_text(&cleaned, excluded))
    {
        None
    } else {
        Some(cleaned)
    }
}

fn equivalent_text(left: &str, right: &str) -> bool {
    clean_text(left).eq_ignore_ascii_case(&clean_text(right))
}

fn session_location(app: &App, session_id: Uuid) -> String {
    let Some(session) = app.sessions.get(&session_id).map(|state| &state.session) else {
        return "Unknown".to_string();
    };
    let project = session
        .project_id
        .and_then(|project_id| app.projects.iter().find(|project| project.id == project_id))
        .map(|project| project.name.clone())
        .unwrap_or_else(|| "All projects".to_string());

    let mut ancestors = Vec::new();
    let mut next = session.parent_id;
    let mut seen = HashSet::new();
    while let Some(parent_id) = next {
        if !seen.insert(parent_id) || ancestors.len() >= MAX_HIERARCHY_DEPTH {
            break;
        }
        let Some(parent) = app.sessions.get(&parent_id).map(|state| &state.session) else {
            break;
        };
        ancestors.push(display_title(app, parent));
        next = parent.parent_id;
    }
    ancestors.reverse();

    if ancestors.is_empty() {
        format!("{project} / Root")
    } else {
        format!("{project} / {}", ancestors.join(" / "))
    }
}

fn display_title(app: &App, session: &rsi_common::types::Session) -> String {
    clean_text(&resolve_session_display_identity(session, &app.sessions).effective_title)
}

fn kind_label(kind: SessionKind) -> Option<&'static str> {
    match kind {
        SessionKind::Standard => None,
        SessionKind::TaskRabbit => Some("TASKRABBIT"),
        SessionKind::Bug => Some("BUG"),
        SessionKind::Group => Some("GROUP"),
        SessionKind::Epic => Some("EPIC"),
        SessionKind::Story => Some("STORY"),
        SessionKind::Task => Some("TASK"),
        SessionKind::Feature => Some("FEATURE"),
        SessionKind::Refactor => Some("REFACTOR"),
        SessionKind::Research => Some("RESEARCH"),
        _ => Some("SESSION"),
    }
}

fn clean_text(value: &str) -> String {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= INSPECTOR_TEXT_LIMIT {
        return normalized;
    }
    let mut truncated: String = normalized
        .chars()
        .take(INSPECTOR_TEXT_LIMIT.saturating_sub(1))
        .collect();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::app_test_helpers::{baseline_session, with_session_list};
    use crate::types::SessionState;
    use rsi_common::types::{PendingQuestion, QuestionItem};

    #[test]
    fn question_and_bare_approval_use_explicit_waiting_bodies() {
        let mut app = with_session_list(1);
        let session_id = app.filtered_session_order[0];
        let session = &mut app.sessions.get_mut(&session_id).unwrap().session;
        session.pending_question = Some(PendingQuestion {
            questions: vec![QuestionItem {
                question: "Which persistence policy should be used?".to_string(),
                header: "Policy".to_string(),
                options: Vec::new(),
                multi_select: false,
            }],
        });

        let inspector = compute_session_inspector(&app, session_id, 1).unwrap();
        assert!(matches!(
            inspector.body,
            SessionInspectorBody::Waiting {
                requirement: WaitingRequirement::Reply,
                ref next_action,
                ..
            } if next_action == "Which persistence policy should be used?"
        ));

        let session = &mut app.sessions.get_mut(&session_id).unwrap().session;
        session.pending_question = None;
        session.status = SessionStatus::WaitingApproval;
        let inspector = compute_session_inspector(&app, session_id, 1).unwrap();
        assert!(matches!(
            inspector.body,
            SessionInspectorBody::Waiting {
                requirement: WaitingRequirement::Approval,
                ..
            }
        ));
    }

    #[test]
    fn running_failed_completed_and_inactive_statuses_are_exhaustive() {
        let mut app = with_session_list(1);
        let session_id = app.filtered_session_order[0];
        let cases = [
            SessionStatus::Starting,
            SessionStatus::Running,
            SessionStatus::Failed,
            SessionStatus::Completed,
            SessionStatus::Interrupted,
            SessionStatus::Archived,
            SessionStatus::Deleted,
        ];
        for status in cases {
            let session = &mut app.sessions.get_mut(&session_id).unwrap().session;
            session.status = status;
            session.stop_reason = (status == SessionStatus::Failed).then(|| "build failed".into());
            let body = compute_session_inspector(&app, session_id, 1).unwrap().body;
            match status {
                SessionStatus::Starting | SessionStatus::Running => {
                    assert!(matches!(body, SessionInspectorBody::Running { .. }));
                }
                SessionStatus::Failed => {
                    assert!(matches!(
                        body,
                        SessionInspectorBody::Failed {
                            evidence_source: FailureEvidenceSource::StopReason,
                            ..
                        }
                    ));
                }
                SessionStatus::Completed => {
                    assert!(matches!(body, SessionInspectorBody::Completed { .. }));
                }
                _ => assert!(matches!(body, SessionInspectorBody::Inactive { .. })),
            }
        }
    }

    #[test]
    fn failed_retry_and_completed_verification_remain_typed() {
        let mut app = with_session_list(1);
        let session_id = app.filtered_session_order[0];
        let session = &mut app.sessions.get_mut(&session_id).unwrap().session;
        session.status = SessionStatus::Failed;
        session.retry_attempt = Some(2);
        session.max_retries = Some(3);
        let body = compute_session_inspector(&app, session_id, 1).unwrap().body;
        assert!(matches!(
            body,
            SessionInspectorBody::Failed {
                retry: Some(RetryFacts { attempt: 2, max: 3 }),
                ..
            }
        ));

        let session = &mut app.sessions.get_mut(&session_id).unwrap().session;
        session.status = SessionStatus::Completed;
        session.test_passed = Some(false);
        session.clippy_passed = Some(true);
        session.handoff_filepath = Some("thoughts/handoff.md".into());
        let body = compute_session_inspector(&app, session_id, 1).unwrap().body;
        assert!(matches!(
            body,
            SessionInspectorBody::Completed {
                test_passed: Some(false),
                clippy_passed: Some(true),
                handoff: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn containers_take_precedence_and_surface_waiting_descendant() {
        let mut app = with_session_list(0);
        let parent_id = Uuid::new_v4();
        let child_id = Uuid::new_v4();
        let mut parent = baseline_session(parent_id, SessionKind::Epic);
        parent.status = SessionStatus::Completed;
        parent.title = Some("Relay epic".into());
        let mut child = baseline_session(child_id, SessionKind::Task);
        child.parent_id = Some(parent_id);
        child.status = SessionStatus::WaitingApproval;
        child.title = Some("Choose queue policy".into());
        app.sessions.insert(parent_id, SessionState::new(parent));
        app.sessions.insert(child_id, SessionState::new(child));
        app.filtered_session_order = vec![parent_id, child_id];
        app.children_by_parent
            .insert(Some(parent_id), vec![child_id]);

        let body = compute_session_inspector(&app, parent_id, 1).unwrap().body;
        assert!(matches!(
            body,
            SessionInspectorBody::Container {
                next_descendant: Some(InspectorDescendant { id, .. }),
                ..
            } if id == child_id
        ));
    }

    #[test]
    fn malformed_hierarchy_cycle_is_bounded() {
        let mut app = with_session_list(0);
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let mut a = baseline_session(first, SessionKind::Group);
        let mut b = baseline_session(second, SessionKind::Epic);
        a.parent_id = Some(second);
        b.parent_id = Some(first);
        app.sessions.insert(first, SessionState::new(a));
        app.sessions.insert(second, SessionState::new(b));
        app.children_by_parent.insert(Some(first), vec![second]);
        app.children_by_parent.insert(Some(second), vec![first]);

        let body = compute_session_inspector(&app, first, 1).unwrap().body;
        assert!(matches!(body, SessionInspectorBody::Container { .. }));
    }

    #[test]
    fn duplicate_prompt_is_not_repeated_as_latest_state_or_outcome() {
        let mut app = with_session_list(1);
        let session_id = app.filtered_session_order[0];
        let session = &mut app.sessions.get_mut(&session_id).unwrap().session;
        session.title = Some("Same prompt".into());
        session.query = "Same prompt".into();
        session.short_summary = Some("Same prompt".into());
        session.status = SessionStatus::Completed;

        let body = compute_session_inspector(&app, session_id, 1).unwrap().body;
        assert!(matches!(
            body,
            SessionInspectorBody::Completed { ref outcome, .. }
                if !outcome.eq_ignore_ascii_case("same prompt")
        ));
    }
}
