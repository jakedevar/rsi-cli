//! Manager resumability and program-continuation gates, moved down from
//! `session::lifecycle` and `session::agent_verbs` so `store` has no edge into
//! `session` (issue #1021 S3a). The originals re-export these at their old
//! paths. Pure functions over `Session`/`Store`; no session-manager state.

use super::schedule_wake_job::{deterministic_program_guard_job_id, is_program_guard_sentinel};
use crate::error::{DaemonError, Result};
use rsi_common::agent_contract::ProgramContinuationIntentV1;
use rsi_common::types::{
    Recurrence, ScheduledJob, Session, SessionProvider, SessionStatus, WakeMode,
};
use uuid::Uuid;

pub fn manager_notice_deferred() -> DaemonError {
    DaemonError::InvalidParam("manager_notice_deferred".into())
}

pub fn resumable_provider_session_id(session: &Session) -> Option<String> {
    match session.provider {
        SessionProvider::Local => Some(
            session
                .claude_session_id
                .clone()
                .unwrap_or_else(|| session.id.to_string()),
        ),
        _ => session.claude_session_id.clone(),
    }
}

/// The single manager resumability predicate. `resume_lead` admission,
/// `retry_lead` admission and the manager continuation gate all call it, so no
/// lead is queued for a resume the gate refuses. `CodexAppServer` leads are never
/// manager-resumable; other providers need a resumable provider session.
pub(crate) fn manager_lead_provider_resumable(session: &Session) -> bool {
    session.provider != SessionProvider::CodexAppServer
        && resumable_provider_session_id(session).is_some()
}

/// Manager `resume_lead` target check, shared by admission (for settled leads)
/// and the continuation gate. A lead that is not a settled leaf is not
/// resumable now; a settled lead failing [`manager_lead_provider_resumable`]
/// gets typed `manager_v2_resume_unavailable` naming `retry_lead`.
pub fn check_manager_resume_target(session: &Session) -> Result<()> {
    if !matches!(
        session.status,
        SessionStatus::Completed | SessionStatus::Interrupted | SessionStatus::Failed
    ) || !rsi_common::is_leaf_kind(session.session_kind)
    {
        return Err(DaemonError::InvalidParam(
            "manager_v2_lead_not_resumable".into(),
        ));
    }
    if !manager_lead_provider_resumable(session) {
        return Err(manager_resume_unavailable());
    }
    Ok(())
}

/// Typed manager refusal naming the lifecycle action that can proceed. The
/// message is the bare code, so `safe_action_error` still passes it through.
pub(crate) fn manager_refusal_with_next_action(
    code: &'static str,
    next_action: &'static str,
) -> DaemonError {
    DaemonError::StructuredRpc {
        rpc_code: rsi_common::rpc::INVALID_PARAMS,
        message: code.into(),
        data: serde_json::json!({ "code": code, "next_action": next_action }),
    }
}

/// A lead whose provider session cannot be resumed; a fresh-successor
/// `retry_lead` is the continuity-preserving route (Issue #670).
pub(crate) fn manager_resume_unavailable() -> DaemonError {
    manager_refusal_with_next_action("manager_v2_resume_unavailable", "retry_lead")
}

/// Reconstruct the monitor's current-turn assistant accumulator from durable
/// events. Refuse on budget exhaustion rather than overlook a split gate report.
/// The caller holds the Store lock; lifecycle dispatch repeats this proof
/// under the session's spawn guard before provider effects.
pub(crate) fn check_manager_action_program_gate(
    store: &crate::store::Store,
    target: Uuid,
    allow_interrupted_resume: bool,
) -> Result<()> {
    // K2 (#390): an explicit manager retirement supersedes exactly the
    // evidence it witnessed; any later output or re-registration is evaluated.
    if store.manager_lead_program_outcome_superseded(target)? {
        return Ok(());
    }
    let interrupted_resume = allow_interrupted_resume
        && store
            .get_session(target)?
            .is_some_and(|s| s.status == SessionStatus::Interrupted);
    check_manager_program_gate(store, target, false, interrupted_resume)
}

