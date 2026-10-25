//! The settled-execution digest (#2559).
//!
//! A manager that learns an execution settled used to read the node list, the
//! Issue and the Issue list to learn four facts: what landed, what the review
//! decided, what the Issue's state is and which followups the run filed. This
//! module derives those facts once, from durable rows, for
//! `AgentTopologyGetExecution` and the `topology_execution_settled` wake.

use std::collections::BTreeMap;

use chrono::DateTime;
use rsi_common::topology_agent::{
    AgentTopologyOutcomeFailureV1, AgentTopologyOutcomeFollowupV1, AgentTopologyOutcomeIssueV1,
    AgentTopologyOutcomeLandV1, AgentTopologyOutcomeReviewV1, AgentTopologyOutcomeV1,
};
use rusqlite::params;
use serde_json::Value;
use uuid::Uuid;

use crate::error::Result;
use crate::store::Store;
use crate::topology::starters::issue_snapshot;
use crate::topology::store::{self as rows, AttemptRow, AttemptStatus, failure};

/// Bound on the digest a wake carries.
pub(crate) const OUTCOME_WAKE_BYTES: usize = 1_536;
/// Most followups listed (a run files a handful at most).
const FOLLOWUPS_MAX: i64 = 12;
/// Longest Issue title carried in the digest.
const TITLE_BYTES: usize = 80;

/// Whether `status` is one a digest is built for.
pub(crate) fn is_settled(status: &str) -> bool {
    matches!(status, "succeeded" | "failed" | "cancelled" | "blocked")
}

fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn field<'a>(attempt: &'a AttemptRow, name: &str) -> Option<&'a str> {
    attempt
        .output
        .as_ref()?
        .pointer(&format!("/fields/{name}"))
        .and_then(Value::as_str)
}

/// The latest attempt of each node of `kind`, by node id.
fn latest_by_node<'a>(attempts: &'a [AttemptRow], kind: &str) -> BTreeMap<&'a str, &'a AttemptRow> {
    let mut latest: BTreeMap<&str, &AttemptRow> = BTreeMap::new();
    for attempt in attempts
        .iter()
        .filter(|a| a.node_kind == kind && a.status != AttemptStatus::Skipped)
    {
        let entry = latest.entry(attempt.node_id.as_str()).or_insert(attempt);
        if (attempt.iteration, attempt.attempt_no) > (entry.iteration, entry.attempt_no) {
            *entry = attempt;
        }
    }
    latest
}

fn parse_secs(text: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|at| at.timestamp())
}

fn land(store: &Store, attempt: &AttemptRow) -> AgentTopologyOutcomeLandV1 {
    let entry = attempt
        .land_entry_id
        .and_then(|id| store.get_rolling_queue_entry(id).ok().flatten());
    let gate_wall_secs = entry.as_ref().and_then(|entry| {
        let finished = entry.finished_at.as_deref().and_then(parse_secs)?;
        Some((finished - parse_secs(&entry.enqueued_at)?).max(0))
    });
    AgentTopologyOutcomeLandV1 {
        node_id: attempt.node_id.clone(),
        state: field(attempt, "state")
            .map_or_else(|| attempt.status.as_str().to_owned(), str::to_owned),
        landed_sha: field(attempt, "landed_sha").map(str::to_owned),
        entry_id: attempt.land_entry_id,
        gate_wall_secs,
        issue_close_refused: (attempt
            .output
            .as_ref()
            .and_then(|output| output.pointer("/fields/issue_close/status"))
            .and_then(Value::as_str)
            == Some("refused"))
        .then(|| {
            attempt
                .output
                .as_ref()
                .and_then(|output| output.pointer("/fields/issue_close/code"))
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned()
        }),
    }
}

fn review(attempts: &[AttemptRow], latest: &AttemptRow) -> AgentTopologyOutcomeReviewV1 {
    let verdict = match latest.status {
        AttemptStatus::Succeeded => match field(latest, "verdict") {
            Some("accepted") => "approved",
            Some("changes_requested") => "changes_requested",
            _ => "unsettled",
        },
        _ if latest.failure_class.as_deref() == Some(failure::REVIEW_ROUNDS_EXHAUSTED) => {
            "rounds_exhausted"
        }
        _ => "unsettled",
    };
    // A settled attempt records the reviewer; an unsettled one may not, so
    // the node's latest recorded model stands in.
    let reviewer_model = attempts
        .iter()
        .filter(|a| a.node_id == latest.node_id && a.node_kind == "review")
        .max_by_key(|a| (a.iteration, a.attempt_no))
        .filter(|a| field(a, "reviewer_model").is_some())
        .or_else(|| {
            attempts
                .iter()
                .filter(|a| a.node_id == latest.node_id && field(a, "reviewer_model").is_some())
                .max_by_key(|a| (a.iteration, a.attempt_no))
        })
        .and_then(|a| field(a, "reviewer_model"))
        .map(str::to_owned);
    AgentTopologyOutcomeReviewV1 {
        node_id: latest.node_id.clone(),
        verdict: verdict.to_owned(),
        attempts: latest.attempt_no,
        round: latest.iteration.saturating_add(1),
        reviewer_model,
    }
}

