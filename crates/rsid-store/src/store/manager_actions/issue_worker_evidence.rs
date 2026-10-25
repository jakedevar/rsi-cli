//! #2157: durable verification evidence for a reviewer's brief.
//!
//! A bound reviewer reads only its own review Issue, so the daemon copies the
//! implementer's latest handoff into the brief (#1590). When that handoff is
//! empty, or names no `scoped_test`, the reviewer had nothing to check the
//! work's verification against. This module reads the records the daemon
//! already keeps, read-only and bounded, and renders them as a short block:
//!
//! * the implementer session's final `RESULT` line,
//! * its latest succeeded `scoped_test` job receipt (job id, head, ok,
//!   completed counts, log path),
//! * the merge-queue outcome of the landed source (landed SHA, `gate_timing`).
//!
//! Evidence the daemon holds no record of is named unavailable, never omitted.
//!
//! #2727: the landed queue entry's derived filters and gate result ride in a
//! separate landing-gate block that the brief carries whatever the handoff
//! says: a reviewer cannot read the queue (`queue_not_authorized`), so without
//! it a test the gate already ran gets re-run as a job.
//! The reviewer's Issue read scope is not widened: the text rides in the brief.

use crate::error::Result;
use crate::store::Store;
use crate::store::harness_manager_v2::refused;
use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

use super::ACTION_KIND;

/// The implementer sessions of one Issue the evidence is read from, newest first.
const EVIDENCE_WORKERS_MAX: usize = 4;
/// The most bytes of any one evidence line.
const EVIDENCE_LINE_MAX: usize = 600;
/// The most bytes of the derived-filter list in the landing-gate block.
const GATE_FILTERS_MAX: usize = 1_400;
/// The most failing-test names listed in the landing-gate block.
const GATE_FAILING_MAX: usize = 8;

/// Whether a handoff already carries verification evidence: it names a
/// `scoped_test` (`scoped-test`) run or the job that ran it.
pub(super) fn handoff_names_verification(handoff: &str) -> bool {
    let lower = handoff.to_ascii_lowercase();
    lower.contains("scoped_test") || lower.contains("scoped-test") || lower.contains("scoped test")
}

