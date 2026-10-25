//! The operator intake agent ("dream catcher", #3787): a short-lived seat on a
//! small model that turns the operator's requests into triaged Issues so the
//! project manager is no longer the operator's Issue scribe.
//!
//! "Dream" already names the memory subsystem, so the seat is the *intake*
//! agent in code and UI; "dream catcher" is only a display nickname.
//!
//! This module is the policy, with no I/O, so the daemon, the TUI and the tests
//! share one definition:
//!
//! - **Grant boundary** ([`intake_seat_may_call`]): the seat may read Issues to
//!   dedupe, file Issues and read its own catalog and status. It can never
//!   launch workers, message anyone, run jobs or land: every other `Agent*`
//!   verb is refused. The one mail it can cause is the P1 manager notice, which
//!   the daemon raises itself when the seat files a P1 Issue, so the seat has
//!   no send verb at all.
//! - **Triage rubric** ([`PRIORITY_RUBRIC`], [`normalize_intake_filing`]): the
//!   `operator-request` label is always present, a priority is required, and a
//!   `held:<reason>` label marks an item that waits on something physical.
//! - **Mail-only-on-P1** ([`intake_notifies_manager`]): priority 1, or the
//!   explicit [`NOTIFY_MANAGER_LABEL`], produces exactly one manager notice
//!   naming the Issue; priorities 2 to 4 produce none and simply appear in the
//!   manager's normal Issue scan.

use crate::agent_control_schema::AgentControlVerbV1;

/// `Session.agent_role` of an intake seat. The operator-only launch RPC sets
/// it; a session with this role is bound to [`INTAKE_SEAT_AGENT_VERBS`].
pub const INTAKE_AGENT_ROLE: &str = "intake";

/// Display nickname only; never used as an identifier.
pub const INTAKE_NICKNAME: &str = "dream catcher";

/// Label every intake-filed Issue carries.
pub const OPERATOR_REQUEST_LABEL: &str = "operator-request";

/// Label that asks for a manager notice even below P1 ("tell the manager now").
pub const NOTIFY_MANAGER_LABEL: &str = "notify-manager";

/// Prefix of the labels for items that wait on something physical (a device).
pub const HELD_LABEL_PREFIX: &str = "held:";

/// Longest reason after [`HELD_LABEL_PREFIX`].
pub const HELD_REASON_MAX_BYTES: usize = 48;

/// Most labels one intake filing may carry.
pub const MAX_INTAKE_LABELS: usize = 8;

/// Model alias the operator entry point launches the seat on: the cheapest
/// class the launch policy allows. The launch RPC resolves it against the
/// operator's allowed launches and refuses a launch that is not allowed.
pub const INTAKE_DEFAULT_MODEL: &str = "haiku";

/// The only `Agent*` verbs an intake seat may call. Deliberately small: Issue
/// reads for the duplicate check, Issue filing, and the seat's own manual and
/// status. Anything that launches, mails, runs a job, lands or edits another
/// session is absent, and a test pins that.
pub const INTAKE_SEAT_AGENT_VERBS: &[&str] = &[
    "AgentGetAuthorityCatalog",
    "AgentGetStatus",
    "AgentCreateIssue",
    "AgentListIssues",
    "AgentGetIssue",
];

/// Whether an intake seat may call `method`. Only catalogued `Agent*` verbs are
/// gated here: the session's own tool-boundary hook and finite reads are
/// separate audiences with their own scoping.
#[must_use]
pub fn intake_seat_may_call(method: &str) -> bool {
    AgentControlVerbV1::from_method_name(method).is_none()
        || INTAKE_SEAT_AGENT_VERBS.contains(&method)
}

/// The priority rubric the intake prompt teaches, one line per level.
pub const PRIORITY_RUBRIC: &[(u8, &str)] = &[
    (
        1,
        "P1: blocks the operator, an outage or data loss; mails the manager once",
    ),
    (
        2,
        "P2: an operator request with a clear outcome; no mail, the manager's Issue scan finds it",
    ),
    (3, "P3: an idea or normal improvement; no mail"),
    (4, "P4: polish or nice to have; no mail"),
];

