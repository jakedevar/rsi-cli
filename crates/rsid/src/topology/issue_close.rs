//! Closing the input Issue when a land publishes (#2260, plan §3.1).
//!
//! The policy is "close an Issue when its fix is on rolling". A land step with
//! `close_issue` closes the Issue that `inputs.issue` named at accept, as the
//! execution's owner (the manager or Epic lead that requested it; for an
//! operator start the project manager resolved at accept, #1746), through the
//! same guarded `AgentUpdateIssueStatus` path an owner would call by hand. The
//! Issue layer re-derives the owner's live Issue authority in the write
//! transaction, so a paused, replaced or ungranted owner is refused there and
//! the refusal is data: the land still settles `succeeded`, its output says
//! `issue_close: {status: "refused", code}` and the Issue stays open for the
//! owner (never a failed land: the commit is on rolling either way).
//!
//! Outcomes are idempotent, so a crash between the close and the land's
//! settlement re-runs the close on the next tick and the land settles once:
//!
//! - `closed`: this execution's close wrote the Issue (or already had: the
//!   write is keyed by the execution and attempt, and an Issue that is Closed
//!   with that key on its event log is this close replayed);
//! - `already_closed`: the Issue was Closed by someone else; nothing written;
//! - `skipped`: the Issue is Cancelled, archived or gone; left untouched;
//! - `refused`: the owner may not write the Issue (or there is no owner).
//!
//! A transient store error is an `Err` the caller retries on its next tick.

use rsi_common::types::IssueStatus;
use rusqlite::params;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::error::{DaemonError, Result};
use crate::store::Store;
use crate::topology::steps::WorkflowSteps;
use crate::topology::store::{AttemptRow, ExecutionRow};

/// What a land's `close_issue` does to the input Issue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum IssueClose {
    /// Closed by this execution's land.
    Closed,
    /// Already Closed by someone else; nothing written.
    AlreadyClosed,
    /// Left untouched (cancelled, archived or missing); carries the reason.
    Skipped(&'static str),
    /// The owner may not close it; carries the Issue layer's code.
    Refused(String),
    /// Deliberately left open for a manager (#3772): an `already_satisfied`
    /// claim never auto-closes an `operator-request` Issue.
    Held(&'static str),
}

impl IssueClose {
    pub(crate) const fn status(&self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::AlreadyClosed => "already_closed",
            Self::Skipped(_) => "skipped",
            Self::Refused(_) => "refused",
            Self::Held(_) => "held",
        }
    }

    pub(crate) fn code(&self) -> Option<&str> {
        match self {
            Self::Skipped(code) => Some(code),
            Self::Refused(code) => Some(code),
            Self::Held(code) => Some(code),
            Self::Closed | Self::AlreadyClosed => None,
        }
    }

    /// The land output's `issue_close` field.
    pub(crate) fn to_json(&self) -> Value {
        let mut value = json!({ "status": self.status() });
        if let Some(code) = self.code() {
            value["code"] = code.into();
        }
        value
    }
}

/// The Issue a land closes and who closes it.
#[derive(Clone, Debug)]
pub(crate) struct IssueCloseRequest {
    pub(crate) execution_id: Uuid,
    pub(crate) attempt_id: Uuid,
    /// The execution's owner (`requested_by_session_id`), if it has one.
    pub(crate) owner: Option<Uuid>,
    pub(crate) project_id: Uuid,
    /// The Epic the execution runs under (#3274): a displaced project manager
    /// seat's close is authorized as the current seat only while the current
    /// scope still covers it.
    pub(crate) epic_id: Option<Uuid>,
    pub(crate) display_number: i64,
    /// The Issue status note (#2651: the evidence commit and the worker's
    /// verification lines for an already-satisfied close); `None` for a land.
    pub(crate) note: Option<String>,
    /// Prefix of the idempotency key: a land and an already-satisfied close of
    /// the same attempt never share a key.
    pub(crate) key_kind: &'static str,
    /// #3881: the Issue `row_version` an `already_satisfied` proof was validated
    /// against; the close refuses (Issue left open) when the row moved on, so a
    /// proof only closes the exact Issue version whose criteria it covers.
    pub(crate) proved_row_version: Option<i64>,
}