fn clip(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

/// The first full lowercase 40-hex commit id in `text`.
fn commit_in(text: &str) -> Option<String> {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .find(|token| {
            token.len() == 40
                && token
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
        .map(str::to_string)
}

/// The last line of `message` that starts a `RESULT` report.
fn result_line(message: &str) -> Option<&str> {
    message
        .lines()
        .map(str::trim)
        .rfind(|line| line.starts_with("RESULT ") || *line == "RESULT")
}

impl Store {
    /// The sessions launched on `issue_id` as implementers (an Issue binding
    /// with no `review_of`), newest launch first.
    fn evidence_workers(conn: &Connection, project: Uuid, issue_id: Uuid) -> Result<Vec<Uuid>> {
        let mut statement = conn.prepare(
            "SELECT o.target_session_id FROM harness_manager_v2_operations o
             WHERE o.kind=?1 AND o.project_id=?2 AND o.state IN ('running','succeeded')
               AND o.target_session_id IS NOT NULL
               AND json_extract(o.payload_json,'$.issue_binding.issue_id')=?3
               AND json_extract(o.payload_json,'$.issue_binding.review_of') IS NULL
             GROUP BY 1 ORDER BY MAX(o.rowid) DESC LIMIT ?4",
        )?;
        let raw = statement
            .query_map(
                params![
                    ACTION_KIND,
                    project.to_string(),
                    issue_id.to_string(),
                    EVIDENCE_WORKERS_MAX as i64
                ],
                |row| row.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        raw.iter()
            .map(|id| {
                Uuid::parse_str(id).map_err(|_| refused("manager_v2_invalid_stored_identity"))
            })
            .collect()
    }

    /// #604: whether the review worker `reader` may read (`AgentGetJob`, read
    /// only) the verification job `job_id`, owned by `owner`. True only when
    /// all of these hold, and every other case is false (the verb then answers
    /// `job_not_found`, so it is no oracle for foreign jobs):
    /// - `reader`'s latest Issue-bound launch carries `review_of` = N, naming an
    ///   Issue of its own project;
    /// - `owner` is one of the implementer sessions launched on Issue N (the
    ///   sessions [`Self::evidence_workers`] reads, the ones the review's
    ///   handoff was copied from);
    /// - the job is a `scoped_test` verification job of that owner; and
    /// - it is the owner's newest `scoped_test` job, or its id appears in
    ///   Issue N's body or newest [`BOUND_HANDOFF_SCAN_MAX`] handoff records.
    ///
    /// The id scan is a membership test; it never follows or runs anything.
    /// Observation only: ownership, cancel and wake authority are unchanged.
    pub(crate) fn review_worker_may_read_job(
        &self,
        reader: Uuid,
        job_id: Uuid,
        owner: Uuid,
    ) -> Result<bool> {
        let conn = &self.conn;
        let launch: Option<(String, Option<i64>)> = conn
            .query_row(
                "SELECT o.project_id, json_extract(o.payload_json,'$.issue_binding.review_of')
                 FROM harness_manager_v2_operations o
                 WHERE o.kind=?1 AND o.target_session_id=?2
                   AND o.state IN ('running','succeeded')
                 ORDER BY o.rowid DESC LIMIT 1",
                params![ACTION_KIND, reader.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((project, Some(reviewed))) = launch else {
            return Ok(false);
        };
        let Ok(project) = Uuid::parse_str(&project) else {
            return Ok(false);
        };
        let reviewed_issue: Option<(String, String)> = conn
            .query_row(
                "SELECT id, body FROM issues WHERE project_id=?1 AND display_number=?2",
                params![project.to_string(), reviewed],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((reviewed_id, body)) = reviewed_issue else {
            return Ok(false);
        };
        let Ok(reviewed_id) = Uuid::parse_str(&reviewed_id) else {
            return Ok(false);
        };
        if !Self::evidence_workers(conn, project, reviewed_id)?.contains(&owner) {
            return Ok(false);
        }
        let newest: Option<String> = conn
            .query_row(
                "SELECT id FROM agent_jobs
                 WHERE owner_session_id=?1 AND kind='test'
                   AND json_type(params_json,'$.scoped_test')='object'
                 ORDER BY sequence DESC LIMIT 1",
                params![owner.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let id = job_id.to_string();
        if newest.as_deref() == Some(id.as_str()) {
            return Ok(true);
        }
        let verification: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM agent_jobs
             WHERE id=?1 AND owner_session_id=?2 AND kind='test'
               AND json_type(params_json,'$.scoped_test')='object')",
            params![id, owner.to_string()],
            |row| row.get(0),
        )?;
        if !verification {
            return Ok(false);
        }
        if body.to_ascii_lowercase().contains(&id) {
            return Ok(true);
        }
        let mut statement = conn.prepare(
            "SELECT body FROM issue_handoffs WHERE issue_id=?1 ORDER BY sequence DESC LIMIT ?2",
        )?;
        let handoffs = statement
            .query_map(
                params![
                    reviewed_id.to_string(),
                    super::issue_worker::BOUND_HANDOFF_SCAN_MAX
                ],
                |row| row.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(handoffs
            .iter()
            .any(|handoff| handoff.to_ascii_lowercase().contains(&id)))
    }

    /// The `RESULT` line of the newest assistant message of `worker` that has
    /// one among its last few messages (provider diagnostics skipped, #1089).
    fn evidence_result_line(conn: &Connection, worker: Uuid) -> Result<Option<String>> {
        let mut statement = conn.prepare(
            "SELECT content FROM conversation_events
             WHERE session_id=?1 AND event_type='Message' AND role='Assistant'
               AND (metadata IS NULL OR NOT json_valid(metadata)
                    OR COALESCE(json_extract(metadata,'$.provider_diagnostic'),0)!=1)
             ORDER BY sequence DESC LIMIT 3",
        )?;
        let messages = statement
            .query_map(params![worker.to_string()], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(messages
            .iter()
            .find_map(|message| result_line(message).map(str::to_string)))
    }

    /// One line describing the latest succeeded `scoped_test` job owned by
    /// `worker`.
    fn evidence_scoped_job(conn: &Connection, worker: Uuid) -> Result<Option<String>> {
        let row: Option<(String, String, Option<String>, Option<String>)> = conn
            .query_row(
                "SELECT id, log_path, result_json, finished_at FROM agent_jobs
                 WHERE owner_session_id=?1 AND kind='test' AND state='succeeded'
                   AND json_type(params_json,'$.scoped_test')='object'
                 ORDER BY sequence DESC LIMIT 1",
                params![worker.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((id, log_path, result_json, finished_at)) = row else {
            return Ok(None);
        };
        let result: serde_json::Value = result_json
            .as_deref()
            .and_then(|text| serde_json::from_str(text).ok())
            .unwrap_or(serde_json::Value::Null);
        let receipt = result.get("receipt").unwrap_or(&serde_json::Value::Null);
        let text = |key: &str| {
            receipt
                .get(key)
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unrecorded")
        };
        let ok = receipt
            .get("ok")
            .and_then(serde_json::Value::as_bool)
            .map_or("unrecorded".to_string(), |ok| ok.to_string());
        let completed = receipt
            .get("packages")
            .and_then(serde_json::Value::as_array)
            .map(|packages| {
                packages
                    .iter()
                    .filter_map(|item| {
                        let name = item.get("package")?.as_str()?;
                        let count = item.get("completed")?.as_u64()?;
                        Some(format!("{name}:{count}"))
                    })
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .filter(|joined| !joined.is_empty())
            .unwrap_or_else(|| "unrecorded".to_string());
        Ok(Some(clip(
            &format!(
                "scoped_test job {id}: head={} base={} ok={ok} completed={completed} log={log_path} finished_at={}",
                text("head"),
                text("base"),
                finished_at.as_deref().unwrap_or("unrecorded"),
            ),
            EVIDENCE_LINE_MAX,
        )))
    }

    /// One line describing the newest merge-queue entry for `source`.
    fn evidence_queue_entry_by_commit(conn: &Connection, source: &str) -> Result<Option<String>> {
        Self::evidence_queue_line(
            conn,
            "SELECT source_commit, state, outcome_json, finished_at FROM rolling_queue_entries
             WHERE source_commit=?1 ORDER BY sequence DESC LIMIT 1",
            source,
        )
    }

    fn evidence_queue_entry_by_session(conn: &Connection, worker: Uuid) -> Result<Option<String>> {
        Self::evidence_queue_line(
            conn,
            "SELECT source_commit, state, outcome_json, finished_at FROM rolling_queue_entries
             WHERE source_session_id=?1 ORDER BY sequence DESC LIMIT 1",
            &worker.to_string(),
        )
    }

    fn evidence_queue_line(conn: &Connection, sql: &str, key: &str) -> Result<Option<String>> {
        let row: Option<(String, String, Option<String>, Option<String>)> = conn
            .query_row(sql, params![key], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .optional()?;
        let Some((source, state, outcome_json, finished_at)) = row else {
            return Ok(None);
        };
        let outcome: serde_json::Value = outcome_json
            .as_deref()
            .and_then(|text| serde_json::from_str(text).ok())
            .unwrap_or(serde_json::Value::Null);
        let field = |key: &str| {
            outcome
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        };
        let failing = outcome
            .get("failing_tests")
            .and_then(serde_json::Value::as_array)
            .map_or(0, Vec::len);
        let gate_timing = field("detail")
            .and_then(|detail| {
                detail
                    .lines()
                    .find_map(|line| line.find("gate_timing=").map(|at| line[at..].to_string()))
            })
            .map(|line| clip(&line, 300));
        let mut line = format!(
            "merge queue: source={source} state={state} landed_sha={} refusal={} failing_tests={failing}",
            field("landed_sha").as_deref().unwrap_or("none"),
            field("refusal").as_deref().unwrap_or("none"),
        );
        if let Some(finished_at) = finished_at {
            line.push_str(&format!(" finished_at={finished_at}"));
        }
        line.push_str(&format!(
            " {}",
            gate_timing.as_deref().unwrap_or("gate_timing=unrecorded")
        ));
        Ok(Some(clip(&line, EVIDENCE_LINE_MAX)))
    }

    /// One queue entry's gate record: id, source, state, filters, outcome.
    fn gate_entry_row(
        conn: &Connection,
        sql: &str,
        key: &str,
    ) -> Result<
        Option<(
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
        )>,
    > {
        Ok(conn
            .query_row(sql, params![key], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            })
            .optional()?)
    }

    /// #2727: the landing gate's own record for the implementer's source: the
    /// filters the queue derived and stored, the entry state, the landed SHA,
    /// the failing tests and the gate timing. `None` when no entry is recorded.
    fn evidence_gate_block(
        conn: &Connection,
        workers: &[Uuid],
        result_commit: Option<&str>,
    ) -> Result<Option<String>> {
        const COLUMNS: &str =
            "id, source_commit, state, test_filters_json, outcome_json, finished_at";
        let mut row = match result_commit {
            Some(commit) => Self::gate_entry_row(
                conn,
                &format!(
                    "SELECT {COLUMNS} FROM rolling_queue_entries WHERE source_commit=?1 ORDER BY sequence DESC LIMIT 1"
                ),
                commit,
            )?,
            None => None,
        };
        if row.is_none() {
            for worker in workers {
                row = Self::gate_entry_row(
                    conn,
                    &format!(
                        "SELECT {COLUMNS} FROM rolling_queue_entries WHERE source_session_id=?1 ORDER BY sequence DESC LIMIT 1"
                    ),
                    &worker.to_string(),
                )?;
                if row.is_some() {
                    break;
                }
            }
        }
        let Some((id, source, state, filters_json, outcome_json, finished_at)) = row else {
            return Ok(None);
        };
        let filters: Vec<String> = rsi_common::rolling_queue::split_filters_state(
            serde_json::from_str(&filters_json).unwrap_or_default(),
        )
        .0;
        let outcome: serde_json::Value = outcome_json
            .as_deref()
            .and_then(|text| serde_json::from_str(text).ok())
            .unwrap_or(serde_json::Value::Null);
        let text = |key: &str| outcome.get(key).and_then(serde_json::Value::as_str);
        let failing: Vec<&str> = outcome
            .get("failing_tests")
            .and_then(serde_json::Value::as_array)
            .map(|names| names.iter().filter_map(serde_json::Value::as_str).collect())
            .unwrap_or_default();
        let mut listed = String::new();
        let mut shown = 0usize;
        for filter in &filters {
            if listed.len() + filter.len() + 2 > GATE_FILTERS_MAX {
                break;
            }
            if shown > 0 {
                listed.push_str(", ");
            }
            listed.push_str(filter);
            shown += 1;
        }
        if shown < filters.len() {
            listed.push_str(&format!(" ... ({} more not shown)", filters.len() - shown));
        }
        let mut lines = vec![
            format!(
                "queue entry {id}: source={source} state={state} landed_sha={} refusal={} finished_at={}",
                text("landed_sha").unwrap_or("none"),
                text("refusal").unwrap_or("none"),
                finished_at.as_deref().unwrap_or("unrecorded"),
            ),
            format!(
                "derived filters ({}): {}",
                filters.len(),
                if filters.is_empty() {
                    "none recorded"
                } else {
                    &listed
                }
            ),
            format!(
                "failing tests ({}): {}",
                failing.len(),
                if failing.is_empty() {
                    "none".to_string()
                } else {
                    failing
                        .iter()
                        .take(GATE_FAILING_MAX)
                        .map(|name| clip(name, 160))
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            ),
        ];
        if let Some(timing) = text("detail").and_then(|detail| {
            detail
                .lines()
                .find_map(|line| line.find("gate_timing=").map(|at| clip(&line[at..], 300)))
        }) {
            lines.push(timing);
        }
        Ok(Some(lines.join("\n")))
    }

    /// #2727: the landing-gate block of a reviewer's brief, for the implementer
    /// work of `issue_id`: what the merge queue derived and ran for it, so the
    /// reviewer need not re-run a test the gate already ran. Read-only; best
    /// effort like [`Self::review_verification_evidence`].
    pub(super) fn review_landing_gate_evidence(
        conn: &Connection,
        project: Uuid,
        issue_id: Uuid,
    ) -> String {
        let block = (|| -> Result<String> {
            let workers = Self::evidence_workers(conn, project, issue_id)?;
            if workers.is_empty() {
                return Ok(
                    "landing gate: unavailable (no implementer session is recorded for the Issue)"
                        .to_string(),
                );
            }
            let mut commit = None;
            for worker in &workers {
                if let Some(line) = Self::evidence_result_line(conn, *worker)? {
                    commit = commit_in(&line);
                    break;
                }
            }
            Ok(Self::evidence_gate_block(conn, &workers, commit.as_deref())?.unwrap_or_else(|| {
                "landing gate: unavailable (no queue entry is recorded for the implementer's source)"
                    .to_string()
            }))
        })();
        block.unwrap_or_else(|_| "landing gate: unavailable (the queue lookup failed)".to_string())
    }

    /// The evidence lines for the implementer work of `issue_id`, at most about
    /// 2 KiB, unavailable parts named. Read-only. Best effort: a store error
    /// reads as unavailable evidence rather than refusing the launch.
    pub(super) fn review_verification_evidence(
        conn: &Connection,
        project: Uuid,
        issue_id: Uuid,
    ) -> String {
        Self::try_review_verification_evidence(conn, project, issue_id).unwrap_or_else(|_| {
            "implementer verification evidence: unavailable (the evidence lookup failed)"
                .to_string()
        })
    }

    fn try_review_verification_evidence(
        conn: &Connection,
        project: Uuid,
        issue_id: Uuid,
    ) -> Result<String> {
        let workers = Self::evidence_workers(conn, project, issue_id)?;
        if workers.is_empty() {
            return Ok("implementer verification evidence: unavailable (no implementer session is recorded for the Issue)".to_string());
        }
        let mut lines = vec![format!(
            "implementer sessions (newest first): {}",
            workers
                .iter()
                .map(Uuid::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )];
        let mut result = None;
        for worker in &workers {
            if let Some(line) = Self::evidence_result_line(conn, *worker)? {
                result = Some(line);
                break;
            }
        }
        lines.push(result.as_deref().map_or_else(
            || "final RESULT line: unavailable (no RESULT line in the implementer's final messages)".to_string(),
            |line| format!("final RESULT line: {}", clip(line, EVIDENCE_LINE_MAX)),
        ));
        let mut job = None;
        for worker in &workers {
            if let Some(line) = Self::evidence_scoped_job(conn, *worker)? {
                job = Some(line);
                break;
            }
        }
        lines.push(job.unwrap_or_else(|| {
            "scoped_test job: unavailable (no succeeded scoped_test job is recorded for the implementer)".to_string()
        }));
        let mut queue = match result.as_deref().and_then(commit_in) {
            Some(commit) => Self::evidence_queue_entry_by_commit(conn, &commit)?,
            None => None,
        };
        if queue.is_none() {
            for worker in &workers {
                if let Some(line) = Self::evidence_queue_entry_by_session(conn, *worker)? {
                    queue = Some(line);
                    break;
                }
            }
        }
        lines.push(queue.unwrap_or_else(|| {
            "merge queue: unavailable (no queue entry is recorded for the implementer's source)"
                .to_string()
        }));
        Ok(lines.join("\n"))
    }
}