/// Classify the lead's current program evidence for a retirement witness.
/// Store errors propagate; only the two typed gate classes are recorded.
pub(crate) fn classify_manager_program_evidence(
    store: &crate::store::Store,
    target: Uuid,
) -> Result<&'static str> {
    match check_manager_program_gate(store, target, false, false) {
        Ok(()) => Ok("none"),
        Err(error) => {
            let message = error.to_string();
            if message.contains("manager_v2_program_evidence_unknown") {
                Ok("program_evidence_unknown")
            } else if message.contains("manager_v2_human_or_recovery_owner") {
                Ok("human_or_recovery_owner")
            } else {
                Err(error)
            }
        }
    }
}

pub fn check_manager_notice_program_gate(store: &crate::store::Store, target: Uuid) -> Result<()> {
    check_manager_program_gate(store, target, true, false)
}

pub fn check_manager_program_gate(
    store: &crate::store::Store,
    target: Uuid,
    allow_child_watch: bool,
    allow_interrupted_resume: bool,
) -> Result<()> {
    use rsi_common::agent_contract::{
        ContractError, OrchestrationContinuationStateV1, ProgramContinuationIntentV1,
        parse_orchestration_outcome_v1, program_continuation_intent_v1_with_registration,
    };

    const MAX_EVENTS: usize = 256;
    const MAX_BYTES: usize = 256 * 1024;
    let evidence_unknown = || {
        if allow_child_watch {
            manager_notice_deferred()
        } else {
            DaemonError::InvalidParam("manager_v2_program_evidence_unknown".into())
        }
    };
    let recovery_owned = || {
        if allow_child_watch {
            manager_notice_deferred()
        } else {
            DaemonError::InvalidParam("manager_v2_human_or_recovery_owner".into())
        }
    };
    let sentinel_id = deterministic_program_guard_job_id(target);
    let sentinel = store.get_scheduled_job(&sentinel_id)?;
    if sentinel
        .as_ref()
        .is_some_and(|job| !is_program_guard_sentinel(job, target))
        || (sentinel.is_none() && store.scheduled_job_exists(&sentinel_id)?)
    {
        return Err(evidence_unknown());
    }
    let registered = sentinel.as_ref().is_some_and(|job| job.enabled);
    let last_user: Option<i64> = store.conn.query_row(
        "SELECT MAX(sequence) FROM conversation_events WHERE session_id=?1 AND role='User'",
        [target.to_string()],
        |row| row.get(0),
    )?;
    let mut stmt = store.conn.prepare(
        "SELECT substr(content,1,?2), length(CAST(content AS BLOB))
         FROM conversation_events
         WHERE session_id=?1 AND role='Assistant'
           AND sequence > ?4
         ORDER BY sequence LIMIT ?3",
    )?;
    let mut rows = stmt.query(rusqlite::params![
        target.to_string(),
        MAX_BYTES + 1,
        MAX_EVENTS + 1,
        last_user.unwrap_or(-1),
    ])?;
    let mut output = String::new();
    let mut separated = String::new();
    let mut count = 0;
    while let Some(row) = rows.next()? {
        count += 1;
        let bytes: usize = row.get(1)?;
        if count > MAX_EVENTS || bytes > MAX_BYTES.saturating_sub(output.len()) {
            return Err(evidence_unknown());
        }
        let text = row.get::<_, String>(0)?;
        // #413: streamed Assistant events may split one carrier mid-line, so
        // the verbatim concatenation stays the primary reading. A second
        // reading puts each event on its own line, so a carrier followed (or
        // preceded) by prose in another event is not glued into trailing
        // characters or a missing key.
        if !separated.is_empty() {
            separated.push('\n');
        }
        separated.push_str(&text);
        output.push_str(&text);
    }
    let mut outcome = parse_orchestration_outcome_v1(&output);
    if outcome.is_err() && separated != output {
        let separated_outcome = parse_orchestration_outcome_v1(&separated);
        if separated_outcome.is_ok() {
            output = separated;
            outcome = separated_outcome;
        }
    }
    if outcome.as_ref().is_ok_and(|outcome| {
        outcome.continuation_state == OrchestrationContinuationStateV1::HumanGate
    }) {
        return Err(recovery_owned());
    }
    // An Interrupted provider need not have emitted a terminal report. An
    // explicit manager Resume may continue its ordinary partial text, after a
    // known User boundary, without pretending malformed report fragments are
    // a valid outcome. No prose is interpreted as completion or permission.
    if registered
        && allow_interrupted_resume
        && last_user.is_some()
        && matches!(outcome, Err(ContractError::MissingField { ref field }) if field == "orchestration_outcome_v1")
        && program_continuation_intent_v1_with_registration(&output, false)
            == ProgramContinuationIntentV1::NotProgram
        && ordinary_interrupted_program_text(&output)
    {
        return if manager_program_continuation_job_enabled(store, target, sentinel_id)? {
            Err(recovery_owned())
        } else {
            Ok(())
        };
    }
    match program_continuation_intent_v1_with_registration(&output, registered) {
        // All remaining missing or malformed evidence stays unknown. Terminal
        // settlement retains responsibility for invalid terminal output.
        ProgramContinuationIntentV1::InvalidProgram(_)
        | ProgramContinuationIntentV1::RequireAnyGuard => Err(evidence_unknown()),
        intent @ (ProgramContinuationIntentV1::RequireChildWatch { job_id }
        | ProgramContinuationIntentV1::RequireResumeWake { job_id })
            if !allow_child_watch =>
        {
            if !registered || job_id == sentinel_id || last_user.is_none() {
                return Err(evidence_unknown());
            }
            // Use the terminal-settlement validator for the declared owner.
            // Interrupted turns do not run that settlement, so a declaration
            // alone cannot prove they have a recovery path.
            if exact_master_continuation_guard_present(store, target, sentinel_id, &intent)? {
                return Err(recovery_owned());
            }
            if !allow_interrupted_resume {
                return Err(recovery_owned());
            }
            // A different enabled job (including a pending child-watch
            // delivery) still owns continuation. Inspect raw rows so an
            // unreadable job cannot disappear through tolerant hydration.
            if manager_program_continuation_job_enabled(store, target, sentinel_id)? {
                return Err(recovery_owned());
            }
            // Only proven absence or a well-formed disabled continuation is
            // stranded. Wrong mode/recipient or unreadable rows stay unknown.
            match store.get_scheduled_job(&job_id)? {
                Some(job) => {
                    let expected_shape = match intent {
                        ProgramContinuationIntentV1::RequireChildWatch { .. } => {
                            matches!(job.wake_mode, WakeMode::OnTerminal(watched) if watched != target)
                        }
                        ProgramContinuationIntentV1::RequireResumeWake { .. } => {
                            job.wake_mode == WakeMode::Resume
                                && matches!(job.schedule.recurrence, Recurrence::Once)
                        }
                        _ => unreachable!("only exact continuation intents enter this gate"),
                    };
                    if job.enabled || job.wake_session_id != Some(target) || !expected_shape {
                        return Err(evidence_unknown());
                    }
                }
                None if store.scheduled_job_exists(&job_id)? => return Err(evidence_unknown()),
                None => {}
            }
            // Existing action claims, runtime retry/capacity/human gates and
            // the spawn guard own the subsequent same-session Resume. This
            // read-only proof neither schedules a wake nor resets a budget.
            Ok(())
        }
        ProgramContinuationIntentV1::RequireResumeWake { .. } => Err(manager_notice_deferred()),
        ProgramContinuationIntentV1::RequireChildWatch { .. }
        | ProgramContinuationIntentV1::NotProgram
        | ProgramContinuationIntentV1::TerminalAllowed => Ok(()),
    }
}

