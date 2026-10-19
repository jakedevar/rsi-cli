//! Store half of the worker no-result guard (issue #1098): the wake identity
//! constants and the `Store` queries the session-side guard calls. Moved here
//! from `session::worker_result_guard` so `store` has no edge into `session`
//! (issue #1021 S4); the guard re-exports the constants at their old path.

use rsi_common::types::{Session, SessionStatus};
use rusqlite::{OptionalExtension, params};
use uuid::Uuid;

use crate::store::Store;

/// #3209: whether an armed result wake of a session is certain to fire (or
/// time out), see [`Store::owner_result_wake_bound`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResultWakeBound {
    Bounded(Uuid),
    Unbounded {
        wake_id: Uuid,
        /// The `when` predicate as JSON, or the `on_terminal:<session>` mode.
        predicate: String,
        armed_at: chrono::DateTime<chrono::Utc>,
    },
}

/// Marker carried by the automatic continuation message. Its presence in the
/// transcript is the durable "already nudged once" record.
pub const NO_RESULT_MARKER: &str = "[rsid-no-result]";

const NO_RESULT_NAMESPACE: Uuid = Uuid::from_u128(0x1098_0000_0000_4000_8000_0000_0000_0001);

/// Typed refusal: the delivery-time re-proof found the worker ineligible and
/// the wake is retired (never deferred, never delivered).
pub const NO_RESULT_RETIRED: &str = "no_result_continuation_retired";

pub const NO_RESULT_WAKE_NAME_PREFIX: &str = "worker-no-result-";

/// Name prefix of the delivery watch an agent message arms on its recipient
/// (`arm_agent_message_watch_in_tx`): the sender hears if the recipient ends.
pub const AGENT_MESSAGE_WATCH_NAME_PREFIX: &str = "agent-message-";

/// Terminal-watch note for a worker whose final turn has no marker line but
/// whose bound Issue carries its handoff record: read that record before
/// recovering; it may report a held, untested or partial state.
pub const NO_MARKER_ISSUE_HANDOFF_NOTE: &str =
    "no-marker: issue handoff published, read it (not a verified result)";

/// Bound on the parent walk that decides whether a watched session is a
/// worker's launcher.
const LAUNCHER_CHAIN_DEPTH_CAP: usize = 16;

/// True when any line of `text` is a final worker report: a `RESULT ...`
/// line or a `PIPELINE HANDOFF ...` marker (reviewers report `PIPELINE
/// HANDOFF — REVIEW:` with no `RESULT` line, #1392). Leading markup (`**`,
/// `#`, `>`, backticks, list dashes) and markup closing the keyword
/// (`**RESULT**:`) are tolerated; prose such as "the result is" is not a
/// report.
///
/// #2525: a near-miss header (`**IMPLEMENTATION HANDOFF — ...**`, see
/// [`near_miss_handoff_stage`]) is a report too: a worker that clearly
/// finished is not stranded because its header word order differs.
pub fn is_final_report(text: &str) -> bool {
    is_strict_final_report(text) || near_miss_handoff_stage(text).is_some()
}

/// [`is_final_report`] without the #2525 near-miss header forms.
fn is_strict_final_report(text: &str) -> bool {
    text.lines().any(|line| {
        let line = strip_line_markup(line);
        keyword_line(line, "RESULT")
            || keyword_line(line, "PIPELINE HANDOFF")
            || review_verdict_line(line).is_some()
    })
}

/// #2525: the stage of a near-miss handoff header: a line that opens with
/// `<STAGE> HANDOFF` (an uppercase stage word, optional leading markdown
/// emphasis, then end of line or a space, colon, dash or closing markup)
/// instead of `PIPELINE HANDOFF — <STAGE>:`. `PIPELINE HANDOFF` itself is the
/// exact form and is never a near miss; prose that merely mentions the words
/// ("I wrote the IMPLEMENTATION HANDOFF") does not open a line with them.
pub fn near_miss_handoff_stage(text: &str) -> Option<&str> {
    text.lines().find_map(|line| {
        let line = strip_line_markup(line);
        let (stage, rest) = line.split_once(' ')?;
        if stage == "PIPELINE"
            || stage.len() < 2
            || !stage.bytes().all(|b| b.is_ascii_uppercase() || b == b'_')
        {
            return None;
        }
        let rest = rest.strip_prefix("HANDOFF")?;
        let rest = rest.trim_start_matches(['*', '`', '_']);
        (rest.is_empty() || rest.starts_with([' ', ':', '\t', '\u{2014}', '\u{2013}', '-']))
            .then_some(stage)
    })
}

