//! Hub #3345: satellite ENQUEUE intake.
//!
//! A satellite manager reports `ENQUEUE <sha> ...` to the hub (`AgentReportToHub`).
//! The hub daemon, not the hub manager, verifies the commit, probes it against
//! the rolling tip and every queued entry, enqueues it when clean and tells the
//! satellite what became of it. This module is the pure part: the grammar of
//! the report, the conflict classification and the replies.
//!
//! The report text stays free-form and untrusted on the wire (older satellites
//! keep working); the intake acts only on a text that parses unambiguously, and
//! malformed ENQUEUE reports stay plain manager notices. Conflicting hub Issue
//! claims receive a named report refusal.
//!
//! Grammar (whitespace separated, the first word is `ENQUEUE`):
//!
//! ```text
//! ENQUEUE <40-hex sha> [sha=<sha>] [branch=<name>] [issue=#N] [fixes=#N]
//!         [fixes_tests=a::b,c::d] [expedite[=<reason-token>]]
//! ```
//!
//! Claim fields also accept a whitespace separator (`fixes_issue 12`).
//! `hub #N` (as in `laptop #659 = hub #3931`) names the implementation Issue;
//! different implementation Issue numbers are refused rather than guessed.
//! A bare `expedite` (including `Please expedite.`) requests the queue's explicit
//! expedite admission; it must succeed or return the queue's named refusal.
//!
//! The source commit is `sha=` when given, else the one distinct 40-hex word of
//! the line; two different bare SHAs are ambiguous and parse to nothing.

use crate::rolling_queue::{
    QueueExpediteRequestV1, ROLLING_QUEUE_MAX_FIXES_TESTS, valid_test_name,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Longest branch name a claim carries.
pub const INTAKE_BRANCH_MAX_BYTES: usize = 200;
/// Conflicting paths named per conflict in a reply.
pub const INTAKE_CONFLICT_MAX_PATHS: usize = 8;
/// Conflicts named in one reply.
pub const INTAKE_CONFLICT_MAX_ENTRIES: usize = 8;
/// Prefix of the queue idempotency key and the satellite message key.
pub const INTAKE_KEY_PREFIX: &str = "sat-intake";
/// Subject prefix of the manager digest notice (`<peer>:<report>` follows).
pub const INTAKE_NOTICE_SUBJECT_PREFIX: &str = "satellite_intake:";
/// `record_kind` of the digest notice.
pub const INTAKE_NOTICE_RECORD_KIND: &str = "satellite_intake";
/// Attempts the intake makes on a transient failure before it gives up and
/// asks the manager.
pub const INTAKE_MAX_ATTEMPTS: u32 = 6;

/// What an ENQUEUE report asks the hub to land.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SatelliteEnqueueClaimV1 {
    pub source_commit: String,
    pub branch: Option<String>,
    /// The hub Issue the source implements.
    pub issue: Option<i64>,
    /// The red hub Issue the source claims to fix.
    pub fixes_issue: Option<i64>,
    pub fixes_tests: Vec<String>,
    pub expedite: Option<QueueExpediteRequestV1>,
}

fn is_sha(word: &str) -> bool {
    word.len() == 40
        && word
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn valid_branch(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= INTAKE_BRANCH_MAX_BYTES
        && !name.starts_with(['-', '/', '.'])
        && !name.ends_with(['/', '.'])
        && !name.contains("..")
        && !name.contains("//")
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"/._-".contains(&byte))
}

fn issue_number(value: &str) -> Option<i64> {
    let digits = value.strip_prefix('#').unwrap_or(value);
    if digits.is_empty() || digits.len() > 9 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse::<i64>().ok().filter(|number| *number >= 1)
}

/// Named report refusal for conflicting hub implementation Issue claims.
pub const SATELLITE_ENQUEUE_ISSUE_AMBIGUOUS: &str = "satellite_enqueue_issue_ambiguous";

/// Why an ENQUEUE report cannot be taken in automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueClaimParseError {
    /// Leave the report as a plain manager notice, as before.
    Malformed,
    /// Refuse the report by name rather than choosing an implementation Issue.
    ConflictingIssues,
}