/// The longest verification excerpt an already-satisfied note carries; the
/// whole note stays within `ISSUE_STATUS_NOTE_MAX_BYTES_V1`.
const SATISFIED_NOTE_EXCERPT_BYTES: usize = 1_600;

/// Idempotency-key prefix of an already-satisfied close (#2651).
const SATISFIED_KEY_KIND: &str = "topology-satisfied";

/// `issue_close.code` of an `already_satisfied` close refused because the Issue
/// changed after its proof was validated (#3881).
pub(crate) const ISSUE_CHANGED_AFTER_PROOF: &str = "already_satisfied_issue_changed";

/// The label the operator's own requests carry.
const OPERATOR_REQUEST_LABEL: &str = "operator-request";

/// `issue_close.code` of an `already_satisfied` settle that left an
/// `operator-request` Issue open (#3772).
pub(crate) const MANAGER_ACKNOWLEDGEMENT_REQUIRED: &str = "manager_acknowledgement_required";

/// A list-item line's text without its bullet or number (`-`, `*`, `1.`,
/// `2)`, `- [ ]`); `None` for any other line.
fn list_item(line: &str) -> Option<&str> {
    let line = line.trim_start();
    if let Some(rest) = line.strip_prefix(['-', '*', '+']) {
        let rest = rest.strip_prefix([' ', '\t'])?.trim_start();
        let rest = ["[ ]", "[x]", "[X]"]
            .iter()
            .find_map(|mark| rest.strip_prefix(mark))
            .map_or(rest, str::trim_start);
        return Some(rest);
    }
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let rest = line[digits..].strip_prefix(['.', ')'])?;
    Some(rest.strip_prefix([' ', '\t'])?.trim_start())
}

/// #3808: how many acceptance criteria an Issue body states: the list items
/// under its "acceptance criteria" (or "acceptance") heading or label, ending
/// at the next heading or the first plain paragraph after the list. An Issue
/// with no such list (the criteria are prose) states at least one criterion,
/// so the proof never maps nothing.
pub(crate) fn issue_acceptance_criteria_count(issue_body: &str) -> usize {
    let mut in_section = false;
    let mut count = 0;
    for line in issue_body.lines() {
        let plain = line.trim().trim_start_matches(['#', '*', '_', ' ']);
        if plain.to_ascii_lowercase().starts_with("acceptance") {
            if in_section && count > 0 {
                break;
            }
            in_section = true;
            continue;
        }
        if !in_section {
            continue;
        }
        if list_item(line).is_some() {
            count += 1;
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() || line.starts_with([' ', '\t']) {
            continue;
        }
        if count > 0 || trimmed.starts_with('#') {
            break;
        }
    }
    count.max(1)
}

/// One entry per line of a handoff field: the inline value, or the indented or
/// bulleted lines of a block value. `none`, `null`, `n/a` and `-` are no entry.
fn field_entries(lines: &[&str], keys: &[&str]) -> Vec<String> {
    let mut entries = Vec::new();
    for (at, line) in lines.iter().enumerate() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if !keys.contains(&key.trim().to_ascii_lowercase().as_str()) {
            continue;
        }
        let mut values = vec![value.trim().to_owned()];
        if value.trim().is_empty() {
            values = lines[at + 1..]
                .iter()
                .take_while(|next| {
                    next.starts_with([' ', '\t', '-', '*']) || next.trim().is_empty()
                })
                .filter(|next| !next.trim().is_empty())
                .map(|next| list_item(next).unwrap_or(next.trim()).to_owned())
                .collect();
        }
        entries.extend(values.into_iter().filter(|entry| {
            !entry.is_empty()
                && !matches!(
                    entry.to_ascii_lowercase().as_str(),
                    "none" | "null" | "n/a" | "-"
                )
        }));
    }
    entries
}

/// Words that claim success without naming anything checked.
fn is_bare_claim(evidence: &str) -> bool {
    matches!(
        evidence
            .trim_matches(|c: char| !c.is_alphanumeric())
            .to_ascii_lowercase()
            .as_str(),
        "" | "ok"
            | "okay"
            | "done"
            | "pass"
            | "passed"
            | "passes"
            | "yes"
            | "true"
            | "holds"
            | "satisfied"
            | "met"
            | "none"
            | "null"
            | "n/a"
    )
}