fn strip_line_markup(line: &str) -> &str {
    line.trim_start_matches(|c: char| {
        c.is_whitespace() || matches!(c, '*' | '#' | '>' | '`' | '-' | '_')
    })
}

/// #1935: a turn that only answers a stale wake stays this short.
const NOOP_TURN_MAX_BYTES: usize = 1500;

/// #1935: the message is a daemon scheduled-wake delivery (a `when` wake's
/// body carries its `[wake_when]` status block).
fn is_scheduled_wake_delivery(content: &str) -> bool {
    content.contains("<rsid-daemon-message source=\"scheduled-wake\"")
        || content.contains("[wake_when]")
}

/// #1935: a line of `text` is a `PIPELINE HANDOFF` marker. Prose that merely
/// mentions the words ("I haven't written the PIPELINE HANDOFF yet") is not a
/// marker line; the topology executor uses this too (#2363).
pub fn is_pipeline_handoff(text: &str) -> bool {
    text.lines()
        .any(|line| keyword_line(strip_line_markup(line), "PIPELINE HANDOFF"))
}

/// #1616: a reviewer's closing `REVIEW APPROVE ...` / `REVIEW CHANGES ...`
/// (or `REVIEW REQUEST_CHANGES ...`) line is a final report. Verdicts are ASCII case-insensitive (#1791).
/// Only a verdict word right after `REVIEW` counts
/// (`REVIEW: ACCEPTED` fields sit under a `PIPELINE HANDOFF` marker and prose
/// such as "review approved by" is not a report).
fn review_verdict_line(line: &str) -> Option<&'static str> {
    let rest = line.strip_prefix("REVIEW")?;
    let rest = rest.trim_start_matches(['*', '`', '_', ':', ' ', '\t']);
    if !line["REVIEW".len()..].starts_with([' ', ':', '\t', '*', '`', '_']) {
        return None;
    }
    // The reviewer brief's `REQUEST_CHANGES` spelling is the `CHANGES` verdict.
    [
        ("APPROVE", "APPROVE"),
        ("CHANGES", "CHANGES"),
        ("REQUEST_CHANGES", "CHANGES"),
    ]
    .into_iter()
    .find(|(word, _)| {
        rest.get(..word.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(word))
            && rest.get(word.len()..).is_some_and(|tail| {
                tail.is_empty() || tail.starts_with(|c: char| !c.is_alphanumeric())
            })
    })
    .map(|(_, verdict)| verdict)
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

/// Words a launch-brief line uses to say how the final report must look.
const REPORT_FORMAT_CUES: [&str; 12] = [
    "final report",
    "final message",
    "final line",
    "final answer",
    "result format",
    "report format",
    "prefix",
    "begin with",
    "start with",
    "starts with",
    "end with",
    "report using",
];

/// Keywords the launch brief mentions that are never a custom format.
const NOT_A_CUSTOM_FORMAT: [&str; 4] = ["RESULT", "PIPELINE", "HANDOFF", "STAGE"];

/// #151: the report prefixes a launch brief declares (`LANDED <sha>` in a
/// line such as "end with a final line starting `LANDED <sha>`"). A prefix is
/// the first word of a backticked or quoted span on a line that talks about
/// the final report, when that word is 3+ uppercase letters/underscores and
/// not one of the built-in keywords.
pub fn declared_result_prefixes(brief: &str) -> Vec<String> {
    let mut prefixes: Vec<String> = Vec::new();
    for line in brief.lines() {
        let lower = line.to_ascii_lowercase();
        if !REPORT_FORMAT_CUES.iter().any(|cue| lower.contains(cue)) {
            continue;
        }
        for (index, span) in line.split(['`', '"']).enumerate() {
            // Odd segments sit between two quote marks.
            if index % 2 == 0 {
                continue;
            }
            let word = span.split_whitespace().next().unwrap_or_default();
            let word = word.trim_end_matches([':', ',', '.']);
            let custom = word.len() >= 3
                && word.bytes().all(|b| b.is_ascii_uppercase() || b == b'_')
                && !NOT_A_CUSTOM_FORMAT.contains(&word);
            if custom && !prefixes.iter().any(|known| known == word) {
                prefixes.push(word.to_owned());
            }
        }
    }
    prefixes
}