fn manager_program_continuation_job_enabled(
    store: &crate::store::Store,
    target: Uuid,
    sentinel: Uuid,
) -> Result<bool> {
    Ok(store.conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM scheduled_jobs
         WHERE enabled=1 AND wake_session_id=?1 AND id<>?2)",
        rusqlite::params![target.to_string(), sentinel.to_string()],
        |row| row.get(0),
    )?)
}

/// Conservatively distinguish ordinary interrupted prose from an attempted
/// report or question. Ambiguous syntax remains unknown; this is not a parser
/// fallback and never manufactures an orchestration outcome.
fn ordinary_interrupted_program_text(output: &str) -> bool {
    let text = output.trim();
    let lower = text.to_ascii_lowercase();
    !text.is_empty()
        && !text.contains(['{', '}', '[', ']', '`', ':', '?'])
        // The legacy ORCHESTRATION COMPLETE heading and Mode field can be
        // interrupted before any colon is emitted. Look through Markdown
        // presentation only to refuse known report tokens, never to parse an
        // outcome. Unbalanced delimiters still leave the evidence unknown.
        && !lower.lines().any(|line| {
            let line = line.replace(['*', '_', '~'], "");
            let mut words = line
                .split(|c: char| c.is_whitespace() || c == '|')
                .map(|word| word.trim_start_matches(['#', '>']))
                .filter(|word| !word.is_empty());
            // Skip nested quote/heading/list prefixes and table cell borders.
            let first = words.find(|word| {
                !matches!(*word, "-" | "+")
                    && !word.strip_suffix(['.', ')']).is_some_and(|number| {
                        !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())
                    })
            });
            match first {
                Some("mode") => true,
                Some("orchestration") => words
                    .next()
                    .is_none_or(|word| "complete".starts_with(word)),
                _ => false,
            }
        })
        && ![
            "orchestration_outcome",
            "continuation_",
            "continuation state",
            "blocker_",
            "human_gate",
            "human gate",
            "queue_exhausted",
            "next_slice",
            "next-slice",
            "next slice",
            "pipeline handoff",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
}

