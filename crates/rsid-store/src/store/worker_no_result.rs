//! Store half of the worker no-result guard (issue #1098): the wake identity
//! constants and the `Store` queries the session-side guard calls. Moved here
//! from `session::worker_result_guard` so `store` has no edge into `session`
//! (issue #1021 S4); the guard re-exports the constants at their old path.

use rsi_common::types::{Session, SessionStatus};
use rusqlite::{OptionalExtension, params};
use uuid::Uuid;

use crate::store::Store;

/// Marker carried by the automatic continuation message. Its presence in the
/// transcript is the durable "already nudged once" record.
pub const NO_RESULT_MARKER: &str = "[rsid-no-result]";

const NO_RESULT_NAMESPACE: Uuid = Uuid::from_u128(0x1098_0000_0000_4000_8000_0000_0000_0001);

/// Typed refusal: the delivery-time re-proof found the worker ineligible and
/// the wake is retired (never deferred, never delivered).
pub const NO_RESULT_RETIRED: &str = "no_result_continuation_retired";

pub const NO_RESULT_WAKE_NAME_PREFIX: &str = "worker-no-result-";

/// True when any line of `text` is a final worker report: a `RESULT ...`
/// line or a `PIPELINE HANDOFF ...` marker (reviewers report `PIPELINE
/// HANDOFF — REVIEW:` with no `RESULT` line, #1392). Leading markup (`**`,
/// `#`, `>`, backticks, list dashes) and markup closing the keyword
/// (`**RESULT**:`) are tolerated; prose such as "the result is" is not a
/// report.
pub fn is_final_report(text: &str) -> bool {
    text.lines().any(|line| {
        let line = strip_line_markup(line);
        keyword_line(line, "RESULT")
            || keyword_line(line, "PIPELINE HANDOFF")
            || review_verdict_line(line).is_some()
    })
}

fn strip_line_markup(line: &str) -> &str {
    line.trim_start_matches(|c: char| {
        c.is_whitespace() || matches!(c, '*' | '#' | '>' | '`' | '-' | '_')
    })
}

/// #1616: a reviewer's closing `REVIEW APPROVE ...` / `REVIEW CHANGES ...`
/// line is a final report. Only a verdict word right after `REVIEW` counts
/// (`REVIEW: ACCEPTED` fields sit under a `PIPELINE HANDOFF` marker and prose
/// such as "review approved by" is not a report).
fn review_verdict_line(line: &str) -> Option<&'static str> {
    let rest = line.strip_prefix("REVIEW")?;
    let rest = rest.trim_start_matches(['*', '`', '_', ':', ' ', '\t']);
    if !line["REVIEW".len()..].starts_with([' ', ':', '\t', '*', '`', '_']) {
        return None;
    }
    ["APPROVE", "CHANGES"].into_iter().find(|verdict| {
        rest.strip_prefix(verdict)
            .is_some_and(|tail| tail.is_empty() || tail.starts_with(|c: char| !c.is_alphanumeric()))
    })
}

/// #1616: the verdict (`APPROVE` or `CHANGES`) of the latest `REVIEW` final
/// line in the session's latest turn, for the terminal-watch annotation.
pub fn review_verdict(text: &str) -> Option<&'static str> {
    text.lines()
        .rev()
        .find_map(|line| review_verdict_line(strip_line_markup(line)))
}

fn keyword_line(line: &str, keyword: &str) -> bool {
    let Some(rest) = line.strip_prefix(keyword) else {
        return false;
    };
    let rest = rest.trim_start_matches(['*', '`', '_']);
    rest.is_empty() || rest.starts_with([' ', ':', '\t'])
}

/// Deterministic id of a worker's single automatic continuation wake.
pub fn no_result_wake_id(session_id: Uuid) -> Uuid {
    Uuid::new_v5(&NO_RESULT_NAMESPACE, session_id.as_bytes())
}