/// Issues of `project` filed by one of the execution's node sessions, other
/// than the execution's own input Issue.
fn followups(
    store: &Store,
    project: Uuid,
    sessions: &[Uuid],
    input_issue: Option<i64>,
) -> Result<Vec<AgentTopologyOutcomeFollowupV1>> {
    if sessions.is_empty() {
        return Ok(Vec::new());
    }
    let sessions =
        serde_json::to_string(&sessions.iter().map(Uuid::to_string).collect::<Vec<_>>())?;
    let mut statement = store.conn.prepare(
        "SELECT display_number,title FROM issues \
         WHERE project_id=?1 AND archived_at IS NULL \
           AND created_by_session_id IN (SELECT value FROM json_each(?2)) \
           AND display_number IS NOT ?3 \
         ORDER BY display_number LIMIT ?4",
    )?;
    let rows = statement
        .query_map(
            params![project.to_string(), sessions, input_issue, FOLLOWUPS_MAX],
            |row| {
                Ok(AgentTopologyOutcomeFollowupV1 {
                    display_number: row.get(0)?,
                    title: clip(&row.get::<_, String>(1)?, TITLE_BYTES),
                })
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// The digest of a settled execution; `None` when the execution is unknown.
///
/// # Errors
/// Store failures.
pub(crate) fn settled_outcome(
    store: &Store,
    execution_id: Uuid,
) -> Result<Option<AgentTopologyOutcomeV1>> {
    let Some(execution) = rows::load_execution(store, execution_id)? else {
        return Ok(None);
    };
    let attempts = rows::load_attempts(store, execution_id)?;
    let mut outcome = AgentTopologyOutcomeV1::default();

    let snapshot = issue_snapshot(execution.input.as_ref());
    if let Some((project, number)) = snapshot {
        let title = execution
            .input
            .as_ref()
            .and_then(|input| input.pointer("/issue/title"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let status = u32::try_from(number)
            .ok()
            .and_then(|number| store.issue_status_by_number(project, number).ok().flatten())
            .map(|status| status.as_str().to_owned());
        outcome.issue = Some(AgentTopologyOutcomeIssueV1 {
            display_number: number,
            title: clip(title, TITLE_BYTES),
            status,
        });
    }

    outcome.lands = latest_by_node(&attempts, "land")
        .values()
        .map(|attempt| land(store, attempt))
        .collect();
    outcome.reviews = latest_by_node(&attempts, "review")
        .values()
        .map(|latest| review(&attempts, latest))
        .collect();

    let mut latest_any: BTreeMap<&str, &AttemptRow> = BTreeMap::new();
    for attempt in &attempts {
        let entry = latest_any
            .entry(attempt.node_id.as_str())
            .or_insert(attempt);
        if (attempt.iteration, attempt.attempt_no) > (entry.iteration, entry.attempt_no) {
            *entry = attempt;
        }
    }
    outcome.failures = latest_any
        .values()
        .filter(|a| matches!(a.status, AttemptStatus::Failed | AttemptStatus::Blocked))
        .map(|a| AgentTopologyOutcomeFailureV1 {
            node_id: a.node_id.clone(),
            status: a.status.as_str().to_owned(),
            failure_class: a.failure_class.clone(),
            attempt_id: a.id,
        })
        .collect();

    if let Some(project) = execution
        .project_id
        .or(snapshot.map(|(project, _)| project))
    {
        let mut sessions: Vec<Uuid> = attempts.iter().map(|a| a.session_id).collect();
        sessions.sort();
        sessions.dedup();
        outcome.followups = followups(store, project, &sessions, snapshot.map(|(_, n)| n))?;
    }
    Ok(Some(outcome))
}

/// #3387: whether a `succeeded` execution's outcome asks nothing of its
/// owner: the input Issue is Closed, no node failed, no land's Issue close was
/// refused, every review settled approved, and the run filed no follow-up.
/// Anything else, including an outcome with no input Issue, wakes the owner.
pub(crate) fn is_clean_success(outcome: &AgentTopologyOutcomeV1) -> bool {
    outcome
        .issue
        .as_ref()
        .is_some_and(|issue| issue.status.as_deref() == Some("Closed"))
        && outcome.failures.is_empty()
        && outcome.followups.is_empty()
        && outcome
            .lands
            .iter()
            .all(|land| land.issue_close_refused.is_none())
        && outcome
            .reviews
            .iter()
            .all(|review| review.verdict == "approved")
}

/// #3387: the one-line digest entry of a clean success: execution, Issue and
/// the SHA each land published.
pub(crate) fn digest_line(execution_id: Uuid, outcome: &AgentTopologyOutcomeV1) -> String {
    let mut line = format!("execution {execution_id} succeeded");
    if let Some(issue) = &outcome.issue {
        line.push_str(&format!(
            "; issue #{} \"{}\" Closed",
            issue.display_number, issue.title
        ));
    }
    for sha in outcome.lands.iter().filter_map(|l| l.landed_sha.as_deref()) {
        line.push_str(&format!("; landed {sha}"));
    }
    line
}

/// #2584: what had already happened when a node failure ended the execution:
/// the SHA each land node published and each review's verdict, so a failure
/// after the source landed does not read as a bare `failed`. `None` when
/// nothing had landed or been reviewed.
pub(crate) fn failure_context(store: &Store, execution_id: Uuid) -> Option<String> {
    let outcome = settled_outcome(store, execution_id).ok()??;
    let mut parts: Vec<String> = outcome
        .lands
        .iter()
        .filter_map(|land| {
            let sha = land.landed_sha.as_deref()?;
            Some(format!("{} landed {sha}", land.node_id))
        })
        .collect();
    parts.extend(
        outcome
            .reviews
            .iter()
            .filter(|review| review.verdict != "unsettled")
            .map(|review| format!("{}: {}", review.node_id, verdict_phrase(review))),
    );
    (!parts.is_empty()).then(|| format!("settled before the failure: {}", parts.join("; ")))
}

fn verdict_phrase(review: &AgentTopologyOutcomeReviewV1) -> String {
    let n = review.attempts;
    match review.verdict.as_str() {
        "approved" => format!("approved on attempt {n}"),
        "changes_requested" => format!("changes requested on attempt {n}"),
        "rounds_exhausted" => format!("rounds exhausted on attempt {n}"),
        _ => format!("unsettled after attempt {n}"),
    }
}

/// The digest as the wake's report lines, within `max_bytes`: the Issue, the
/// lands, the reviews, the failures, then the followups; lines that no longer
/// fit are counted, not cut mid-line. Attempt ids appear on failure lines
/// only.
pub(crate) fn render(outcome: &AgentTopologyOutcomeV1, max_bytes: usize) -> String {
    let mut lines: Vec<String> = Vec::new();
    if let Some(issue) = &outcome.issue {
        lines.push(format!(
            "  issue #{} {} \"{}\"",
            issue.display_number,
            issue.status.as_deref().unwrap_or("status_unknown"),
            issue.title
        ));
    }
    for land in &outcome.lands {
        let mut line = format!("  land {}: {}", land.node_id, land.state);
        if let Some(sha) = &land.landed_sha {
            line.push_str(&format!(" sha={sha}"));
        }
        if let Some(secs) = land.gate_wall_secs {
            line.push_str(&format!(" gate_wall={secs}s"));
        }
        if let Some(entry) = land.entry_id {
            line.push_str(&format!(" entry={entry}"));
        }
        if let Some(code) = &land.issue_close_refused {
            line.push_str(&format!(" issue_close=refused:{code}"));
        }
        lines.push(line);
    }
    for review in &outcome.reviews {
        let mut line = format!("  review {}: {}", review.node_id, verdict_phrase(review));
        if review.round > 1 {
            line.push_str(&format!(" (round {})", review.round));
        }
        if let Some(model) = &review.reviewer_model {
            line.push_str(&format!(" reviewer={model}"));
        }
        lines.push(line);
    }
    for failure in &outcome.failures {
        lines.push(format!(
            "  FAILED {}: {} failure_class={} attempt_id={}",
            failure.node_id,
            failure.status,
            failure.failure_class.as_deref().unwrap_or("-"),
            failure.attempt_id
        ));
    }
    for followup in &outcome.followups {
        lines.push(format!(
            "  filed #{} \"{}\"",
            followup.display_number, followup.title
        ));
    }
    let mut out = String::from("outcome:");
    let mut omitted = 0usize;
    for line in lines {
        if out.len() + line.len() + 1 > max_bytes.saturating_sub(48) {
            omitted += 1;
        } else {
            out.push('\n');
            out.push_str(&line);
        }
    }
    if omitted > 0 {
        out.push_str(&format!(
            "\n  (+{omitted} more; read AgentTopologyGetExecution)"
        ));
    }
    out
}
