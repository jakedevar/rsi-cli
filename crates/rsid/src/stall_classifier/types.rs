//! Strict-JSON contract between the daemon and the classifier model.
//!
//! `Verdict` is a closed whitelist: the model cannot invent new verdicts at
//! deserialization time (`#[serde(rename_all = "PascalCase")]` rejects
//! unknown variants), which is R2's prompt-injection defense at the parser
//! layer.
//!
//! `ClassifierVerdict` is the deserialize target for the LLM's response
//! object. `NudgeAction` is the daemon-internal effect type the scheduler
//! routes to the nudge consumer loop.

use rsi_common::types::{SessionKind, SessionProvider, SessionStatus};
use uuid::Uuid;

/// Closed whitelist of classifier verdicts. Any other string at deserialize
/// time produces a serde error and the scheduler logs + skips the verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "PascalCase")]
pub enum Verdict {
    /// Agent reached natural completion. No further action needed.
    Finished,
    /// Agent is waiting for human input (pending_question present, or
    /// awaiting a decision/permission). No automated action.
    NeedsUser,
    /// Agent was mid-work and stopped without resolving. A short "continue"
    /// nudge will get it moving.
    StalledContinue,
    /// Agent appears blocked because a spawned sub-agent or team member went
    /// silent. A nudge to check on sub-agents (TaskList/TaskGet) will help.
    StalledCheckTeam,
}

impl Verdict {
    /// Human-readable lowercase label used in TUI rendering and telemetry.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Finished => "finished",
            Self::NeedsUser => "needs_user",
            Self::StalledContinue => "stalled_continue",
            Self::StalledCheckTeam => "stalled_check_team",
        }
    }
}

/// Wire-format struct the LLM is asked to emit. `nudge_prompt` and
/// `reasoning` are optional in serde (model may omit them on the
/// telemetry-only verdicts).
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ClassifierVerdict {
    pub verdict: Verdict,
    pub confidence: f64,
    #[serde(default)]
    pub nudge_prompt: Option<String>,
    #[serde(default)]
    pub reasoning: String,
}

/// Daemon-side action selected after applying the confidence floor + verdict
/// gating. The scheduler emits either of these to the nudge consumer.
#[derive(Debug, Clone)]
pub enum NudgeAction {
    /// Publish a `DaemonEvent::SessionClassified` only. No subprocess action.
    NotifyOnly { verdict: Verdict },
    /// Internal `SessionManager::continue_session()` call with the
    /// model-authored prompt.
    Continue { verdict: Verdict, prompt: String },
}

impl NudgeAction {
    /// Short telemetry label used in the bus event payload. Kept as a
    /// borrowed `&'static str` so the call site doesn't allocate.
    pub fn label(&self) -> &'static str {
        match self {
            Self::NotifyOnly { .. } => "notify_only",
            Self::Continue { .. } => "continue",
        }
    }
}

/// Read-only snapshot of one child of the session under classification.
/// Built from `Store::list_children` rows; sorted by `created_at` ASC.
#[derive(Debug, Clone)]
pub struct ChildSummary {
    pub session_id: Uuid,
    pub kind: SessionKind,
    pub status: SessionStatus,
    pub idle_secs: u64,
}

/// Composite input to the classifier prompt builder. Constructed by
/// `stall_classifier::input::build_classification_input` in Phase 3.
#[derive(Debug, Clone)]
pub struct ClassificationInput {
    pub session_id: Uuid,
    pub session_kind: SessionKind,
    pub provider: SessionProvider,
    pub idle_secs: u64,
    pub pending_question: Option<String>,
    /// Pre-formatted excerpt (roles, truncated content, newline-separated).
    pub excerpt: String,
    pub children: Vec<ChildSummary>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_roundtrip_all_variants() {
        for v in [
            Verdict::Finished,
            Verdict::NeedsUser,
            Verdict::StalledContinue,
            Verdict::StalledCheckTeam,
        ] {
            let json = serde_json::to_string(&v).expect("serialize");
            let back: Verdict = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(v, back);
        }
    }

    #[test]
    fn verdict_pascal_case_wire_format() {
        let json = serde_json::to_string(&Verdict::StalledCheckTeam).unwrap();
        assert_eq!(json, "\"StalledCheckTeam\"");
    }

    #[test]
    fn classifier_verdict_tolerates_missing_optional_fields() {
        let raw = r#"{"verdict":"Finished","confidence":0.95}"#;
        let v: ClassifierVerdict = serde_json::from_str(raw).expect("parse");
        assert!(matches!(v.verdict, Verdict::Finished));
        assert_eq!(v.confidence, 0.95);
        assert!(v.nudge_prompt.is_none());
        assert_eq!(v.reasoning, "");
    }

    #[test]
    fn classifier_verdict_full_payload() {
        let raw = r#"{
            "verdict":"StalledContinue",
            "confidence":0.82,
            "nudge_prompt":"Please continue your work.",
            "reasoning":"Last assistant message ended mid-task."
        }"#;
        let v: ClassifierVerdict = serde_json::from_str(raw).expect("parse");
        assert!(matches!(v.verdict, Verdict::StalledContinue));
        assert_eq!(v.confidence, 0.82);
        assert_eq!(
            v.nudge_prompt.as_deref(),
            Some("Please continue your work.")
        );
        assert!(v.reasoning.contains("mid-task"));
    }

    #[test]
    fn unknown_verdict_string_rejected() {
        let raw = r#"{"verdict":"Maybe","confidence":0.5}"#;
        let r: Result<ClassifierVerdict, _> = serde_json::from_str(raw);
        assert!(r.is_err());
    }

    #[test]
    fn nudge_action_labels() {
        assert_eq!(
            NudgeAction::NotifyOnly {
                verdict: Verdict::Finished
            }
            .label(),
            "notify_only"
        );
        assert_eq!(
            NudgeAction::Continue {
                verdict: Verdict::StalledContinue,
                prompt: "go".to_string(),
            }
            .label(),
            "continue"
        );
    }

    #[test]
    fn verdict_str_labels() {
        assert_eq!(Verdict::Finished.as_str(), "finished");
        assert_eq!(Verdict::NeedsUser.as_str(), "needs_user");
        assert_eq!(Verdict::StalledContinue.as_str(), "stalled_continue");
        assert_eq!(Verdict::StalledCheckTeam.as_str(), "stalled_check_team");
    }
}