/// Parse an ENQUEUE report text. `None` when the text is not an ENQUEUE line,
/// names no unambiguous full SHA, or carries malformed or conflicting fields.
#[must_use]
pub fn parse_enqueue_claim(text: &str) -> Option<SatelliteEnqueueClaimV1> {
    try_parse_enqueue_claim(text).ok()
}

/// Parse claims while retaining the reason report admission must refuse them.
pub fn try_parse_enqueue_claim(
    text: &str,
) -> Result<SatelliteEnqueueClaimV1, EnqueueClaimParseError> {
    use EnqueueClaimParseError::{ConflictingIssues, Malformed};

    let mut words = text.split_whitespace().peekable();
    if words.next() != Some("ENQUEUE") {
        return Err(Malformed);
    }
    let mut explicit: Option<String> = None;
    let mut bare: Vec<String> = Vec::new();
    let mut branch: Option<String> = None;
    let mut issue: Option<i64> = None;
    let mut explicit_issue_seen = false;
    let mut fixes_issue: Option<i64> = None;
    let mut fixes_tests: Option<Vec<String>> = None;
    let mut expedite = None;
    while let Some(word) = words.next() {
        let word = word.trim_matches(|c: char| matches!(c, ',' | ';' | '(' | ')' | '[' | ']'));
        if word.trim_end_matches('.') == "expedite" {
            if expedite
                .replace(QueueExpediteRequestV1 {
                    reason: "satellite ENQUEUE requested expedite".into(),
                })
                .is_some()
            {
                return Err(Malformed);
            }
            continue;
        }
        let field = if matches!(word, "issue" | "fixes" | "fixes_issue" | "fixes_tests")
            || (word == "hub" && words.peek().is_some_and(|value| value.starts_with('#')))
        {
            Some((word, words.next().ok_or(Malformed)?))
        } else {
            word.split_once('=')
        };
        if let Some((key, value)) = field {
            let value = value.trim_end_matches([',', ';']);
            let taken = match key {
                "sha" => {
                    if !is_sha(value) {
                        return Err(Malformed);
                    }
                    explicit.replace(value.to_owned()).is_some()
                }
                "branch" => {
                    if !valid_branch(value) {
                        return Err(Malformed);
                    }
                    branch.replace(value.to_owned()).is_some()
                }
                "issue" | "hub" => {
                    // The hub reference often closes a parenthesized satellite
                    // mapping; leave existing claim-field punctuation unchanged.
                    let value = if key == "hub" {
                        value.trim_end_matches([',', ';', ')', ']', '.'])
                    } else {
                        value
                    };
                    let number = issue_number(value).ok_or(Malformed)?;
                    if issue.is_some_and(|seen| seen != number) {
                        return Err(ConflictingIssues);
                    }
                    issue = Some(number);
                    if key == "issue" {
                        std::mem::replace(&mut explicit_issue_seen, true)
                    } else {
                        false
                    }
                }
                "fixes" | "fixes_issue" => fixes_issue
                    .replace(issue_number(value).ok_or(Malformed)?)
                    .is_some(),
                "fixes_tests" => {
                    let names: Vec<String> = value
                        .split(',')
                        .filter(|name| !name.is_empty())
                        .map(str::to_owned)
                        .collect();
                    if names.is_empty()
                        || names.len() > ROLLING_QUEUE_MAX_FIXES_TESTS
                        || names.iter().any(|name| !valid_test_name(name))
                    {
                        return Err(Malformed);
                    }
                    fixes_tests.replace(names).is_some()
                }
                "expedite" => {
                    let request = QueueExpediteRequestV1 {
                        reason: value.to_owned(),
                    };
                    request.validate().map_err(|_| Malformed)?;
                    expedite.replace(request).is_some()
                }
                // Unknown keys are free-form commentary (`status=green`).
                _ => false,
            };
            if taken {
                return Err(Malformed);
            }
        } else if is_sha(word) && !bare.iter().any(|seen| seen == word) {
            bare.push(word.to_owned());
        }
    }
    let source_commit = match (explicit, bare.as_slice()) {
        (Some(sha), _) => sha,
        (None, [only]) => only.clone(),
        _ => return Err(Malformed),
    };
    Ok(SatelliteEnqueueClaimV1 {
        source_commit,
        branch,
        issue,
        fixes_issue,
        fixes_tests: fixes_tests.unwrap_or_default(),
        expedite,
    })
}