pub fn exact_master_continuation_guard_present(
    store: &crate::store::Store,
    session_id: Uuid,
    program_guard_id: Uuid,
    intent: &ProgramContinuationIntentV1,
) -> Result<bool> {
    let is_one_shot_resume = |job: &ScheduledJob| {
        job.id != program_guard_id
            && job.enabled
            && job.wake_session_id == Some(session_id)
            && job.wake_mode == WakeMode::Resume
            && matches!(&job.schedule.recurrence, Recurrence::Once)
    };
    let present = match intent {
        ProgramContinuationIntentV1::RequireChildWatch { job_id } => {
            let consumed_at = store.last_provider_output_at(session_id)?;
            store.get_scheduled_job(job_id)?.is_some_and(|job| {
                // The declared watch may still be enabled while its delivered
                // owner turn settles. Provider output after that delivery is
                // proof the watch was consumed; a later scheduler tick will
                // retire it, so it cannot own the next program continuation.
                let already_consumed = job
                    .last_fired_at
                    .zip(consumed_at)
                    .is_some_and(|(delivered, produced)| produced > delivered);
                job.enabled
                    && job.wake_session_id == Some(session_id)
                    && matches!(job.wake_mode, WakeMode::OnTerminal(_))
                    && !already_consumed
            })
        }
        ProgramContinuationIntentV1::RequireResumeWake { job_id } => store
            .get_scheduled_job(job_id)?
            .is_some_and(|job| is_one_shot_resume(&job)),
        // Legacy reports do not carry an exact durable row identity. Recover
        // conservatively instead of scanning enabled historical jobs.
        ProgramContinuationIntentV1::RequireAnyGuard => false,
        ProgramContinuationIntentV1::InvalidProgram(_) => false,
        ProgramContinuationIntentV1::NotProgram | ProgramContinuationIntentV1::TerminalAllowed => {
            true
        }
    };
    Ok(present)
}