/// The criterion number of a summary entry `AC<n>: <evidence>` (`AC 2 -`,
/// `#2.`, `2)` also read) and its evidence part.
fn criterion_entry(entry: &str) -> Option<(usize, &str)> {
    let rest = entry.trim_start_matches(['*', '_', '`', ' ']);
    let rest = match rest.get(..2) {
        Some(prefix) if prefix.eq_ignore_ascii_case("ac") => &rest[2..],
        _ => rest,
    };
    let rest = rest.trim_start_matches(['#', ' ', '-']);
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    let number = rest[..digits].parse().ok()?;
    let evidence = rest[digits..]
        .trim_start_matches(['*', '_', '`'])
        .strip_prefix([':', '.', ')', '-', '\u{2014}'])?;
    Some((number, evidence.trim()))
}

/// Whether a test `result` text reports a pass (#3880): a passing word or exit
/// status 0, and no failing word, error or nonzero exit. A failing word counts
/// as zero failures only when a `0`/`no` precedes or a `0` follows it
/// (`ok. 3 passed; 0 failed`).
fn result_is_passing(result: &str) -> bool {
    let lowered = result.to_ascii_lowercase();
    let words: Vec<&str> = lowered
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();
    let mut passing = false;
    for (at, word) in words.iter().enumerate() {
        let before = at.checked_sub(1).map(|prev| words[prev]);
        let after = words.get(at + 1).copied();
        match *word {
            "fail" | "fails" | "failed" | "failing" | "failure" | "failures" | "error"
            | "errors" | "errored" | "erroring" | "red" | "panic" | "panicked" | "timeout"
            | "nonzero" => {
                let none =
                    matches!(before, Some("0" | "no" | "zero" | "without")) || after == Some("0");
                if !none {
                    return false;
                }
            }
            "non" if after == Some("zero") => return false,
            "exit" | "exited" | "code" => {
                let status = words[at + 1..]
                    .iter()
                    .take(3)
                    .find(|next| !matches!(**next, "code" | "status" | "with" | "of"));
                match status.and_then(|next| next.parse::<u32>().ok()) {
                    Some(0) => passing = true,
                    Some(_) => return false,
                    None => {}
                }
            }
            "pass" | "passed" | "passes" | "passing" | "ok" | "okay" | "green" | "success"
            | "successful" | "succeeded" | "clean"
                if !matches!(before, Some("not" | "no" | "never")) =>
            {
                passing = true;
            }
            _ => {}
        }
    }
    passing
}

/// Whether a `tests` entry names a command and a passing result:
/// `<command>: <result>` where the result says pass, ok or exit 0. A failing,
/// erroring or nonzero result is no proof (#3880).
fn names_command_and_result(entry: &str) -> bool {
    let Some((command, result)) = entry.rsplit_once(':') else {
        return false;
    };
    !command.trim().is_empty() && result_is_passing(result)
}

/// #3772, #3808: what an `already_satisfied` handoff lacks to count as proof,
/// or `None` when its shape is right. The daemon cannot judge evidence; it
/// refuses a claim whose shape offers none:
/// - `summary` carries one `AC<n>: <evidence>` entry for each of the Issue's
///   `criteria` (numbers 1..=criteria), each with an evidence part that is not
///   a bare word such as `ok`;
/// - `tests` (or `tests_added`) lists entries of the form `<command>: <result>`
///   where each result is a passing one: a failing, erroring or nonzero result
///   (`failed (3 failures)`, `error`, `exit 1`) is no proof (#3880).
///
/// The commit rules are checked apart.
pub(crate) fn satisfied_proof_gap(worker_message: &str, criteria: usize) -> Option<String> {
    let handoff = worker_message
        .find("PIPELINE HANDOFF")
        .map_or("", |at| &worker_message[at..]);
    let lines: Vec<&str> = handoff.lines().skip(1).collect();
    let criteria = criteria.max(1);
    let summary = field_entries(&lines, &["summary"]);
    let mut evidenced = std::collections::BTreeSet::new();
    for entry in &summary {
        let Some((number, evidence)) = criterion_entry(entry) else {
            continue;
        };
        if !is_bare_claim(evidence) {
            evidenced.insert(number);
        }
    }
    let missing: Vec<String> = (1..=criteria)
        .filter(|number| !evidenced.contains(number))
        .map(|number| format!("AC{number}"))
        .collect();
    if !missing.is_empty() {
        return Some(format!(
            "a `summary` with one `AC<n>: <evidence>` line for each of the Issue's {criteria} acceptance criteria (missing or without evidence: {})",
            missing.join(", ")
        ));
    }
    let tests = field_entries(&lines, &["tests", "tests_added"]);
    if tests.is_empty() || !tests.iter().all(|test| names_command_and_result(test)) {
        return Some(
            "`tests` entries of the form `<command>: <result>` (the commands you ran and their pass/fail results)"
                .into(),
        );
    }
    None
}