/// Where an intake is. Mirrors the CHECK of `satellite_enqueue_intakes.state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntakeStateV1 {
    /// Recorded with the report; the daemon has not decided yet.
    Received,
    /// On the rolling queue; the satellite hears when it settles.
    Enqueued,
    /// Not enqueued: it conflicts with rolling or a queued entry. The
    /// satellite was told, and resends after merging.
    Conflicts,
    /// Not enqueued, and the satellite was told why.
    Refused,
    /// Not enqueued: AGENTS.md reserves a pre-merge review for it, so the
    /// hub manager decides.
    NeedsReview,
    /// The entry settled (or the source was already on rolling) and the
    /// satellite was told.
    Settled,
}

impl IntakeStateV1 {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Received => "received",
            Self::Enqueued => "enqueued",
            Self::Conflicts => "conflicts",
            Self::Refused => "refused",
            Self::NeedsReview => "needs_review",
            Self::Settled => "settled",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "received" => Self::Received,
            "enqueued" => Self::Enqueued,
            "conflicts" => Self::Conflicts,
            "refused" => Self::Refused,
            "needs_review" => Self::NeedsReview,
            "settled" => Self::Settled,
            _ => return None,
        })
    }
}

/// One conflict found by the probe (`conflicts_with` in the reply).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntakeConflictV1 {
    /// `rolling` or `entry`.
    pub with: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<Uuid>,
    /// The queued entry's branch, when the hub knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_commit: Option<String>,
    pub paths: Vec<String>,
    /// Only schema-migration bookkeeping conflicts, and the source declares a
    /// provisional migration: the lander renumbers it, so it is no blocker.
    #[serde(default)]
    pub renumberable: bool,
}

const MIGRATION_PREFIXES: [&str; 3] = [
    "crates/rsid-store/src/store/migrations/",
    "crates/rsid/src/store/migrations/",
    "tools/provisional-migrations/",
];
const MIGRATION_FILES: [&str; 2] = [
    "tools/released-migrations.json",
    "crates/rsid-store/src/store/migrations.rs",
];

/// Whether `path` is schema-migration bookkeeping the lander renumbers for a
/// source that declares a provisional migration.
#[must_use]
pub fn is_migration_bookkeeping_path(path: &str) -> bool {
    MIGRATION_FILES.contains(&path)
        || MIGRATION_PREFIXES
            .iter()
            .any(|prefix| path.starts_with(prefix))
}

/// A conflict is renumberable (#3336) when it names paths, every one of them
/// migration bookkeeping, and the source declares a provisional migration.
#[must_use]
pub fn conflict_is_renumberable(paths: &[String], source_declares_provisional: bool) -> bool {
    source_declares_provisional
        && !paths.is_empty()
        && paths.iter().all(|path| is_migration_bookkeeping_path(path))
}

fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

fn conflict_lines(conflicts: &[IntakeConflictV1]) -> String {
    let mut out = String::new();
    for conflict in conflicts.iter().take(INTAKE_CONFLICT_MAX_ENTRIES) {
        out.push_str("conflicts_with: ");
        out.push_str(&conflict.with);
        if let Some(branch) = &conflict.branch {
            out.push_str(&format!(" branch={branch}"));
        }
        if let Some(commit) = &conflict.entry_commit {
            out.push_str(&format!(" commit={}", short(commit)));
        }
        if let Some(entry) = conflict.entry_id {
            out.push_str(&format!(" entry={entry}"));
        }
        let shown: Vec<&str> = conflict
            .paths
            .iter()
            .take(INTAKE_CONFLICT_MAX_PATHS)
            .map(String::as_str)
            .collect();
        out.push_str(&format!(" paths={}", shown.join(",")));
        if conflict.paths.len() > INTAKE_CONFLICT_MAX_PATHS {
            out.push_str(&format!(
                " (+{} more)",
                conflict.paths.len() - INTAKE_CONFLICT_MAX_PATHS
            ));
        }
        out.push('\n');
    }
    out
}