impl Store {
    /// Covered worker: a leaf with a parent that is neither a manager seat nor
    /// an Epic lead.
    pub fn worker_no_result_covered(&self, session: &Session) -> crate::error::Result<bool> {
        let Some(parent) = session.parent_id else {
            return Ok(false);
        };
        if !rsi_common::is_leaf_kind(session.session_kind) {
            return Ok(false);
        }
        if self.get_session(parent)?.is_none() {
            return Ok(false);
        }
        let leads_epic: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE lead_session_id=?1)",
            params![session.id.to_string()],
            |row| row.get(0),
        )?;
        if leads_epic {
            return Ok(false);
        }
        if let Some(project) = session.project_id
            && self
                .get_harness_manager_notice_config(project)?
                .and_then(|config| config.current_session_id)
                == Some(session.id)
        {
            return Ok(false);
        }
        Ok(true)
    }

    /// The single automatic continuation was already issued for this worker.
    pub fn worker_no_result_nudged(&self, session_id: Uuid) -> crate::error::Result<bool> {
        let wake: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM scheduled_jobs WHERE id=?1)",
            params![no_result_wake_id(session_id).to_string()],
            |row| row.get(0),
        )?;
        if wake {
            return Ok(true);
        }
        let marker: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM conversation_events
              WHERE session_id=?1 AND role='User' AND instr(content, ?2)>0)",
            params![session_id.to_string(), NO_RESULT_MARKER],
            |row| row.get(0),
        )?;
        Ok(marker)
    }

    /// #1109: the worker's automatic continuation wake is still armed, so its
    /// Completed status is transient and a terminal watch must not fire yet.
    pub fn no_result_wake_pending(&self, session_id: Uuid) -> crate::error::Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM scheduled_jobs WHERE id=?1 AND enabled=1)",
            params![no_result_wake_id(session_id).to_string()],
            |row| row.get(0),
        )?)
    }

    /// #1124: delivery-time re-proof for a no-result continuation wake, run
    /// under the target's spawn guard immediately before the provider effect.
    /// Jobs that are not no-result wakes pass untouched. The wake is only ever
    /// delivered to its own Completed leaf worker that nobody halted, paused
    /// or archived and that has no pending question or unresolved approval;
    /// anything else is refused with `NO_RESULT_RETIRED` and the scheduler
    /// retires the row.
    pub(crate) fn check_no_result_wake_delivery(
        &self,
        target: Uuid,
        job_ids: &[Uuid],
    ) -> crate::error::Result<()> {
        use crate::error::DaemonError;
        let retired = || DaemonError::InvalidParam(NO_RESULT_RETIRED.into());
        for job in job_ids {
            let name: Option<String> = self
                .conn
                .query_row(
                    "SELECT name FROM scheduled_jobs WHERE id=?1",
                    params![job.to_string()],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(worker) = name
                .as_deref()
                .and_then(|name| name.strip_prefix(NO_RESULT_WAKE_NAME_PREFIX))
                .and_then(|id| Uuid::parse_str(id).ok())
                .filter(|worker| no_result_wake_id(*worker) == *job)
            else {
                continue;
            };
            if worker != target {
                return Err(retired());
            }
            let Some(session) = self.get_session(worker)? else {
                return Err(retired());
            };
            if session.status != SessionStatus::Completed
                || session.pending_archive
                || !self.worker_no_result_covered(&session)?
                || self.get_operator_pause(worker)?
                    != crate::store::manager_actions::OperatorPause::None
            {
                return Err(retired());
            }
            // #1392: the worker already reported in its latest turn (the
            // arm-time check can miss a report split across messages), so the
            // nudge would only repeat it.
            if self.latest_turn_reported(worker)? {
                return Err(retired());
            }
            // Raw markers: hydration tolerates malformed JSON, which must not
            // erase a human gate.
            let held: bool = self.conn.query_row(
                "SELECT pending_question_json IS NOT NULL
                     OR EXISTS(SELECT 1 FROM approvals WHERE session_id=?1 AND status='Pending')
                 FROM sessions WHERE id=?1",
                params![worker.to_string()],
                |row| row.get(0),
            )?;
            if held {
                return Err(retired());
            }
        }
        Ok(())
    }

    /// #1392: an assistant message of the session's latest turn (after its
    /// last user message) carries a final `RESULT` or `PIPELINE HANDOFF`.
    pub fn latest_turn_reported(&self, session_id: Uuid) -> crate::error::Result<bool> {
        let mut stmt = self.conn.prepare(
            "SELECT content FROM conversation_events
              WHERE session_id=?1 AND event_type='Message' AND role='Assistant'
                AND sequence > COALESCE((SELECT MAX(sequence) FROM conversation_events
                     WHERE session_id=?1 AND event_type='Message' AND role='User'), -1)
              ORDER BY sequence DESC LIMIT 64",
        )?;
        let contents = stmt.query_map(params![session_id.to_string()], |row| {
            row.get::<_, String>(0)
        })?;
        for content in contents {
            if is_final_report(&content?) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// #1555: the session has an enabled wake of its own (a `resume` wake or a
    /// daemon-evaluated `when` wake): its next turn is already scheduled, so a
    /// turn that ended without a report is a pending report, not a stranding.
    /// Terminal watches (`on_terminal:*`) are the manager's and never count.
    pub fn owner_has_enabled_own_wake(&self, owner: Uuid) -> crate::error::Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM scheduled_jobs
              WHERE wake_session_id=?1 AND enabled=1
                AND (wake_mode='resume' OR json_type(schedule_json,'$.wake_when') IS NOT NULL))",
            params![owner.to_string()],
            |row| row.get(0),
        )?)
    }

    /// #1588: a completed turn is interim while its own wake or daemon job
    /// is pending. Keep child watches armed until that work settles. A bound
    /// worker with no report and no wake still needs the #1555 stranded notice,
    /// even when it left a job running; a running unit cannot resume it.
    pub fn worker_terminal_watch_pending(&self, session: &Session) -> crate::error::Result<bool> {
        if session.status != SessionStatus::Completed {
            return Ok(false);
        }
        Ok(self.owner_has_enabled_own_wake(session.id)?
            || (self.owner_has_running_agent_job(session.id)?
                && self.worker_stranded_note(session)?.is_none()))
    }

    /// #1555: stranded-worker notice for the launching manager. `Some` when an
    /// Issue-bound worker's turn ended (`Completed`) with no final
    /// `RESULT`/`PIPELINE HANDOFF`/BATON line while nothing of its own will
    /// resume it: no enabled `resume`/`when` wake (a refused or retired wake is
    /// not enabled). The text names every daemon job and unit it still owns,
    /// so the manager can stop or adopt them. A worker with a pending wake, a
    /// final report, or no Issue binding yields `None`.
    pub fn worker_stranded_note(&self, session: &Session) -> crate::error::Result<Option<String>> {
        if session.status != SessionStatus::Completed
            || Self::bound_issue_for_worker_on(&self.conn, session.id)?.is_none()
            || self.latest_turn_reported(session.id)?
            || self.owner_has_enabled_own_wake(session.id)?
        {
            return Ok(None);
        }
        let mut stmt = self.conn.prepare(
            "SELECT id, unit_name FROM agent_jobs
              WHERE owner_session_id=?1 AND state='running' ORDER BY sequence LIMIT 8",
        )?;
        let owned: Vec<(String, String)> = stmt
            .query_map(params![session.id.to_string()], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<Result<_, _>>()?;
        let mut note = String::from("no-result: stranded");
        if !owned.is_empty() {
            let owns: Vec<String> = owned
                .iter()
                .map(|(job, unit)| format!("job {} unit {unit}", &job[..job.len().min(8)]))
                .collect();
            note.push_str(&format!("; owns {}", owns.join(" | ")));
        }
        Ok(Some(note))
    }

    /// #1616: the terminal-watch annotation for a Completed worker: the
    /// stranded notice, else `review: APPROVE|CHANGES` for a reviewer whose
    /// latest turn ended with a verdict line.
    pub fn worker_watch_note(&self, session: &Session) -> crate::error::Result<Option<String>> {
        if let Some(note) = self.worker_stranded_note(session)? {
            return Ok(Some(note));
        }
        if session.status != SessionStatus::Completed {
            return Ok(None);
        }
        Ok(self
            .latest_turn_review_verdict(session.id)?
            .map(|verdict| format!("review: {verdict}")))
    }

    /// #1616: the reviewer verdict in the session's latest turn, if any.
    pub fn latest_turn_review_verdict(
        &self,
        session_id: Uuid,
    ) -> crate::error::Result<Option<&'static str>> {
        let mut stmt = self.conn.prepare(
            "SELECT content FROM conversation_events
              WHERE session_id=?1 AND event_type='Message' AND role='Assistant'
                AND sequence > COALESCE((SELECT MAX(sequence) FROM conversation_events
                     WHERE session_id=?1 AND event_type='Message' AND role='User'), -1)
              ORDER BY sequence DESC LIMIT 64",
        )?;
        let contents = stmt.query_map(params![session_id.to_string()], |row| {
            row.get::<_, String>(0)
        })?;
        for content in contents {
            if let Some(verdict) = review_verdict(&content?) {
                return Ok(Some(verdict));
            }
        }
        Ok(None)
    }

    /// A daemon job the session owns is still running.
    pub fn owner_has_running_agent_job(&self, owner: Uuid) -> crate::error::Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM agent_jobs WHERE owner_session_id=?1 AND state IN ('queued','running') LIMIT 1",
                params![owner.to_string()],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }
}