/// Priority used when the operator did not signal one.
pub const DEFAULT_INTAKE_PRIORITY: u8 = 3;

/// A refusal the daemon returns to the seat; each code names the fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntakeRefusal {
    /// Priority must be 1 to 4 and is required.
    PriorityRequired,
    PriorityOutOfRange(i64),
    /// `held:` needs a short `[a-z0-9-]` reason.
    HeldLabelMalformed(String),
    TooManyLabels,
}

impl IntakeRefusal {
    /// Stable code for the RPC error.
    #[must_use]
    pub fn code(&self) -> String {
        match self {
            Self::PriorityRequired => "intake_priority_required".into(),
            Self::PriorityOutOfRange(p) => format!("intake_priority_out_of_range:{p}"),
            Self::HeldLabelMalformed(label) => format!("intake_held_label_malformed:{label}"),
            Self::TooManyLabels => "intake_too_many_labels".into(),
        }
    }
}

/// A triaged filing after the daemon's normalization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntakeFiling {
    pub priority: u8,
    /// `operator-request` first, then the seat's labels, deduplicated.
    pub labels: Vec<String>,
}

fn held_label_valid(label: &str) -> bool {
    let Some(reason) = label.strip_prefix(HELD_LABEL_PREFIX) else {
        return true;
    };
    !reason.is_empty()
        && reason.len() <= HELD_REASON_MAX_BYTES
        && reason
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Normalize what an intake seat asked to file. The result always carries
/// [`OPERATOR_REQUEST_LABEL`], a priority in 1 to 4, and only well-formed
/// `held:<reason>` labels.
///
/// # Errors
/// A missing or out-of-range priority, a malformed `held:` label, or more than
/// [`MAX_INTAKE_LABELS`] labels.
pub fn normalize_intake_filing(
    priority: Option<i64>,
    labels: &[String],
) -> Result<IntakeFiling, IntakeRefusal> {
    let priority = priority.ok_or(IntakeRefusal::PriorityRequired)?;
    if !(1..=4).contains(&priority) {
        return Err(IntakeRefusal::PriorityOutOfRange(priority));
    }
    let mut out = vec![OPERATOR_REQUEST_LABEL.to_string()];
    for label in labels {
        let label = label.trim();
        if label.is_empty() {
            continue;
        }
        if !held_label_valid(label) {
            return Err(IntakeRefusal::HeldLabelMalformed(label.to_string()));
        }
        if !out.iter().any(|existing| existing == label) {
            out.push(label.to_string());
        }
    }
    if out.len() > MAX_INTAKE_LABELS {
        return Err(IntakeRefusal::TooManyLabels);
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Ok(IntakeFiling {
        priority: priority as u8,
        labels: out,
    })
}

/// Whether filing this Issue mails the manager: priority 1, or the explicit
/// [`NOTIFY_MANAGER_LABEL`]. Priorities 2 to 4 never do.
#[must_use]
pub fn intake_notifies_manager(filing: &IntakeFiling) -> bool {
    filing.priority == 1 || filing.labels.iter().any(|l| l == NOTIFY_MANAGER_LABEL)
}

/// The system prompt the operator entry point gives the intake seat.
#[must_use]
pub fn intake_seat_query(request: &str) -> String {
    format!("/intake {request}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_control_schema::agent_control_catalog_v1;

    fn labels(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn grant_allows_only_issue_filing_and_reads() {
        for verb in INTAKE_SEAT_AGENT_VERBS {
            assert!(intake_seat_may_call(verb), "{verb}");
            assert!(
                AgentControlVerbV1::from_method_name(verb).is_some(),
                "{verb} must be a catalogued verb"
            );
        }
    }

    #[test]
    fn grant_refuses_every_other_agent_verb() {
        let mut refused = 0;
        for descriptor in agent_control_catalog_v1() {
            if INTAKE_SEAT_AGENT_VERBS.contains(&descriptor.method) {
                continue;
            }
            assert!(
                !intake_seat_may_call(descriptor.method),
                "{} must be outside the intake grant",
                descriptor.method
            );
            refused += 1;
        }
        assert!(
            refused > 20,
            "the catalog has many more verbs than the grant"
        );
    }

    #[test]
    fn grant_cannot_launch_land_mail_or_edit() {
        for verb in [
            "AgentSpawnChild",
            "AgentManagerLaunchIssueWorker",
            "AgentSubmitJob",
            "AgentEnqueueLandingSource",
            "AgentSendMessage",
            "AgentManagerNotify",
            "AgentManagerSend",
            "AgentUpdateIssue",
            "AgentContinueChild",
            "AgentHalt",
            "AgentScheduleWake",
        ] {
            assert!(!intake_seat_may_call(verb), "{verb}");
        }
    }

    #[test]
    fn grant_leaves_non_agent_audiences_to_their_own_scoping() {
        assert!(intake_seat_may_call("ListSessions"));
    }

    #[test]
    fn filing_always_carries_operator_request_first() {
        let filing = normalize_intake_filing(Some(3), &labels(&["tui"])).unwrap();
        assert_eq!(filing.labels, labels(&["operator-request", "tui"]));
        let again =
            normalize_intake_filing(Some(3), &labels(&["operator-request", "tui"])).unwrap();
        assert_eq!(again, filing);
    }

    #[test]
    fn priority_is_required_and_bounded() {
        assert_eq!(
            normalize_intake_filing(None, &[]),
            Err(IntakeRefusal::PriorityRequired)
        );
        assert_eq!(
            normalize_intake_filing(Some(0), &[]),
            Err(IntakeRefusal::PriorityOutOfRange(0))
        );
        assert_eq!(
            normalize_intake_filing(Some(5), &[]),
            Err(IntakeRefusal::PriorityOutOfRange(5))
        );
        for p in 1..=4 {
            assert_eq!(
                normalize_intake_filing(Some(p), &[]).unwrap().priority,
                p as u8
            );
        }
    }

    #[test]
    fn held_labels_need_a_short_slug_reason() {
        let ok = normalize_intake_filing(Some(2), &labels(&["held:phone-app"])).unwrap();
        assert!(ok.labels.contains(&"held:phone-app".to_string()));
        for bad in ["held:", "held:Phone App", "held:a/b"] {
            assert!(
                matches!(
                    normalize_intake_filing(Some(2), &labels(&[bad])),
                    Err(IntakeRefusal::HeldLabelMalformed(_))
                ),
                "{bad}"
            );
        }
        let long = format!("held:{}", "a".repeat(HELD_REASON_MAX_BYTES + 1));
        assert!(normalize_intake_filing(Some(2), &[long]).is_err());
    }

    #[test]
    fn too_many_labels_are_refused() {
        let many = (0..MAX_INTAKE_LABELS)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>();
        assert_eq!(
            normalize_intake_filing(Some(3), &many),
            Err(IntakeRefusal::TooManyLabels)
        );
    }

    #[test]
    fn only_p1_or_the_explicit_label_mails_the_manager() {
        let mails = |p: i64, l: &[&str]| {
            intake_notifies_manager(&normalize_intake_filing(Some(p), &labels(l)).unwrap())
        };
        assert!(mails(1, &[]));
        assert!(!mails(2, &[]));
        assert!(!mails(3, &[]));
        assert!(!mails(4, &["held:device"]));
        assert!(mails(3, &[NOTIFY_MANAGER_LABEL]));
    }

    #[test]
    fn rubric_covers_every_priority_once() {
        let levels = PRIORITY_RUBRIC.iter().map(|(p, _)| *p).collect::<Vec<_>>();
        assert_eq!(levels, vec![1, 2, 3, 4]);
        assert!((1..=4).contains(&(DEFAULT_INTAKE_PRIORITY as i64)));
    }

    #[test]
    fn refusal_codes_are_stable() {
        assert_eq!(
            IntakeRefusal::PriorityRequired.code(),
            "intake_priority_required"
        );
        assert_eq!(
            IntakeRefusal::PriorityOutOfRange(9).code(),
            "intake_priority_out_of_range:9"
        );
    }

    #[test]
    fn query_wraps_the_request_in_the_intake_command() {
        assert_eq!(intake_seat_query("add dark mode"), "/intake add dark mode");
    }
}
