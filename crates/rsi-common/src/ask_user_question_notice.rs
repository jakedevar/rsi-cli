//! The typed result a headless Claude session gets for an `AskUserQuestion`
//! that no human can answer (hub #13, hub #1161).
//!
//! One source for the code and the agent-facing text, shared by two paths: the
//! `PreToolUse` hook (`boundary_mail_hook`) hands it to the model as the tool
//! result in the same turn, and `rsid` prefixes it to the next continuation of
//! a session that still holds an unanswered question.

/// Typed code for a Claude `AskUserQuestion` that no human has answered.
pub const UNDELIVERABLE_CODE: &str = "ask_user_question_undeliverable";

/// Agent-facing text: nobody can answer the question here, so the agent decides.
#[must_use]
pub fn undeliverable_notice() -> String {
    format!(
        "[rsi:{UNDELIVERABLE_CODE}] Your AskUserQuestion was NOT declined by a human: the \
         headless CLI auto-rejected it and no human has answered it here. Decide yourself \
         and record the decision (and the assumption it rests on) in your handoff or commit \
         message so the operator can review it."
    )
}