/// #2651: the note an already-satisfied close writes on the Issue: the
/// evidence commit and the worker's own handoff lines (its verification
/// claims), without the marker and path fields.
pub(crate) fn already_satisfied_note(evidence_commit: &str, worker_message: &str) -> String {
    let handoff = worker_message
        .find("PIPELINE HANDOFF")
        .map_or("", |at| &worker_message[at..]);
    let mut excerpt = String::new();
    for line in handoff.lines().skip(1) {
        let key = line.split(':').next().unwrap_or("").trim();
        if matches!(
            key,
            "doc_path" | "manifest_path" | "status" | "evidence_commit"
        ) {
            continue;
        }
        if excerpt.len() + line.len() + 1 > SATISFIED_NOTE_EXCERPT_BYTES {
            break;
        }
        excerpt.push_str(line.trim_end());
        excerpt.push('\n');
    }
    let mut note = format!(
        "Already satisfied on rolling (topology run, no landing): evidence commit {evidence_commit}."
    );
    if !excerpt.trim().is_empty() {
        note.push_str("\nWorker verification:\n");
        note.push_str(excerpt.trim_end());
    }
    note.replace('\0', "")
}

impl IssueCloseRequest {
    /// The close a worker's `already_satisfied` handoff asks for (#2651): the
    /// execution's input Issue, written under a key of its own attempt.
    /// `Err(refused)` when the execution carries no Issue snapshot.
    pub(crate) fn for_already_satisfied(
        execution: &ExecutionRow,
        attempt: &AttemptRow,
        note: String,
        proved_row_version: Option<i64>,
    ) -> std::result::Result<Self, IssueClose> {
        let (project_id, display_number) =
            crate::topology::starters::issue_snapshot(execution.input.as_ref())
                .ok_or_else(|| IssueClose::Refused("issue_input_missing".into()))?;
        Ok(Self {
            execution_id: execution.id,
            attempt_id: attempt.id,
            owner: execution.requested_by_session_id,
            project_id,
            epic_id: execution.epic_id.or(execution.parent_session_id),
            display_number,
            note: Some(note),
            key_kind: SATISFIED_KEY_KIND,
            proved_row_version,
        })
    }

    /// The close `attempt`'s land step asks for: `None` when it does not close
    /// an Issue; `Some(Err(refused))` when it does but the execution carries
    /// no Issue snapshot (accept refuses that, so only a hand-built execution
    /// reaches it).
    pub(crate) fn for_land(
        execution: &ExecutionRow,
        attempt: &AttemptRow,
    ) -> Option<std::result::Result<Self, IssueClose>> {
        let closes = WorkflowSteps::from_workflow(&execution.definition)
            .is_ok_and(|steps| steps.close_issue(&attempt.node_id));
        closes.then(|| {
            let (project_id, display_number) =
                crate::topology::starters::issue_snapshot(execution.input.as_ref())
                    .ok_or_else(|| IssueClose::Refused("issue_input_missing".into()))?;
            Ok(Self {
                execution_id: execution.id,
                attempt_id: attempt.id,
                owner: execution.requested_by_session_id,
                project_id,
                epic_id: execution.epic_id.or(execution.parent_session_id),
                display_number,
                note: None,
                key_kind: "topology-land",
                proved_row_version: None,
            })
        })
    }

    /// The Issue write's idempotency key: it names the execution and the land
    /// attempt, and the owner's Issue event log keeps it.
    pub(crate) fn idempotency_key(&self) -> String {
        format!(
            "{}:{}:{}",
            self.key_kind, self.execution_id, self.attempt_id
        )
    }
}