/// The reply for a source that conflicts: merge, then resend.
#[must_use]
pub fn conflicts_reply(source: &str, issue: Option<i64>, conflicts: &[IntakeConflictV1]) -> String {
    let mut text =
        format!("HUB INTAKE conflicts: {source} was NOT enqueued (hub #3345).\nsource: {source}\n");
    if let Some(issue) = issue {
        text.push_str(&format!("issue: #{issue}\n"));
    }
    text.push_str(&conflict_lines(conflicts));
    text.push_str(
        "next: merge origin/rolling (and the named entry's branch when it is a queued sibling; \
         never rebase), push, then send a new ENQUEUE report for the new head. \
         Nothing is queued for this report.",
    );
    text
}

/// The reply for a source the daemon refused to enqueue or could not fetch.
#[must_use]
pub fn refused_reply(source: &str, issue: Option<i64>, code: &str, next: &str) -> String {
    let mut text =
        format!("HUB INTAKE refused: {source} was NOT enqueued (hub #3345).\nsource: {source}\n");
    if let Some(issue) = issue {
        text.push_str(&format!("issue: #{issue}\n"));
    }
    text.push_str(&format!("code: {code}\nnext: {next}"));
    text
}

/// The reply for a source held for the hub manager's pre-merge review.
#[must_use]
pub fn needs_review_reply(source: &str, issue: Option<i64>, why: &str) -> String {
    let mut text =
        format!("HUB INTAKE held: {source} was NOT enqueued (hub #3345).\nsource: {source}\n");
    if let Some(issue) = issue {
        text.push_str(&format!("issue: #{issue}\n"));
    }
    text.push_str(&format!(
        "why: {why}\nnext: the hub manager reviews it before it is queued; you hear when it is enqueued or refused. Do not resend."
    ));
    text
}

/// The reply once the source is on the queue.
#[must_use]
pub fn enqueued_reply(source: &str, issue: Option<i64>, entry: Uuid, sequence: i64) -> String {
    let mut text = format!(
        "HUB INTAKE enqueued: {source} is on the hub merge queue (hub #3345).\nsource: {source}\n"
    );
    if let Some(issue) = issue {
        text.push_str(&format!("issue: #{issue}\n"));
    }
    text.push_str(&format!(
        "entry: {entry}\nsequence: {sequence}\nnext: you are told again when it publishes or is refused."
    ));
    text
}

/// The reply when the queue entry settled.
#[must_use]
pub fn settled_reply(
    source: &str,
    issue: Option<i64>,
    outcome: &str,
    landed_sha: Option<&str>,
    refusal: Option<&str>,
    failing_tests: &[String],
    detail: Option<&str>,
) -> String {
    let mut text = format!("HUB INTAKE {outcome}: {source} (hub #3345).\nsource: {source}\n");
    if let Some(issue) = issue {
        text.push_str(&format!("issue: #{issue}\n"));
    }
    if let Some(landed) = landed_sha {
        text.push_str(&format!(
            "PUBLISHED: rolling now contains {source}; landed tip {landed}. Fetch origin/rolling and continue from it.\n"
        ));
    }
    if let Some(refusal) = refusal {
        text.push_str(&format!("code: {refusal}\n"));
    }
    for test in failing_tests.iter().take(8) {
        text.push_str(&format!("failing_test: {test}\n"));
    }
    if let Some(detail) = detail {
        let tail: String = detail.chars().take(600).collect();
        text.push_str(&format!("detail: {tail}\n"));
    }
    if landed_sha.is_none() {
        text.push_str(
            "next: fix the cause, merge origin/rolling (never rebase) and send a new ENQUEUE report for the new head.",
        );
    }
    text.trim_end().to_owned()
}