/// #151: a line of `text` opens with one of the brief's declared prefixes.
fn has_declared_report_line(text: &str, prefixes: &[String]) -> bool {
    text.lines().any(|line| {
        let line = strip_line_markup(line);
        prefixes.iter().any(|prefix| keyword_line(line, prefix))
    })
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

    /// #3218: the session is the reviewer of a DB-native review assignment that
    /// already holds its submitted receipt. The receipt is that reviewer's
    /// result, so a "no final report" nudge only spends a turn (and used to
    /// move the session onto a new invocation).
    pub fn worker_submitted_review_receipt(&self, session_id: Uuid) -> crate::error::Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM manager_review_assignments a
               JOIN manager_review_receipts r ON r.assignment_id=a.assignment_id
              WHERE a.reviewer_session_id=?1)",
            params![session_id.to_string()],
            |row| row.get(0),
        )?)
    }

    /// #2501: a terminal watch owned by `owner` that only tells the worker its
    /// manager or launcher ended (a message-delivery watch, or a watch on a
    /// session in the worker's parent chain) is moot once the worker's latest
    /// turn carries its final report: resuming it spends a turn to repeat what
    /// the handoff already said. Parked workers (no final report in the latest
    /// turn) and watches on anything else (their own children) still fire.
    pub fn finished_worker_launcher_watch(
        &self,
        owner: Uuid,
        watched: &[Uuid],
        job_name: &str,
    ) -> crate::error::Result<bool> {
        let Some(session) = self.get_session(owner)? else {
            return Ok(false);
        };
        if !self.worker_no_result_covered(&session)? {
            return Ok(false);
        }
        let launcher_notice = job_name.starts_with(AGENT_MESSAGE_WATCH_NAME_PREFIX) || {
            let mut chain = Vec::new();
            let mut current = session.parent_id;
            while let Some(id) = current {
                if chain.len() >= LAUNCHER_CHAIN_DEPTH_CAP || chain.contains(&id) {
                    break;
                }
                chain.push(id);
                current = self.get_session(id)?.and_then(|row| row.parent_id);
            }
            watched.iter().any(|id| chain.contains(id))
        };
        if !launcher_notice {
            return Ok(false);
        }
        self.latest_turn_reported(owner)
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
            // #3218: a reviewer holding its submitted receipt has reported.
            if self.worker_submitted_review_receipt(worker)? {
                return Err(retired());
            }
            // #1392: the worker already reported in its latest turn (the
            // arm-time check can miss a report split across messages), so the
            // nudge would only repeat it.
            if self.latest_turn_reported(worker)?
                || self.latest_turn_reported_in_declared_form(worker)?
            {
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
    /// #1935: a short no-op turn that answers a stale scheduled wake after the
    /// worker's report does not replace that report (see
    /// [`Store::report_turn_contents`]).
    pub fn latest_turn_reported(&self, session_id: Uuid) -> crate::error::Result<bool> {
        Ok(self
            .report_turn_contents(session_id)?
            .iter()
            .any(|content| is_final_report(content)))
    }

    /// #3090: the latest turn carries a final report AND the daemon is not
    /// still owed a job result for it. A marker written while the worker's
    /// `when.jobs_terminal` wake is pending is provisional (#2054): the wake
    /// resumes the worker with the result, so the worker is parked, not done
    /// ([`Self::owner_job_result_wake_pending`]). The one finality rule the
    /// worker roster and [`Self::parked_session_pending`] share.
    pub fn latest_turn_final_result(&self, session_id: Uuid) -> crate::error::Result<bool> {
        Ok(self.latest_turn_reported(session_id)?
            && !self.owner_job_result_wake_pending(session_id)?)
    }

    /// Hub #3026: the session's latest turn carries a final `PIPELINE HANDOFF`
    /// marker line (a `RESULT` line or a prose mention does not count).
    pub fn latest_turn_pipeline_handoff(&self, session_id: Uuid) -> crate::error::Result<bool> {
        Ok(self
            .report_turn_contents(session_id)?
            .iter()
            .any(|content| is_pipeline_handoff(content)))
    }

    /// #151: the worker's latest turn reported in a form `is_final_report` does
    /// not know: a line opening with a result prefix its launch brief (the
    /// session's first user message) declares, or a handoff the worker
    /// published to an Issue since its latest non-daemon user message. Either
    /// is the worker saying it is done, so the no-result nudge would only
    /// repeat it. Says nothing about the verdict of that report.
    pub fn latest_turn_reported_in_declared_form(
        &self,
        session_id: Uuid,
    ) -> crate::error::Result<bool> {
        if self.worker_published_handoff_this_turn(session_id)? {
            return Ok(true);
        }
        let brief: Option<String> = self
            .conn
            .query_row(
                "SELECT content FROM conversation_events
                  WHERE session_id=?1 AND event_type='Message' AND role='User'
                  ORDER BY sequence ASC LIMIT 1",
                params![session_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let prefixes = declared_result_prefixes(brief.as_deref().unwrap_or_default());
        if prefixes.is_empty() {
            return Ok(false);
        }
        Ok(self
            .report_turn_contents(session_id)?
            .iter()
            .any(|content| has_declared_report_line(content, &prefixes)))
    }

    /// #151: the worker appended an Issue handoff after the latest user message
    /// that was not a daemon notice (wake deliveries and nudges are daemon
    /// messages and do not start a new unit of work).
    fn worker_published_handoff_this_turn(&self, worker: Uuid) -> crate::error::Result<bool> {
        let turn_start: Option<String> = self
            .conn
            .query_row(
                "SELECT created_at FROM conversation_events
                  WHERE session_id=?1 AND event_type='Message' AND role='User'
                    AND instr(content, '<rsid-daemon-message')=0
                  ORDER BY sequence DESC LIMIT 1",
                params![worker.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM issue_handoffs
              WHERE author_session_id=?1 AND (?2 IS NULL OR created_at >= ?2))",
            params![worker.to_string(), turn_start],
            |row| row.get(0),
        )?)
    }

    /// #1935: the assistant messages (newest first) that decide whether the
    /// session reported. Normally the latest turn. When the latest turn opened
    /// with a daemon `scheduled-wake` delivery and only added a short no-op
    /// reply with no report of its own (a wake that fired after the worker's
    /// handoff, "that wake is stale"), the turn is folded into the previous
    /// one, repeatedly, until a turn carries a report. A session with no report
    /// anywhere, or whose latest turn did real work (a user, manager or
    /// terminal-watch message, or a long reply), keeps the latest turn alone,
    /// so genuine stranding is still detected.
    fn report_turn_contents(&self, session_id: Uuid) -> crate::error::Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT role, content FROM conversation_events
              WHERE session_id=?1 AND event_type='Message' AND role IN ('User','Assistant')
              ORDER BY sequence DESC LIMIT 128",
        )?;
        let rows: Vec<(String, String)> = stmt
            .query_map(params![session_id.to_string()], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<Result<_, _>>()?;
        // `rows` is newest first: split it into turns, each an assistant run
        // followed (further back) by the user message that opened it.
        let mut index = 0;
        let mut latest: Vec<String> = Vec::new();
        let mut noop_bytes = 0;
        let mut first = true;
        while index < rows.len() {
            let mut turn: Vec<String> = Vec::new();
            while index < rows.len() && rows[index].0 == "Assistant" {
                turn.push(rows[index].1.clone());
                index += 1;
            }
            let opener = rows.get(index).map(|(_, content)| content.clone());
            index += 1;
            let reported = turn.iter().any(|content| is_final_report(content));
            if first {
                first = false;
                if reported {
                    return Ok(turn);
                }
                latest = turn.clone();
            } else if reported {
                return Ok(turn);
            }
            noop_bytes += turn.iter().map(String::len).sum::<usize>();
            if noop_bytes > NOOP_TURN_MAX_BYTES
                || !opener.as_deref().is_some_and(is_scheduled_wake_delivery)
            {
                break;
            }
        }
        Ok(latest)
    }

    /// #1935: a daemon-evaluated `when` wake owned by a covered worker is
    /// superseded when the worker wrote its `PIPELINE HANDOFF` after arming it
    /// *and* after the predicate settled (#2054, see
    /// [`Store::latest_handoff_after`]): delivering it would only open a new
    /// turn on finished work, and the turn's no-op reply would replace the
    /// handoff as the final message. `settled_at` is when the predicate became
    /// true; `None` (unknown) never supersedes. A handoff written while the
    /// predicate was still pending (a provisional marker on a parked turn)
    /// does not: the worker has not seen the result yet.
    pub fn wake_superseded_by_handoff(
        &self,
        worker: Uuid,
        armed_at: chrono::DateTime<chrono::Utc>,
        settled_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> crate::error::Result<bool> {
        let Some(settled_at) = settled_at else {
            return Ok(false);
        };
        Ok(self
            .latest_handoff_after(worker, armed_at)?
            .is_some_and(|handoff_at| handoff_at >= settled_at))
    }

    /// #1935: when a covered worker last wrote a `PIPELINE HANDOFF` after
    /// `armed_at`; `None` for an uncovered worker or no such handoff.
    pub fn latest_handoff_after(
        &self,
        worker: Uuid,
        armed_at: chrono::DateTime<chrono::Utc>,
    ) -> crate::error::Result<Option<chrono::DateTime<chrono::Utc>>> {
        let Some(session) = self.get_session(worker)? else {
            return Ok(None);
        };
        if !self.worker_no_result_covered(&session)? {
            return Ok(None);
        }
        let armed = armed_at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let mut stmt = self.conn.prepare(
            "SELECT content, created_at FROM conversation_events
              WHERE session_id=?1 AND event_type='Message' AND role='Assistant'
                AND created_at > ?2
              ORDER BY sequence DESC LIMIT 64",
        )?;
        let rows = stmt.query_map(params![worker.to_string(), armed], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (content, created_at) = row?;
            if is_pipeline_handoff(&content) {
                return Ok(chrono::DateTime::parse_from_rfc3339(&created_at)
                    .ok()
                    .map(|at| at.with_timezone(&chrono::Utc)));
            }
        }
        Ok(None)
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

    /// #1958: the session ended its turn with an armed durable wake of its own
    /// (a `resume` wake, a daemon-evaluated `when` wake, or an `on_terminal`
    /// watch it owns). The armed wake is the pending result: the daemon resumes
    /// the worker when it fires, so no "no result" nudge applies.
    pub fn owner_has_armed_result_wake(&self, owner: Uuid) -> crate::error::Result<bool> {
        Ok(self.owner_armed_result_wake(owner)?.is_some())
    }

    /// #2212: the id of one armed result wake of `owner` (the same set as
    /// [`Self::owner_has_armed_result_wake`]), so a refusal can name it.
    pub fn owner_armed_result_wake(&self, owner: Uuid) -> crate::error::Result<Option<Uuid>> {
        let id: Option<String> = self
            .conn
            .query_row(
                "SELECT id FROM scheduled_jobs
                  WHERE wake_session_id=?1 AND enabled=1
                    AND (wake_mode='resume'
                      OR json_type(schedule_json,'$.wake_when') IS NOT NULL
                      OR wake_mode LIKE 'on_terminal:%')
                  ORDER BY created_at, id LIMIT 1",
                params![owner.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        id.map(|id| {
            Uuid::parse_str(&id).map_err(|e| {
                crate::error::DaemonError::Store(format!("invalid scheduled job id: {e}"))
            })
        })
        .transpose()
    }

    /// #3209: how firmly the armed result wakes of `owner` (the same set as
    /// [`Self::owner_armed_result_wake`]) promise a next turn. `None`: no armed
    /// wake. [`ResultWakeBound::Bounded`]: one of them is certain to fire or
    /// time out: a plain `resume` wake with a readable `next_fire_at` (it fires then; one
    /// without can never become due and is bounded only like an `on_terminal` watch), a
    /// `when` wake with a `deadline`, or a `when` wake / `on_terminal` watch
    /// still inside `horizon` of its arming (`now - armed_at < horizon`).
    /// [`ResultWakeBound::Unbounded`]: every armed wake is a `when` wake without
    /// a deadline or an `on_terminal` watch past the horizon: nothing says it
    /// ever fires (a landing entry that was rejected, an Issue that stays open,
    /// a watched session that is itself parked), so it names the oldest one.
    pub fn owner_result_wake_bound(
        &self,
        owner: Uuid,
        horizon: chrono::Duration,
        now: chrono::DateTime<chrono::Utc>,
    ) -> crate::error::Result<Option<ResultWakeBound>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, wake_mode, schedule_json, created_at, next_fire_at FROM scheduled_jobs
              WHERE wake_session_id=?1 AND enabled=1
                AND (wake_mode='resume'
                  OR json_type(schedule_json,'$.wake_when') IS NOT NULL
                  OR wake_mode LIKE 'on_terminal:%')
              ORDER BY created_at, id LIMIT 64",
        )?;
        let rows = stmt
            .query_map(params![owner.to_string()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut unbounded = None;
        for (id, mode, schedule_json, created_at, next_fire_at) in rows {
            let id = Uuid::parse_str(&id).map_err(|e| {
                crate::error::DaemonError::Store(format!("invalid scheduled job id: {e}"))
            })?;
            let schedule: serde_json::Value =
                serde_json::from_str(&schedule_json).unwrap_or(serde_json::Value::Null);
            let when = schedule
                .get("wake_when")
                .filter(|value| !value.is_null())
                .and_then(|value| {
                    serde_json::from_value::<rsi_common::wake_predicate::WakeWhenState>(
                        value.clone(),
                    )
                    .ok()
                });
            let armed_at = when
                .as_ref()
                .map(|state| state.armed_at)
                .or_else(|| {
                    chrono::DateTime::parse_from_rfc3339(&created_at)
                        .ok()
                        .map(|at| at.with_timezone(&chrono::Utc))
                })
                .unwrap_or(now);
            let in_horizon = now.signed_duration_since(armed_at) < horizon;
            let bounded = match &when {
                Some(state) => state.deadline.is_some() || in_horizon,
                None if mode.starts_with("on_terminal:") => in_horizon,
                // #3221: a plain resume wake is certain to fire only when it has a
                // readable fire time; one without can never become due, so it
                // is bounded only inside the horizon of its arming.
                None if next_fire_at
                    .as_deref()
                    .is_some_and(|at| chrono::DateTime::parse_from_rfc3339(at).is_ok()) =>
                {
                    true
                }
                None => in_horizon,
            };
            if bounded {
                return Ok(Some(ResultWakeBound::Bounded(id)));
            }
            if unbounded.is_none() {
                let predicate = match &when {
                    Some(state) => serde_json::to_string(&state.predicate)
                        .unwrap_or_else(|_| "unreadable predicate".to_owned()),
                    None => mode.clone(),
                };
                unbounded = Some(ResultWakeBound::Unbounded {
                    wake_id: id,
                    predicate,
                    armed_at,
                });
            }
        }
        Ok(unbounded)
    }

    /// #2212 / #2144: the session ended its turn without a final report but
    /// the daemon will resume it, because it owns an armed wake or a non-terminal
    /// daemon job. Returns the pending item (`pending wake <id>` or `running job
    /// <id>`), `None` when nothing will resume it or it already reported.
    ///
    /// This is the one liveness predicate for a *parked* worker, shared by the
    /// one-worker-per-Issue launch check and durable agent mail (a parked
    /// session accepts mail that is delivered at its next resume). It does not
    /// read the session status: the caller decides that only a `Completed`
    /// session can be parked.
    pub fn parked_session_pending(&self, session_id: Uuid) -> crate::error::Result<Option<String>> {
        if self.latest_turn_final_result(session_id)? {
            return Ok(None);
        }
        if let Some(wake) = self.owner_armed_result_wake(session_id)? {
            return Ok(Some(format!("pending wake {wake}")));
        }
        if let Some(job) = self.owner_running_agent_job(session_id)? {
            return Ok(Some(format!("running job {job}")));
        }
        Ok(None)
    }

    /// #1588: a completed turn is interim while its own wake or daemon job
    /// is pending. Keep child watches armed until that work settles. A bound
    /// worker with no report and no wake still needs the #1555 stranded notice,
    /// even when it left a job running; a running unit cannot resume it.
    ///
    /// Hub #2987: an `Interrupted` turn the graceful-restart journal still owns
    /// (a deploy `interrupt_workers` cut, #1461) is interim too: the next process
    /// continues it, so the watch stays armed and fires on the continuation's
    /// real end. An intent that failed (`not_resumable`) or waits on a hold (`operator_paused`, ...) no longer
    /// owns the session, so the interruption fires the watch as any other.
    pub fn worker_terminal_watch_pending(&self, session: &Session) -> crate::error::Result<bool> {
        if session.status == SessionStatus::Interrupted {
            return self.restart_intent_will_continue_session(session.id);
        }
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
    /// final report, or no Issue binding yields `None`. A worker that published
    /// a handoff to its bound Issue but left the marker out of its final turn
    /// gets [`NO_MARKER_ISSUE_HANDOFF_NOTE`] instead of "no-result: stranded"
    /// (#187): the record exists, but only its text says whether it is a result,
    /// a hold or a partial, so the note never implies green.
    pub fn worker_stranded_note(&self, session: &Session) -> crate::error::Result<Option<String>> {
        if session.status != SessionStatus::Completed {
            return Ok(None);
        }
        let Some((_, issue)) = Self::bound_issue_for_worker_on(&self.conn, session.id)? else {
            return Ok(None);
        };
        if self.latest_turn_reported(session.id)? || self.owner_has_enabled_own_wake(session.id)? {
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
        // #2362 parity: the roster calls a worker that published an Issue handoff
        // `final_handoff`. The watch keeps the missing marker visible but names
        // the durable record instead of "lost"; the text claims no verdict, so a
        // held or untested handoff is never read as a verified result.
        let mut note = String::from(if self.worker_issue_handoff_published(session.id, issue)? {
            NO_MARKER_ISSUE_HANDOFF_NOTE
        } else {
            "no-result: stranded"
        });
        if !owned.is_empty() {
            let owns: Vec<String> = owned
                .iter()
                .map(|(job, unit)| format!("job {} unit {unit}", &job[..job.len().min(8)]))
                .collect();
            note.push_str(&format!("; owns {}", owns.join(" | ")));
        }
        Ok(Some(note))
    }

    /// The worker appended a handoff record to its bound Issue (`AgentUpdateIssue
    /// {handoff}`). Says nothing about the handoff's verdict.
    fn worker_issue_handoff_published(
        &self,
        worker: Uuid,
        issue: Uuid,
    ) -> crate::error::Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM issue_handoffs WHERE issue_id=?1 AND author_session_id=?2)",
            params![issue.to_string(), worker.to_string()],
            |row| row.get(0),
        )?)
    }

    /// #1616: the terminal-watch annotation for a Completed worker: the
    /// stranded notice, else `review: APPROVE|CHANGES` for a reviewer whose
    /// latest turn ended with a verdict line.
    pub fn worker_watch_note(&self, session: &Session) -> crate::error::Result<Option<String>> {
        // #2109: what the daemon did about a provider capacity failure.
        let capacity = self.capacity_watch_note(session)?;
        let result = if let Some(note) = self.worker_stranded_note(session)? {
            Some(note)
        } else if session.status != SessionStatus::Completed {
            None
        } else {
            self.latest_turn_review_verdict(session.id)?
                .map(|verdict| format!("review: {verdict}"))
        };
        // #2525: the report was accepted through a near-miss header.
        let result = match (result, self.latest_turn_near_miss_stage(session)?) {
            (Some(result), Some(stage)) => {
                Some(format!("{result}, handoff_header_normalized: {stage}"))
            }
            (None, Some(stage)) => Some(format!("handoff_header_normalized: {stage}")),
            (result, None) => result,
        };
        Ok(match (capacity, result) {
            (Some(capacity), Some(result)) => Some(format!("{capacity}, {result}")),
            (capacity, result) => capacity.or(result),
        })
    }

    /// #2525: the stage of a near-miss handoff header when the Completed
    /// worker's latest turn reported only through it (no exact `RESULT` or
    /// `PIPELINE HANDOFF` line), else `None`.
    fn latest_turn_near_miss_stage(
        &self,
        session: &Session,
    ) -> crate::error::Result<Option<String>> {
        if session.status != SessionStatus::Completed {
            return Ok(None);
        }
        let contents = self.report_turn_contents(session.id)?;
        if contents
            .iter()
            .any(|content| is_strict_final_report(content))
        {
            return Ok(None);
        }
        Ok(contents
            .iter()
            .find_map(|content| near_miss_handoff_stage(content).map(str::to_owned)))
    }

    /// #1616: the reviewer verdict in the session's latest turn, if any.
    pub fn latest_turn_review_verdict(
        &self,
        session_id: Uuid,
    ) -> crate::error::Result<Option<&'static str>> {
        Ok(self
            .report_turn_contents(session_id)?
            .iter()
            .find_map(|content| review_verdict(content)))
    }

    /// #1734: a daemon job the session owns is still running AND the session
    /// has an enabled `when.jobs_terminal` wake naming it. The worker ended its
    /// turn on purpose to be resumed with the job's result, so a foreground-wait
    /// nudge would contradict the published completion-wake workflow.
    pub fn owner_running_job_has_completion_wake(&self, owner: Uuid) -> crate::error::Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(
               SELECT 1 FROM scheduled_jobs w
                 JOIN json_each(w.schedule_json,'$.wake_when.predicate.jobs_terminal') ids
                 JOIN agent_jobs j ON j.id=ids.value
               WHERE w.wake_session_id=?1 AND w.enabled=1 AND json_valid(w.schedule_json)
                 AND j.owner_session_id=?1 AND j.state IN ('queued','running'))",
            params![owner.to_string()],
            |row| row.get(0),
        )?)
    }

    /// #3090: the worker owns an enabled `when.jobs_terminal` wake whose result
    /// the daemon will still deliver, so a `PIPELINE HANDOFF` line in its latest
    /// turn is provisional (#2054), not a final result. The wake delivers unless
    /// the predicate had settled before that handoff
    /// ([`Self::wake_superseded_by_handoff`]): while a named job is queued or
    /// running the predicate is pending, and once every job finished the wake is
    /// still owed when the worker's latest handoff after arming it predates the
    /// last finish or an unknown finish time (`None` never supersedes, as the
    /// scheduler reads it). A wake naming a job the worker does not own never
    /// counts, as in [`Self::owner_running_job_has_completion_wake`].
    pub fn owner_job_result_wake_pending(&self, owner: Uuid) -> crate::error::Result<bool> {
        let mut wakes = self.conn.prepare(
            "SELECT w.created_at,
                    json_extract(w.schedule_json,'$.wake_when.predicate.jobs_terminal')
               FROM scheduled_jobs w
              WHERE w.wake_session_id=?1 AND w.enabled=1 AND json_valid(w.schedule_json)
                AND json_type(w.schedule_json,'$.wake_when.predicate.jobs_terminal')='array'",
        )?;
        let wakes = wakes
            .query_map(params![owner.to_string()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (created_at, ids) in wakes {
            let Ok(ids) = serde_json::from_str::<Vec<String>>(&ids) else {
                continue;
            };
            let Some(armed_at) = chrono::DateTime::parse_from_rfc3339(&created_at)
                .ok()
                .map(|at| at.with_timezone(&chrono::Utc))
            else {
                continue;
            };
            let mut settled: Option<Option<chrono::DateTime<chrono::Utc>>> = Some(None);
            let mut named = 0usize;
            for id in &ids {
                let job: Option<(String, Option<String>)> = self
                    .conn
                    .query_row(
                        "SELECT state, finished_at FROM agent_jobs
                          WHERE id=?1 AND owner_session_id=?2",
                        params![id, owner.to_string()],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?;
                let Some((state, finished_at)) = job else {
                    continue;
                };
                named += 1;
                if matches!(state.as_str(), "queued" | "running") {
                    return Ok(true);
                }
                let finished = finished_at.and_then(|at| {
                    chrono::DateTime::parse_from_rfc3339(&at)
                        .ok()
                        .map(|at| at.with_timezone(&chrono::Utc))
                });
                settled = match (settled, finished) {
                    (Some(prior), Some(at)) => Some(Some(prior.map_or(at, |p| p.max(at)))),
                    _ => None,
                };
            }
            if named == 0 {
                continue;
            }
            // Every named job ended: owed unless the handoff came after the
            // last finish. An unknown finish time never supersedes.
            let settled_at = settled.flatten();
            if !self.wake_superseded_by_handoff(owner, armed_at, settled_at)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// A daemon job the session owns is still running.
    pub fn owner_has_running_agent_job(&self, owner: Uuid) -> crate::error::Result<bool> {
        Ok(self.owner_running_agent_job(owner)?.is_some())
    }

    /// #2212: the id of one non-terminal daemon job the session owns, so a
    /// refusal can name it.
    pub fn owner_running_agent_job(&self, owner: Uuid) -> crate::error::Result<Option<Uuid>> {
        let id: Option<String> = self
            .conn
            .query_row(
                "SELECT id FROM agent_jobs WHERE owner_session_id=?1 AND state IN ('queued','running') ORDER BY sequence LIMIT 1",
                params![owner.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        id.map(|id| {
            Uuid::parse_str(&id)
                .map_err(|e| crate::error::DaemonError::Store(format!("invalid agent job id: {e}")))
        })
        .transpose()
    }
}

#[cfg(test)]
mod near_miss_tests {
    use super::{is_final_report, near_miss_handoff_stage};

    /// #2525: header word order, markdown emphasis and a missing colon do not
    /// make a finished worker look stranded.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn near_miss_handoff_headers_are_final_reports() {
        for (text, stage) in [
            (
                "**IMPLEMENTATION HANDOFF — role: research, budget: 250 tokens**",
                "IMPLEMENTATION",
            ),
            ("IMPLEMENTATION HANDOFF", "IMPLEMENTATION"),
            ("## RESEARCH HANDOFF: done", "RESEARCH"),
            ("prose first\n`REVIEW HANDOFF - ok`", "REVIEW"),
        ] {
            assert_eq!(near_miss_handoff_stage(text), Some(stage), "{text}");
            assert!(is_final_report(text), "{text}");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn prose_and_exact_headers_are_not_near_misses() {
        for text in [
            "PIPELINE HANDOFF — IMPLEMENTATION:",
            "I wrote the IMPLEMENTATION HANDOFF already",
            "the implementation handoff is ready",
            "IMPLEMENTATION HANDOFFS are listed below",
            "all done, tests pass",
        ] {
            assert_eq!(near_miss_handoff_stage(text), None, "{text}");
        }
        assert!(is_final_report("PIPELINE HANDOFF — IMPLEMENTATION:"));
        assert!(!is_final_report("all done, tests pass"));
    }
}