/// An Issue-layer error that is a refusal (authority, scope, transition), not a
/// transient condition: `Some(code)`. A stale version or storage failure is
/// retried on the next tick, as is any other error.
fn refusal_code(error: &DaemonError) -> Option<String> {
    let code = match error {
        DaemonError::StructuredRpc { message, .. } => message.as_str(),
        DaemonError::PolicyDenied(message) | DaemonError::InvalidParam(message) => message.as_str(),
        _ => return None,
    };
    let issue_layer = code.starts_with("agent_issue_")
        || code == rsi_common::global_manager::MANAGER_PROJECT_NOT_IN_SCOPE;
    let transient = matches!(
        code,
        "agent_issue_stale_version" | "agent_issue_storage_failure"
    );
    (issue_layer && !transient).then(|| code.to_owned())
}

/// Close the Issue as the owner. Pure over rows; call it under the store lock
/// so the read, the guarded write and the replay check see one state.
///
/// # Errors
/// A transient store error (including a stale row version): ask again.
pub(crate) fn close_input_issue(store: &Store, request: &IssueCloseRequest) -> Result<IssueClose> {
    let Some(owner) = request.owner else {
        return Ok(IssueClose::Refused("owner_required".into()));
    };
    // #2472: a rotated portfolio seat closes through its node's current seat.
    let requested = owner;
    let owner = crate::topology::agent::live_owner(store, owner)?;
    // #3274: a seat an explicit re-appointment displaced closes as the current
    // appointed manager, and only for an Epic that manager's scope covers.
    if store.displaced_manager_seat_now(requested)?.is_some() {
        let covered = store
            .get_harness_manager(request.project_id)?
            .zip(request.epic_id)
            .is_some_and(|(config, epic)| config.epic_ids.contains(&epic));
        if !covered {
            return Ok(IssueClose::Refused("epic_out_of_scope".into()));
        }
    }
    let Some(issue) =
        store.get_issue_by_display_number(request.project_id, request.display_number)?
    else {
        return Ok(IssueClose::Skipped("issue_missing"));
    };
    if issue.archived_at.is_some() {
        return Ok(IssueClose::Skipped("issue_archived"));
    }
    let key = request.idempotency_key();
    match issue.status {
        IssueStatus::Cancelled => return Ok(IssueClose::Skipped("issue_cancelled")),
        IssueStatus::Closed => {
            // Closed with this close's own key on its event log: the write
            // committed before a crash, so this is the close replayed.
            let ours: bool = store.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM issue_events \
                 WHERE issue_id=?1 AND actor_session_id=?2 AND idempotency_key=?3)",
                params![issue.id.to_string(), owner.to_string(), key],
                |row| row.get(0),
            )?;
            return Ok(if ours {
                IssueClose::Closed
            } else {
                IssueClose::AlreadyClosed
            });
        }
        IssueStatus::Open | IssueStatus::InProgress => {}
    }
    // #3881: the proof covered an earlier version of this Issue (its criteria
    // may have changed since): refuse, leave it open and say why.
    if request
        .proved_row_version
        .is_some_and(|proved| proved != issue.row_version)
    {
        return Ok(IssueClose::Refused(ISSUE_CHANGED_AFTER_PROOF.into()));
    }
    // #3772: an `already_satisfied` word is a worker's claim, not a landed
    // commit. An operator's own request is never closed on it alone: it stays
    // open for the manager to acknowledge (or reopen the work).
    if request.key_kind == SATISFIED_KEY_KIND
        && issue
            .labels
            .iter()
            .any(|label| label == OPERATOR_REQUEST_LABEL)
    {
        return Ok(IssueClose::Held(MANAGER_ACKNOWLEDGEMENT_REQUIRED));
    }
    let written = store.agent_update_issue_status(
        owner,
        &rsi_common::rpc::AgentUpdateIssueStatusRequestV1 {
            note: request.note.clone(),
            issue_id: Some(issue.id),
            display_number: None,
            status: IssueStatus::Closed,
            expected_row_version: issue.row_version,
            idempotency_key: key,
            include_issue: false,
            project_id: Some(request.project_id),
        },
    );
    match written {
        Ok(_) => Ok(IssueClose::Closed),
        Err(error) => refusal_code(&error).map_or(Err(error), |code| Ok(IssueClose::Refused(code))),
    }
}