/// Stable idempotency key shared by the queue entry and each reply.
#[must_use]
pub fn intake_key(peer: Uuid, report: Uuid, phase: &str) -> String {
    format!("{INTAKE_KEY_PREFIX}:{peer}:{report}:{phase}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
    const OTHER: &str = "fedcba9876543210fedcba9876543210fedcba98";

    #[test]
    fn a_plain_enqueue_line_names_its_source_and_claims() {
        let claim = parse_enqueue_claim(&format!(
            "ENQUEUE {SHA} issue=#3345 branch=rsi/abc-def fixes=#12 fixes_tests=a::b,c::d status=green"
        ))
        .unwrap();
        assert_eq!(claim.source_commit, SHA);
        assert_eq!(claim.branch.as_deref(), Some("rsi/abc-def"));
        assert_eq!(claim.issue, Some(3345));
        assert_eq!(claim.fixes_issue, Some(12));
        assert_eq!(claim.fixes_tests, vec!["a::b", "c::d"]);
    }

    #[test]
    fn an_explicit_sha_wins_over_other_shas_in_the_line() {
        let claim =
            parse_enqueue_claim(&format!("ENQUEUE sha={SHA} (base {OTHER}) issue=7")).unwrap();
        assert_eq!(claim.source_commit, SHA);
        assert_eq!(claim.issue, Some(7));
    }

    #[test]
    fn whitespace_claims_and_an_expedite_request_are_preserved_3958() {
        let claim = parse_enqueue_claim(&format!(
            "ENQUEUE {SHA} issue #3931 fixes_issue 3953 (qa red) fixes_tests a::b,c::d Please expedite."
        )).unwrap();
        assert_eq!(claim.issue, Some(3931));
        assert_eq!(claim.fixes_issue, Some(3953));
        assert_eq!(claim.fixes_tests, ["a::b", "c::d"]);
        assert_eq!(
            claim.expedite.unwrap().reason,
            "satellite ENQUEUE requested expedite"
        );
        let explicit = parse_enqueue_claim(&format!("ENQUEUE {SHA} expedite=qa-repair")).unwrap();
        assert_eq!(explicit.expedite.unwrap().reason, "qa-repair");
    }

    #[test]
    fn historical_enqueue_report_retains_fix_and_priority_3958() {
        // Read-only replay of report 57a525b4-fee7-423d-b516-681c8ada57b0
        // (hub notice sequence 64095), the production input cited by #3958.
        let text = concat!(
            "ENQUEUE 1c686a76ba04e55f733df36b96523d075b26592d laptop #659 = hub #3931 ",
            "fixes_issue 3953 (qa red). Snapshot-only: 3 insta .snap lines for crates/rsi ",
            "overlay::manager_v2::tests::layout (gate-timeout policy). Exact job c0bbf2a6 ",
            "green 3/3 at this head (screenshot_state_layout_at_120/200, policy_provider_grant). ",
            "scoped_test cc0d1aa5 red only from unrelated desktop/TUI E2E baseline reds + ",
            "network error and it omitted the 3 layout tests (laptop kaizen #662 #663 #664). ",
            "Branch rsi/ebac32bb-7b65-406a-836f-5a4f35b91010 pushed. Please expedite."
        );
        let claim = parse_enqueue_claim(text).unwrap();
        assert_eq!(
            claim.source_commit,
            "1c686a76ba04e55f733df36b96523d075b26592d"
        );
        assert_eq!(claim.issue, Some(3931));
        assert_eq!(claim.fixes_issue, Some(3953));
        assert_eq!(
            claim.expedite.unwrap().reason,
            "satellite ENQUEUE requested expedite"
        );
    }

    #[test]
    fn hub_issue_notation_preserves_the_implementation_issue_3984() {
        for fields in [
            "laptop #659 = hub #3931",
            "mac #615 = hub #3931; issue=#3931",
            "issue #3931 (laptop #659 = hub #3931)",
            "hub #3931 hub #3931",
            "[laptop #659 = hub #3931]",
            "laptop #659 = hub #3931.",
            "hub=#3931 hub commentary",
        ] {
            let claim = try_parse_enqueue_claim(&format!("ENQUEUE {SHA} {fields}"))
                .unwrap_or_else(|error| panic!("{fields}: {error:?}"));
            assert_eq!(claim.issue, Some(3931), "{fields}");
        }
        let prose = try_parse_enqueue_claim(&format!("ENQUEUE {SHA} hub tests green")).unwrap();
        assert_eq!(prose.issue, None);
    }

    #[test]
    fn conflicting_hub_issue_claims_have_a_named_parse_error_3984() {
        for fields in [
            "laptop #659 = hub #3931 mac #615 = hub #3932",
            "issue=#3931 hub #3932",
            "hub #3931 issue #3932",
            "issue=#3931 issue=#3932",
        ] {
            assert_eq!(
                try_parse_enqueue_claim(&format!("ENQUEUE {SHA} {fields}")),
                Err(EnqueueClaimParseError::ConflictingIssues),
                "{fields}"
            );
        }
    }

    #[test]
    fn an_ambiguous_or_malformed_line_is_not_acted_on() {
        for text in [
            format!("ENQUEUE {SHA} {OTHER}"),
            "ENQUEUE deadbeef issue=#1".to_owned(),
            format!("DEPLOY-READY {SHA}"),
            format!("RESULT ENQUEUE {SHA}"),
            format!("ENQUEUE {SHA} issue=#0"),
            format!("ENQUEUE {SHA} issue=#1 issue=#2"),
            format!("ENQUEUE {SHA} issue=#1 issue=#1"),
            format!("ENQUEUE {SHA} branch=../x"),
            format!("ENQUEUE {SHA} fixes_tests=ok,-bad"),
            format!("ENQUEUE {SHA} fixes_issue"),
            format!("ENQUEUE {SHA} fixes_issue invalid"),
            format!("ENQUEUE {SHA} expedite="),
            format!("ENQUEUE {SHA} expedite expedite=again"),
            format!("ENQUEUE sha={}", SHA.to_uppercase()),
            String::new(),
        ] {
            assert_eq!(parse_enqueue_claim(&text), None, "{text:?}");
        }
    }

    #[test]
    fn the_same_sha_twice_is_one_source() {
        assert!(parse_enqueue_claim(&format!("ENQUEUE {SHA} again {SHA}")).is_some());
    }

    #[test]
    fn only_migration_bookkeeping_with_a_declaration_is_renumberable() {
        let paths = vec![
            "crates/rsid-store/src/store/migrations/v100.rs".to_owned(),
            "tools/released-migrations.json".to_owned(),
        ];
        assert!(conflict_is_renumberable(&paths, true));
        assert!(!conflict_is_renumberable(&paths, false));
        assert!(!conflict_is_renumberable(&[], true));
        let mixed = vec![
            "tools/released-migrations.json".to_owned(),
            "crates/rsid/src/lib.rs".to_owned(),
        ];
        assert!(!conflict_is_renumberable(&mixed, true));
    }

    #[test]
    fn states_round_trip() {
        for state in [
            IntakeStateV1::Received,
            IntakeStateV1::Enqueued,
            IntakeStateV1::Conflicts,
            IntakeStateV1::Refused,
            IntakeStateV1::NeedsReview,
            IntakeStateV1::Settled,
        ] {
            assert_eq!(IntakeStateV1::parse(state.as_str()), Some(state));
        }
        assert_eq!(IntakeStateV1::parse("other"), None);
    }

    #[test]
    fn replies_name_the_typed_facts_and_stay_small() {
        let conflicts = vec![IntakeConflictV1 {
            with: "entry".into(),
            entry_id: Some(Uuid::nil()),
            branch: Some("rsi/x".into()),
            entry_commit: Some(OTHER.into()),
            paths: (0..20).map(|n| format!("src/f{n}.rs")).collect(),
            renumberable: false,
        }];
        let text = conflicts_reply(SHA, Some(5), &conflicts);
        assert!(text.contains("conflicts_with: entry branch=rsi/x"));
        assert!(text.contains("(+12 more)"));
        assert!(text.contains("issue: #5"));
        assert!(text.len() < crate::satellite_dispatch::SATELLITE_MESSAGE_MAX_BYTES);
        let done = settled_reply(SHA, None, "published", Some(OTHER), None, &[], None);
        assert!(done.contains("PUBLISHED"));
        let red = settled_reply(
            SHA,
            None,
            "refused",
            None,
            Some("gate_red"),
            &["a::b".to_owned()],
            Some("tail"),
        );
        assert!(red.contains("failing_test: a::b") && red.contains("code: gate_red"));
        assert!(enqueued_reply(SHA, None, Uuid::nil(), 3).contains("sequence: 3"));
        assert!(needs_review_reply(SHA, None, "carries a migration").contains("held"));
        assert!(refused_reply(SHA, None, "source_not_on_origin", "push it").contains("push it"));
    }

    #[test]
    fn keys_fit_the_message_key_bound() {
        let key = intake_key(Uuid::nil(), Uuid::nil(), "settled");
        assert!(key.len() <= crate::satellite_dispatch::SATELLITE_MESSAGE_MAX_KEY_BYTES);
        assert!(key.len() <= crate::rolling_queue::ROLLING_QUEUE_MAX_IDEMPOTENCY_BYTES);
    }
}
