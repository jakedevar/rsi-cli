//! `AgentManagerLaunchIssueWorker` (#1100): admission of one Issue-bound
//! `create_session` action. The action receipt, the Issue note naming the
//! worker, the Issue's InProgress status and the Issue binding all commit in
//! ONE immediate transaction: any refusal (policy, allowlist, capacity, Issue
//! state, authority) rolls everything back, leaving no action, note or status
//! change. The binding is the action's own journalled payload, so no schema
//! change is needed.

use super::issue_worker_capacity::{CAPACITY_MAX_RETRIES, CapacityRetryV1};
use super::issue_worker_evidence::handoff_names_verification;
use super::{
    ACTION_KIND, ManagerActionOperationV2, ManagerActionOriginV2, ManagerIssueBindingV1, state_name,
};
use crate::error::Result;
use crate::store::Store;
use crate::store::harness_manager_v2::{ManagerCallerV1, refused};
use rsi_common::global_manager::{MANAGER_ISSUE_WORKER_ALREADY_LIVE, MANAGER_PROJECT_NOT_IN_SCOPE};
use rsi_common::harness_manager_v2::{
    AgentManagerControlRequestV2, ManagerActionReceiptV2, ManagerActionStateV2, ManagerActionV2,
    ManagerCapabilityV2, ManagerFenceV2, ManagerSandboxSourceV1,
};
use rsi_common::manager_issue_worker::{
    AgentManagerLaunchIssueWorkerRequestV1, MANAGER_ISSUE_REFERENCE_SELECTOR_MAX,
    MANAGER_ISSUE_WORKER_INVALID_REQUEST, MANAGER_ISSUE_WORKER_PARENT_CANDIDATES_MAX,
    MANAGER_ISSUE_WORKER_PARENT_REQUIRED, MANAGER_ISSUE_WORKER_PREDECESSOR_LIVE,
    MANAGER_ISSUE_WORKER_PREDECESSOR_OTHER_ISSUE, MANAGER_ISSUE_WORKER_PREDECESSOR_OUT_OF_SCOPE,
    MANAGER_ISSUE_WORKER_PREDECESSOR_SANDBOX_UNAVAILABLE, MANAGER_ISSUE_WORKER_REFERENCES_OVERFLOW,
    MANAGER_ISSUE_WORKER_RELATED_MAX, MANAGER_ISSUE_WORKER_RELATED_UNAVAILABLE,
    MANAGER_ISSUE_WORKER_REVIEWED_UNAVAILABLE, bounded_issue_reference_selectors,
    text_references_issue,
};
use rsi_common::rpc::{AgentUpdateIssueRequestV1, AgentUpdateIssueStatusRequestV1};
use rsi_common::types::{Issue, IssueStatus, SessionKind, SessionStatus};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use uuid::Uuid;

/// The admitted launch and the Issue as the call left it.
#[derive(Debug, Clone)]
pub struct ManagerIssueWorkerAdmission {
    pub receipt: ManagerActionReceiptV2,
    pub worker_session_id: Uuid,
    pub issue: Issue,
    /// #275: earlier launches' related Issues a plain relaunch carried over.
    pub carried_related: Vec<i64>,
    /// #275: earlier related Issues skipped (gone, archived or over the bound).
    pub skipped_related: Vec<i64>,
}

/// #218: how many of the bound Issue's newest handoff records a read check
/// scans for `#N` selectors. A bound Issue holds a handful; the bound keeps the
/// check cheap, and a selector older than this is simply not granted.
const BOUND_HANDOFF_SCAN_MAX: i64 = 64;

/// #218: the read scope one Issue-bound launch journals in its binding: the
/// `related` Issues the manager attached and the selectors its brief named.
/// Both are launch snapshots. Neither grants any write.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchReadScope {
    pub related: Vec<i64>,
    pub brief_refs: Vec<(i64, i64)>,
}

impl LaunchReadScope {
    /// Add a predecessor's reads (its attached Issues become single-number
    /// selectors), deduplicated. Refuses when the result would exceed the
    /// selector bound.
    ///
    /// # Errors
    /// [`MANAGER_ISSUE_WORKER_REFERENCES_OVERFLOW`].
    fn inherit(&mut self, previous: Self) -> std::result::Result<(), &'static str> {
        let carried = previous
            .brief_refs
            .into_iter()
            .chain(previous.related.into_iter().map(|n| (n, n)));
        for selector in carried {
            if !self.brief_refs.contains(&selector) {
                self.brief_refs.push(selector);
            }
        }
        if self.brief_refs.len() > MANAGER_ISSUE_REFERENCE_SELECTOR_MAX {
            return Err(MANAGER_ISSUE_WORKER_REFERENCES_OVERFLOW);
        }
        Ok(())
    }

    /// Whether the launch itself selects the Issue `number`.
    fn selects(&self, number: i64) -> bool {
        self.related.contains(&number)
            || self
                .brief_refs
                .iter()
                .any(|(from, to)| (*from..=*to).contains(&number))
    }
}

fn unavailable() -> crate::error::DaemonError {
    refused("manager_issue_worker_issue_unavailable")
}

impl Store {
    /// The `(project, Issue)` a worker was bound to by a launch that created
    /// it (`running` or `succeeded` action). Read-only; used for the worker's
    /// own-Issue read authority and its catalog projection. "Latest" is
    /// admission order (`rowid`, #1322), as in
    /// [`Self::live_issue_binding_for_worker_on`].
    pub(crate) fn bound_issue_for_worker_on(
        conn: &Connection,
        worker: Uuid,
    ) -> Result<Option<(Uuid, Uuid)>> {
        let found: Option<(String, String)> = conn
            .query_row(
                "SELECT o.project_id, json_extract(o.payload_json,'$.issue_binding.issue_id')
                 FROM harness_manager_v2_operations o
                 JOIN issues i ON i.project_id=o.project_id
                  AND i.id=json_extract(o.payload_json,'$.issue_binding.issue_id')
                 WHERE o.kind=?1 AND o.target_session_id=?2
                   AND o.state IN ('running','succeeded')
                 ORDER BY o.rowid DESC LIMIT 1",
                params![ACTION_KIND, worker.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        found
            .map(|(project, issue)| {
                Ok((
                    Uuid::parse_str(&project)
                        .map_err(|_| refused("manager_v2_invalid_stored_identity"))?,
                    Uuid::parse_str(&issue)
                        .map_err(|_| refused("manager_v2_invalid_stored_identity"))?,
                ))
            })
            .transpose()
    }

    /// #2311: the display number (the N in #N) of the Issue `worker` was last
    /// launched on, from the same binding as [`Self::bound_issue_for_worker_on`]
    /// (`running` or `succeeded` launch, latest admission). Read-only; the
    /// worker roster shows it.
    pub(crate) fn bound_issue_display_for_worker_on(
        conn: &Connection,
        worker: Uuid,
    ) -> Result<Option<u32>> {
        let number: Option<i64> = conn
            .query_row(
                "SELECT i.display_number
                 FROM harness_manager_v2_operations o
                 JOIN issues i ON i.project_id=o.project_id
                  AND i.id=json_extract(o.payload_json,'$.issue_binding.issue_id')
                 WHERE o.kind=?1 AND o.target_session_id=?2
                   AND o.state IN ('running','succeeded')
                 ORDER BY o.rowid DESC LIMIT 1",
                params![ACTION_KIND, worker.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(number.and_then(|number| u32::try_from(number).ok()))
    }

    /// #218: the read scope a worker's latest Issue-bound launch journalled
    /// (the same launch as [`Self::bound_issue_for_worker_on`]): the attached
    /// `related` Issues and the selectors of the manager's brief. A binding
    /// that predates #218 holds neither.
    pub(crate) fn launch_read_scope_for_worker_on(
        conn: &Connection,
        worker: Uuid,
    ) -> Result<Option<LaunchReadScope>> {
        let found: Option<(Option<String>, Option<String>)> = conn
            .query_row(
                "SELECT json_extract(o.payload_json,'$.issue_binding.related'),
                        json_extract(o.payload_json,'$.issue_binding.brief_refs')
                 FROM harness_manager_v2_operations o
                 JOIN issues i ON i.project_id=o.project_id
                  AND i.id=json_extract(o.payload_json,'$.issue_binding.issue_id')
                 WHERE o.kind=?1 AND o.target_session_id=?2
                   AND o.state IN ('running','succeeded')
                 ORDER BY o.rowid DESC LIMIT 1",
                params![ACTION_KIND, worker.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        // A malformed journal fails closed: it grants no extra read.
        Ok(found.map(|(related, refs)| LaunchReadScope {
            related: related
                .and_then(|json| serde_json::from_str(&json).ok())
                .unwrap_or_default(),
            brief_refs: refs
                .and_then(|json| serde_json::from_str(&json).ok())
                .unwrap_or_default(),
        }))
    }

    /// #218: whether `target` (an Issue of `project`) is one the bound worker
    /// `worker` may read because its bound Issue `bound` references it, read
    /// authority only. The sources are, in order: the launch's attached
    /// `related` Issues and the selectors of the manager's brief (both
    /// snapshots from launch), then the bound Issue's body and its newest
    /// [`BOUND_HANDOFF_SCAN_MAX`] handoff records, read now. Only the bound
    /// Issue's own text is scanned: what a referenced Issue says selects
    /// nothing further. The scan is a membership test on the selectors
    /// ([`text_references_issue`]); it never follows or runs anything.
    ///
    /// Worker-authored handoffs are a source (the PM's same-project intent): a
    /// worker can extend its own read set inside its project, read-only. It
    /// cannot widen across projects, since `target` is resolved in the bound
    /// project first, and it gains no write.
    pub(crate) fn issue_is_referenced_by_binding(
        conn: &Connection,
        worker: Uuid,
        project: Uuid,
        bound: Uuid,
        target: Uuid,
    ) -> Result<bool> {
        let number: Option<i64> = conn
            .query_row(
                "SELECT display_number FROM issues WHERE id=?1 AND project_id=?2",
                params![target.to_string(), project.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(number) = number else {
            return Ok(false);
        };
        let launched = Self::launch_read_scope_for_worker_on(conn, worker)?
            .is_some_and(|scope| scope.selects(number));
        if launched {
            return Ok(true);
        }
        let body: Option<String> = conn
            .query_row(
                "SELECT body FROM issues WHERE id=?1 AND project_id=?2",
                params![bound.to_string(), project.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if body.is_some_and(|body| text_references_issue(&body, number)) {
            return Ok(true);
        }
        let mut statement = conn.prepare(
            "SELECT body FROM issue_handoffs WHERE issue_id=?1 ORDER BY sequence DESC LIMIT ?2",
        )?;
        let handoffs = statement
            .query_map(params![bound.to_string(), BOUND_HANDOFF_SCAN_MAX], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(handoffs
            .iter()
            .any(|handoff| text_references_issue(handoff, number)))
    }

    /// #1845: whether `candidate` is an Issue a review worker was launched on
    /// (`review_of`) for the Issue `reviewed`, in `project`. Read authority
    /// only: it lets a worker bound to `reviewed` read its own reviews.
    pub(crate) fn issue_is_review_of(
        conn: &Connection,
        project: Uuid,
        candidate: Uuid,
        reviewed: Uuid,
    ) -> Result<bool> {
        Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM harness_manager_v2_operations o
             JOIN issues r ON r.id=?4 AND r.project_id=o.project_id
             WHERE o.kind=?1 AND o.project_id=?2 AND o.state IN ('running','succeeded')
               AND json_extract(o.payload_json,'$.issue_binding.issue_id')=?3
               AND json_extract(o.payload_json,'$.issue_binding.review_of')=r.display_number)",
            params![
                ACTION_KIND,
                project.to_string(),
                candidate.to_string(),
                reviewed.to_string()
            ],
            |row| row.get(0),
        )?)
    }

    /// #1845: the Issues a review worker was launched on for the Issue
    /// `reviewed_display`, newest launch first (admission order, #1322).
    fn review_issue_ids(
        conn: &Connection,
        project: Uuid,
        reviewed_display: i64,
    ) -> Result<Vec<Uuid>> {
        let mut statement = conn.prepare(
            "SELECT json_extract(o.payload_json,'$.issue_binding.issue_id')
             FROM harness_manager_v2_operations o
             WHERE o.kind=?1 AND o.project_id=?2 AND o.state IN ('running','succeeded')
               AND json_extract(o.payload_json,'$.issue_binding.review_of')=?3
             GROUP BY 1 ORDER BY MAX(o.rowid) DESC LIMIT 8",
        )?;
        let raw = statement
            .query_map(
                params![ACTION_KIND, project.to_string(), reviewed_display],
                |row| row.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        raw.iter()
            .map(|id| {
                Uuid::parse_str(id).map_err(|_| refused("manager_v2_invalid_stored_identity"))
            })
            .collect()
    }

    /// #1845: the manager's brief with the latest review of every Issue
    /// launched with `review_of` = this Issue appended (see
    /// [`revision_brief`]). Unchanged when the Issue has no review.
    fn brief_with_reviews(
        tx: &Transaction<'_>,
        project: Uuid,
        issue_id: Uuid,
        brief: &str,
    ) -> Result<String> {
        let Some(issue) = Self::get_issue_in_project_tx(tx, project, issue_id)? else {
            return Ok(brief.to_string());
        };
        let reviews = Self::review_issue_ids(tx, project, issue.display_number)?
            .into_iter()
            .filter(|id| *id != issue_id)
            .map(|id| Self::get_issue_in_project_tx(tx, project, id))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        Ok(revision_brief(&issue, &reviews, brief))
    }

    /// #275: the `related` Issues of this Issue's earlier launches (newest
    /// launch first), as the manager attached them and as an earlier plain
    /// relaunch carried them. `idempotency_key` excludes the launch itself.
    fn earlier_related_numbers(
        &self,
        project: Uuid,
        issue_id: Uuid,
        idempotency_key: &str,
    ) -> Result<Vec<i64>> {
        let mut statement = self.conn.prepare(
            "SELECT json_extract(payload_json,'$.issue_binding.related'),
                    json_extract(payload_json,'$.issue_binding.carried_related')
             FROM harness_manager_v2_operations
             WHERE kind=?1 AND project_id=?2 AND state IN ('running','succeeded')
               AND json_extract(payload_json,'$.issue_binding.issue_id')=?3
               AND idempotency_key<>?4
             ORDER BY rowid DESC LIMIT 16",
        )?;
        let rows = statement
            .query_map(
                params![
                    ACTION_KIND,
                    project.to_string(),
                    issue_id.to_string(),
                    idempotency_key
                ],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        // A malformed journal fails closed: it carries nothing.
        let parse = |json: Option<String>| -> Vec<i64> {
            json.and_then(|json| serde_json::from_str(&json).ok())
                .unwrap_or_default()
        };
        let mut numbers = Vec::new();
        for (related, carried) in rows {
            for number in parse(related).into_iter().chain(parse(carried)) {
                if !numbers.contains(&number) {
                    numbers.push(number);
                }
            }
        }
        Ok(numbers)
    }

    /// #275: a plain relaunch (no `continue_from`, no `review_of`) keeps the
    /// read-only `related` Issues its Issue's earlier launches carried, added to
    /// the new launch's own: a union, deduplicated, within the `related` bound.
    /// A number that no longer names a live Issue of the project (gone or
    /// archived) or that does not fit a bound is skipped and named; it never
    /// refuses the launch. Carried numbers also join `scope.brief_refs`, which
    /// is what grants the worker the read. Returns `(carried, skipped)`.
    fn carry_earlier_related(
        &self,
        tx: &Transaction<'_>,
        project: Uuid,
        issue_id: Uuid,
        request: &AgentManagerLaunchIssueWorkerRequestV1,
        scope: &mut LaunchReadScope,
    ) -> Result<(Vec<i64>, Vec<i64>)> {
        let mut carried: Vec<i64> = Vec::new();
        let mut skipped: Vec<i64> = Vec::new();
        let earlier = self.earlier_related_numbers(project, issue_id, &request.idempotency_key)?;
        for number in earlier {
            if number == request.issue || request.related.contains(&number) {
                continue;
            }
            let live = Self::resolve_agent_issue_target_tx(tx, project, None, Some(number))
                .ok()
                .map(|id| Self::get_issue_in_project_tx(tx, project, id))
                .transpose()?
                .flatten()
                .is_some_and(|issue| issue.archived_at.is_none());
            let selector = (number, number);
            let fits = request.related.len() + carried.len() < MANAGER_ISSUE_WORKER_RELATED_MAX
                && (scope.brief_refs.contains(&selector)
                    || scope.brief_refs.len() < MANAGER_ISSUE_REFERENCE_SELECTOR_MAX);
            if !live || !fits {
                if skipped.len() < MANAGER_ISSUE_WORKER_RELATED_MAX * 2 {
                    skipped.push(number);
                }
                continue;
            }
            if !scope.brief_refs.contains(&selector) {
                scope.brief_refs.push(selector);
            }
            carried.push(number);
        }
        Ok((carried, skipped))
    }

    /// #275: the carried and skipped numbers an earlier launch under the same
    /// idempotency key journalled, so a replay reports and keeps what the
    /// original did.
    fn journalled_carry(
        &self,
        project: Uuid,
        idempotency_key: &str,
        issue_id: Uuid,
    ) -> Result<(Vec<i64>, Vec<i64>)> {
        let found: Option<(Option<String>, Option<String>)> = self
            .conn
            .query_row(
                "SELECT json_extract(payload_json,'$.issue_binding.carried_related'),
                        json_extract(payload_json,'$.issue_binding.skipped_related')
                 FROM harness_manager_v2_operations
                 WHERE kind=?1 AND project_id=?2 AND idempotency_key=?3
                   AND json_extract(payload_json,'$.issue_binding.issue_id')=?4
                 ORDER BY rowid DESC LIMIT 1",
                params![
                    ACTION_KIND,
                    project.to_string(),
                    idempotency_key,
                    issue_id.to_string()
                ],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let parse = |json: Option<String>| -> Vec<i64> {
            json.and_then(|json| serde_json::from_str(&json).ok())
                .unwrap_or_default()
        };
        Ok(found.map_or_else(
            || (Vec::new(), Vec::new()),
            |(carried, skipped)| (parse(carried), parse(skipped)),
        ))
    }

    /// #218: the brief with the complete text and latest handoff of each
    /// `related` Issue appended (see [`related_brief`]). A number that names
    /// no Issue of `project` refuses the launch before any effect.
    fn brief_with_related(
        tx: &Transaction<'_>,
        project: Uuid,
        publication: i64,
        related: &[i64],
        carry: (&[i64], &[i64]),
        brief: &str,
    ) -> Result<String> {
        let unavailable = || refused(MANAGER_ISSUE_WORKER_RELATED_UNAVAILABLE);
        let mut issues = Vec::with_capacity(related.len() + carry.0.len());
        for (position, number) in related.iter().chain(carry.0).enumerate() {
            let id = Self::resolve_agent_issue_target_tx(tx, project, None, Some(*number))
                .map_err(|_| unavailable())?;
            let issue = Self::get_issue_in_project_tx(tx, project, id)?.ok_or_else(unavailable)?;
            let stored = Self::newest_stored_handoff(tx, id, &issue.body, review_handoff_start)?;
            issues.push((issue, stored, position >= related.len()));
        }
        Ok(related_brief(publication, &issues, carry.1, brief))
    }

    /// #218: a replay under the same idempotency key must attach the same
    /// related Issues the journalled launch did; any other change is
    /// `manager_v2_idempotency_conflict`, never a silent replay.
    fn ensure_related_replay(
        &self,
        project: Uuid,
        idempotency_key: &str,
        issue_id: Uuid,
        related: &[i64],
    ) -> Result<()> {
        let journalled: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT json_extract(payload_json,'$.issue_binding.related')
                 FROM harness_manager_v2_operations
                 WHERE kind=?1 AND project_id=?2 AND idempotency_key=?3
                   AND json_extract(payload_json,'$.issue_binding.issue_id')=?4
                 ORDER BY rowid DESC LIMIT 1",
                params![
                    ACTION_KIND,
                    project.to_string(),
                    idempotency_key,
                    issue_id.to_string()
                ],
                |row| row.get(0),
            )
            .optional()?;
        let Some(journalled) = journalled else {
            return Ok(());
        };
        let before: Vec<i64> = journalled
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default();
        if before == related {
            return Ok(());
        }
        Err(refused("manager_v2_idempotency_conflict"))
    }

    /// #1845: the journalled operation of an earlier Issue-bound launch under
    /// the same idempotency key, so a replay matches it even when a review
    /// has grown since.
    fn prior_issue_operation(
        &self,
        project: Uuid,
        idempotency_key: &str,
        issue_id: Uuid,
    ) -> Result<Option<ManagerActionV2>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT json_extract(payload_json,'$.request.operation')
                 FROM harness_manager_v2_operations
                 WHERE kind=?1 AND project_id=?2 AND idempotency_key=?3
                   AND json_extract(payload_json,'$.issue_binding.issue_id')=?4
                 ORDER BY rowid DESC LIMIT 1",
                params![
                    ACTION_KIND,
                    project.to_string(),
                    idempotency_key,
                    issue_id.to_string()
                ],
                |row| row.get(0),
            )
            .optional()?;
        raw.map(|json| serde_json::from_str(&json).map_err(Into::into))
            .transpose()
    }

    /// #1284: the worker's binding when it is live now: the worker's latest
    /// Issue-bound launch (as [`Self::bound_issue_for_worker_on`]), the worker
    /// session still live, and no later launch of the same Issue (a #1254
    /// successor, or a relaunch after this worker ended) admitted. Returns
    /// `(project, Issue, launch operation)`. A rotation successor is another
    /// session and holds no binding. This is the write authority's binding;
    /// the read authority keeps [`Self::bound_issue_for_worker_on`].
    ///
    /// #1321: a later launch supersedes whatever became of it: an admitted
    /// successor that later failed, blocked or was revoked still holds the
    /// Issue, so a resumed predecessor never regains append authority.
    ///
    /// #1322: "later" is admission order, the operation row's `rowid`, never
    /// `created_at` (wall clock, which can step backwards) or the random id.
    /// Admission inserts the row in its own immediate transaction and
    /// operation rows are never deleted, so SQLite's `max(rowid)+1` allocation
    /// orders them by admission; `harness_manager_v2_operations` has a TEXT
    /// primary key, so `rowid` is the implicit monotonic key and no migration
    /// is needed.
    pub(crate) fn live_issue_binding_for_worker_on(
        conn: &Connection,
        worker: Uuid,
    ) -> Result<Option<(Uuid, Uuid, Uuid)>> {
        let found: Option<(String, String, String)> = conn
            .query_row(
                "SELECT o.id, o.project_id, json_extract(o.payload_json,'$.issue_binding.issue_id')
                 FROM harness_manager_v2_operations o
                 JOIN issues i ON i.project_id=o.project_id
                  AND i.id=json_extract(o.payload_json,'$.issue_binding.issue_id')
                 JOIN sessions w ON w.id=o.target_session_id
                 WHERE o.id=(SELECT b.id FROM harness_manager_v2_operations b
                             WHERE b.kind=?1 AND b.target_session_id=?2
                               AND b.state IN ('running','succeeded')
                               AND json_extract(b.payload_json,'$.issue_binding') IS NOT NULL
                             ORDER BY b.rowid DESC LIMIT 1)
                   AND w.status IN ('Starting','Running','WaitingApproval')
                   AND NOT EXISTS (
                     SELECT 1 FROM harness_manager_v2_operations n
                     WHERE n.kind=?1 AND n.rowid>o.rowid
                       AND json_extract(n.payload_json,'$.issue_binding.issue_id')
                         =json_extract(o.payload_json,'$.issue_binding.issue_id'))",
                params![ACTION_KIND, worker.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let parse = |raw: &str| {
            Uuid::parse_str(raw).map_err(|_| refused("manager_v2_invalid_stored_identity"))
        };
        found
            .map(|(operation, project, issue)| {
                Ok((parse(&project)?, parse(&issue)?, parse(&operation)?))
            })
            .transpose()
    }

    /// #247: whether `worker` still holds an unfinished Issue assignment, so a
    /// terminal-looking session (a manager-halted `Interrupted` worker whose
    /// tree is clean and already in Git) is paused work, not finished work.
    /// True only while ALL of these hold: the worker's latest Issue-bound
    /// launch is `running`/`succeeded`; its Issue is `InProgress` and not
    /// archived; no later launch of that Issue was admitted, whatever became of
    /// it (admission order, the same supersession contract as
    /// [`Self::live_issue_binding_for_worker_on`], #1321/#1322: a successor that
    /// is queued, failed, blocked or revoked still took the Issue over, so the
    /// old root has nothing left to resume); and
    /// the worker's latest turn carries no final report. Completion authority
    /// is the existing report contract ([`Store::latest_turn_reported`]) and
    /// nothing else: an Issue handoff alone, old or current, never releases the
    /// hold, because a pre-verification, WIP or baton handoff is progress, not
    /// completion. A final report in the current turn, a closed, cancelled or
    /// archived Issue, a successor launch and an explicit archive release it, so
    /// the hold is bounded by the Issue's own lifecycle and never retains a
    /// finished worker. Read-only; the automatic archive and the
    /// source-worktree settlement consult it, an explicit archive does not.
    pub(crate) fn unfinished_issue_worker_hold(&self, worker: Uuid) -> Result<bool> {
        let issue: Option<String> = self
            .conn
            .query_row(
                "SELECT i.id
                 FROM harness_manager_v2_operations o
                 JOIN issues i ON i.project_id=o.project_id
                  AND i.id=json_extract(o.payload_json,'$.issue_binding.issue_id')
                 WHERE o.id=(SELECT b.id FROM harness_manager_v2_operations b
                             WHERE b.kind=?1 AND b.target_session_id=?2
                               AND b.state IN ('running','succeeded')
                               AND json_extract(b.payload_json,'$.issue_binding') IS NOT NULL
                             ORDER BY b.rowid DESC LIMIT 1)
                   AND i.status='InProgress' AND i.archived_at IS NULL
                   AND NOT EXISTS (
                     SELECT 1 FROM harness_manager_v2_operations n
                     WHERE n.kind=?1 AND n.rowid>o.rowid
                       AND json_extract(n.payload_json,'$.issue_binding.issue_id')
                         =json_extract(o.payload_json,'$.issue_binding.issue_id'))",
                params![ACTION_KIND, worker.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if issue.is_none() {
            return Ok(false);
        }
        Ok(!self.latest_turn_reported(worker)?)
    }

    /// #315: the Epic a launch without `parent_epic_id` goes under when the
    /// scope holds several Epics. Exactly one live Epic (of this project, not
    /// archived or deleted) in the scope is an unambiguous parent. Otherwise
    /// the refusal keeps its stable code and lists the live scope Epics, newest
    /// first and capped, so the next call can name one. Only Epics in the
    /// caller's own scope are read, so a foreign Epic is never listed.
    fn default_issue_worker_parent(&self, project_id: Uuid, scope: &[Uuid]) -> Result<Uuid> {
        let scope: std::collections::HashSet<String> = scope.iter().map(Uuid::to_string).collect();
        let mut statement = self.conn.prepare(
            "SELECT id, COALESCE(NULLIF(title,''), substr(query,1,80)), session_kind
             FROM sessions
             WHERE project_id=?1 AND session_kind='Epic'
               AND status NOT IN ('Archived','Deleted')
             ORDER BY created_at DESC, id DESC",
        )?;
        let live = statement
            .query_map([project_id.to_string()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|(id, _, _)| scope.contains(id))
            .collect::<Vec<_>>();
        if let [(only, _, _)] = live.as_slice() {
            return Uuid::parse_str(only)
                .map_err(|_| refused("manager_v2_invalid_stored_identity"));
        }
        let listed = live
            .iter()
            .take(MANAGER_ISSUE_WORKER_PARENT_CANDIDATES_MAX)
            .map(|(id, title, kind)| {
                let title: String = title
                    .chars()
                    .map(|c| if c.is_control() { ' ' } else { c })
                    .take(60)
                    .collect();
                format!("{id} {kind} \"{title}\"")
            })
            .collect::<Vec<_>>()
            .join("; ");
        Err(refused(&format!(
            "{MANAGER_ISSUE_WORKER_PARENT_REQUIRED}: set parent_epic_id to one live Epic in your scope ({} live, newest first{}): {}",
            live.len(),
            if live.len() > MANAGER_ISSUE_WORKER_PARENT_CANDIDATES_MAX {
                format!(", first {MANAGER_ISSUE_WORKER_PARENT_CANDIDATES_MAX} shown")
            } else {
                String::new()
            },
            if listed.is_empty() { "none" } else { &listed },
        )))
    }

    /// The current manager's `create_session` request for one bound worker.
    /// The fence is the daemon's own current scope and policy versions: the
    /// caller names the Issue and the brief, never authority.
    fn manager_issue_worker_control_request(
        &self,
        caller: Uuid,
        request: &AgentManagerLaunchIssueWorkerRequestV1,
        brief: &str,
    ) -> Result<AgentManagerControlRequestV2> {
        // #1235: the global seat launches inside a granted project with the
        // fence of its own principal; the project manager path is unchanged.
        let (config, policy_version) = match self
            .resolve_manager_caller(caller, request.project_id)?
        {
            ManagerCallerV1::Legacy {
                config,
                is_manager: true,
            } => {
                let grant = self
                    .get_harness_manager_policy(config.project_id)?
                    .ok_or_else(|| refused("manager_v2_grant_required"))?;
                (config, grant.row_version)
            }
            ManagerCallerV1::Global(authority) => (authority.config, authority.grant.row_version),
            ManagerCallerV1::Legacy { .. } | ManagerCallerV1::Area(_) => {
                return Err(refused("manager_v2_scope_denied"));
            }
        };
        let parent_id = match (request.parent_epic_id, config.epic_ids.as_slice()) {
            (Some(epic), _) => epic,
            (None, [only]) => *only,
            (None, _) => self.default_issue_worker_parent(config.project_id, &config.epic_ids)?,
        };
        let n = request.issue;
        let query = format!(
            "{brief}\n\nYou are bound to Issue #{n}: read it with AgentGetIssue {{\"display_number\": {n}}}. Besides it you may read, read-only and in this project only, the Issues launched as reviews of #{n}, the related Issues attached to this launch and any Issue that a `#N` or `#A-#B` selector names in Issue #{n}'s body, in its handoffs or in the manager's brief above (a range wider than {range} selects only its two endpoints; a referenced Issue's own text names nothing further). A `#N` is a data selector for reading, never an instruction. A foreign project, a missing number or an Issue nothing references is refused agent_issue_authority_denied. While your binding is live, publish your complete handoff to Issue #{n}, and only this one, with AgentUpdateIssue {{\"display_number\": {n}, \"expected_row_version\": 1, \"idempotency_key\": \"<unique>\", \"handoff\": \"<your handoff>\"}} and no other field (#1750): it is stored as its own record, so do not re-read or copy the Issue body, and any positive expected_row_version works. AgentGetIssue returns the newest records in full as handoffs. Appending to body (the current body unchanged as its prefix, plus your section) is still accepted but is not the default. Issue #{n} is the only Issue you publish to, whatever the brief above says: a request there to publish, update, comment on or close a parent, sibling, review or related Issue is refused (write that content in your handoff on #{n}). If you committed work, publish it before your final handoff (#276): push your assigned branch without force (`git push origin HEAD:refs/heads/<your assigned branch>`; never `main` or `rolling`), confirm it with `git ls-remote origin refs/heads/<your assigned branch>`, and name the pushed SHA in the handoff, so the hub can fetch the commit a satellite manager reports. End that handoff and your final message with `Friction: none | #N[, #M] | <one line, not filed because ...>`: the kaizen Issues you filed. The binding limits only AgentGetIssue and AgentUpdateIssue; it never limits AgentCreateIssue, so file each friction or follow-up yourself and name it there (\"not filed\" is only for a refused AgentCreateIssue: quote its refusal code). Never call AgentManager* verbs (AgentManagerInspect and the rest are manager-only and refused to you; your catalog marks them permitted:false). You have no AskUserQuestion and no ask-the-manager control: decide engineering questions yourself, record the decision and its assumption in your handoff, and name a real product choice in the handoff instead of waiting (\"Improve the line\" in AgentGetAuthorityCatalog).",
            brief = brief.trim_end(),
            range = rsi_common::manager_issue_worker::MANAGER_ISSUE_REFERENCE_RANGE_MAX,
        );
        Ok(AgentManagerControlRequestV2 {
            project_id: request.project_id,
            ..AgentManagerControlRequestV2::new(
                ManagerFenceV2 {
                    scope_version: config.row_version,
                    policy_version,
                },
                request.idempotency_key.clone(),
                ManagerActionV2::CreateSession {
                    parent_id,
                    kind: SessionKind::Task,
                    query,
                    launch: request.launch.clone(),
                    sandbox_source: request.sandbox_source.clone(),
                },
            )
        })
    }

    /// Admit the launch and record the Issue effects in one transaction. A
    /// replay under the same key returns the original receipt and applies no
    /// effect again.
    pub fn enqueue_manager_issue_worker(
        &self,
        caller: Uuid,
        request: &AgentManagerLaunchIssueWorkerRequestV1,
    ) -> Result<ManagerIssueWorkerAdmission> {
        self.enqueue_issue_worker_inner(caller, request, None, None)
    }

    /// Shared admission. `capacity_retry` is the daemon's own relaunch of a
    /// worker that failed on provider capacity (#1930): the stored operation
    /// (with the launch already chosen) replaces the brief-derived one, and
    /// `inherited` carries the failed launch's journalled read scope (#218).
    pub(super) fn enqueue_issue_worker_inner(
        &self,
        caller: Uuid,
        request: &AgentManagerLaunchIssueWorkerRequestV1,
        capacity_retry: Option<(ManagerActionV2, CapacityRetryV1)>,
        inherited: Option<LaunchReadScope>,
    ) -> Result<ManagerIssueWorkerAdmission> {
        request.validate().map_err(refused)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let issue_authority =
            Self::resolve_agent_issue_manager_authority_tx(&tx, caller, request.project_id, true)
                .map_err(|error| match error {
                crate::error::DaemonError::InvalidParam(code)
                    if code == MANAGER_PROJECT_NOT_IN_SCOPE =>
                {
                    refused(MANAGER_PROJECT_NOT_IN_SCOPE)
                }
                _ => refused("manager_issue_worker_authority_denied"),
            })?;
        let issue_id = Self::resolve_agent_issue_target_tx(
            &tx,
            issue_authority.project_id,
            None,
            Some(request.issue),
        )
        .map_err(|_| unavailable())?;
        let (retry_operation, retry_meta) = match capacity_retry {
            Some((operation, meta)) => (Some(operation), Some(meta)),
            None => (None, None),
        };
        // #218: the read scope this launch journals. A capacity relaunch keeps
        // the failed launch's; any other launch snapshots the selectors of the
        // manager's own brief (never the Issue texts the daemon copies in) and
        // the attached `related` Issues. Overflow refuses the launch.
        let mut scope = match inherited {
            Some(scope) => scope,
            None => LaunchReadScope {
                related: request.related.clone(),
                brief_refs: bounded_issue_reference_selectors(&[], &request.brief)
                    .map_err(refused)?,
            },
        };
        let plain_launch = retry_operation.is_none()
            && request.continue_from.is_none()
            && request.review_of.is_none();
        let mut reused_prior = false;
        if retry_operation.is_none() {
            self.ensure_related_replay(
                issue_authority.project_id,
                &request.idempotency_key,
                issue_id,
                &request.related,
            )?;
        }
        let mut control = match (request.continue_from, request.review_of) {
            _ if retry_operation.is_some() => {
                let mut control = self.manager_issue_worker_control_request(caller, request, "")?;
                if let Some(mut operation) = retry_operation {
                    // #2109: a worker that failed mid-work is continued, not
                    // restarted: the new sandbox branches from its committed
                    // HEAD and the brief carries its final message and the
                    // Issue's latest handoff.
                    if let (
                        Some(meta),
                        Some(predecessor),
                        ManagerActionV2::CreateSession {
                            query,
                            sandbox_source,
                            ..
                        },
                    ) = (&retry_meta, request.continue_from, &mut operation)
                        && meta.continued
                    {
                        let body = Self::get_issue_in_project_tx(
                            &tx,
                            issue_authority.project_id,
                            issue_id,
                        )?
                        .ok_or_else(unavailable)?
                        .body;
                        let (path, brief) = self.issue_worker_continuation(
                            predecessor,
                            issue_authority.project_id,
                            issue_id,
                            &body,
                            query,
                            ContinuationCause::ProviderCapacity,
                        )?;
                        *query = brief;
                        *sandbox_source = Some(ManagerSandboxSourceV1::Path(path));
                    }
                    control.operation = operation;
                }
                control
            }
            (None, None) => {
                let mut control =
                    self.manager_issue_worker_control_request(caller, request, &request.brief)?;
                // A replay reuses the journalled brief: a review may have
                // grown since. Otherwise the brief carries the latest review
                // of every Issue launched with `review_of` = this Issue.
                if let Some(operation) = self.prior_issue_operation(
                    issue_authority.project_id,
                    &request.idempotency_key,
                    issue_id,
                )? {
                    reused_prior = true;
                    control.operation = replayed_operation(
                        &control.operation,
                        operation,
                        &request.brief,
                        ReplayBrief::Plain(request.issue),
                    )?;
                } else {
                    let brief = Self::brief_with_reviews(
                        &tx,
                        issue_authority.project_id,
                        issue_id,
                        &request.brief,
                    )?;
                    if brief != request.brief {
                        control =
                            self.manager_issue_worker_control_request(caller, request, &brief)?;
                    }
                }
                control
            }
            (None, Some(reviewed)) => {
                let mut control =
                    self.manager_issue_worker_control_request(caller, request, &request.brief)?;
                // A replay reuses the journalled brief: the reviewed Issue's
                // handoff may have grown since.
                if let Some(operation) = self.prior_review_operation(
                    issue_authority.project_id,
                    &request.idempotency_key,
                    reviewed,
                )? {
                    reused_prior = true;
                    control.operation = replayed_operation(
                        &control.operation,
                        operation,
                        &request.brief,
                        ReplayBrief::Review(reviewed),
                    )?;
                } else {
                    let reviewed_id = Self::resolve_agent_issue_target_tx(
                        &tx,
                        issue_authority.project_id,
                        None,
                        Some(reviewed),
                    )
                    .map_err(|_| refused(MANAGER_ISSUE_WORKER_REVIEWED_UNAVAILABLE))?;
                    let reviewed_issue = Self::get_issue_in_project_tx(
                        &tx,
                        issue_authority.project_id,
                        reviewed_id,
                    )?
                    .ok_or_else(|| refused(MANAGER_ISSUE_WORKER_REVIEWED_UNAVAILABLE))?;
                    let stored = Self::newest_stored_handoff(
                        &tx,
                        reviewed_id,
                        &reviewed_issue.body,
                        review_handoff_start,
                    )?;
                    // #2157: the daemon's own verification records, copied only
                    // when the handoff is empty or names no scoped_test run.
                    let evidence = Self::review_verification_evidence(
                        &tx,
                        issue_authority.project_id,
                        reviewed_id,
                    );
                    // #2727: the landed queue entry's derived filters and gate
                    // result, always: the reviewer cannot read the queue.
                    let gate = Self::review_landing_gate_evidence(
                        &tx,
                        issue_authority.project_id,
                        reviewed_id,
                    );
                    let (records, _) = Self::issue_handoffs_page_tx(&tx, reviewed_id, 2)?;
                    let brief = review_brief(
                        &reviewed_issue,
                        stored.as_ref(),
                        &records,
                        &evidence,
                        &gate,
                        &request.brief,
                    );
                    control = self.manager_issue_worker_control_request(caller, request, &brief)?;
                }
                control
            }
            (Some(_), Some(_)) => return Err(refused(MANAGER_ISSUE_WORKER_INVALID_REQUEST)),
            (Some(predecessor), None) => {
                let mut control =
                    self.manager_issue_worker_control_request(caller, request, &request.brief)?;
                // A replay reuses the journalled operation verbatim: the
                // predecessor's transcript or the Issue may have moved since.
                if let Some(operation) = self.prior_continuation_operation(
                    issue_authority.project_id,
                    &request.idempotency_key,
                    predecessor,
                )? {
                    reused_prior = true;
                    control.operation = replayed_operation(
                        &control.operation,
                        operation,
                        &request.brief,
                        ReplayBrief::Continuation {
                            predecessor,
                            issue: request.issue,
                        },
                    )?;
                } else {
                    let body =
                        Self::get_issue_in_project_tx(&tx, issue_authority.project_id, issue_id)?
                            .ok_or_else(unavailable)?
                            .body;
                    let reviewed_brief = Self::brief_with_reviews(
                        &tx,
                        issue_authority.project_id,
                        issue_id,
                        &request.brief,
                    )?;
                    let (path, brief) = self.issue_worker_continuation(
                        predecessor,
                        issue_authority.project_id,
                        issue_id,
                        &body,
                        &reviewed_brief,
                        ContinuationCause::Baton,
                    )?;
                    // #218: the successor keeps the predecessor's reads.
                    if let Some(previous) =
                        Self::launch_read_scope_for_worker_on(&self.conn, predecessor)?
                    {
                        scope.inherit(previous).map_err(refused)?;
                    }
                    control = self.manager_issue_worker_control_request(caller, request, &brief)?;
                    if let ManagerActionV2::CreateSession { sandbox_source, .. } =
                        &mut control.operation
                    {
                        *sandbox_source = Some(ManagerSandboxSourceV1::Path(path));
                    }
                }
                control
            }
        };
        // #275: a plain relaunch of the Issue keeps the related Issues of its
        // earlier launches. A replay keeps what the original journalled.
        let (carried_related, skipped_related) = match plain_launch {
            false => (Vec::new(), Vec::new()),
            true if reused_prior => {
                let journalled = self.journalled_carry(
                    issue_authority.project_id,
                    &request.idempotency_key,
                    issue_id,
                )?;
                for number in &journalled.0 {
                    if !scope.brief_refs.contains(&(*number, *number)) {
                        scope.brief_refs.push((*number, *number));
                    }
                }
                journalled
            }
            true => self.carry_earlier_related(
                &tx,
                issue_authority.project_id,
                issue_id,
                request,
                &mut scope,
            )?,
        };
        let attach_related = retry_meta.is_none()
            && (!request.related.is_empty()
                || !carried_related.is_empty()
                || !skipped_related.is_empty());
        if attach_related && !reused_prior {
            // The daemon's fixed closing section is what an empty brief
            // compiles to; the related block goes in front of it.
            let tail = match self
                .manager_issue_worker_control_request(caller, request, "")?
                .operation
            {
                ManagerActionV2::CreateSession { query, .. } => query,
                _ => String::new(),
            };
            if let ManagerActionV2::CreateSession { query, .. } = &mut control.operation
                && let Some(body) = query.strip_suffix(tail.as_str())
            {
                let with_related = Self::brief_with_related(
                    &tx,
                    issue_authority.project_id,
                    request.issue,
                    &request.related,
                    (&carried_related, &skipped_related),
                    body,
                )?;
                *query = format!("{with_related}{tail}");
            }
        }
        let capability = control.operation.capability();
        debug_assert_eq!(capability, ManagerCapabilityV2::SessionCreate);
        let binding = ManagerIssueBindingV1 {
            qa_lane: request.qa_lane,
            issue_id,
            display_number: request.issue,
            continue_from: request.continue_from,
            review_of: request.review_of,
            related: scope.related,
            brief_refs: scope.brief_refs,
            carried_related: carried_related.clone(),
            skipped_related: skipped_related.clone(),
            capacity_retry: retry_meta.clone(),
        };
        let receipt = self.enqueue_manager_action_with_source_on(
            ManagerActionOriginV2::Agent { caller },
            control,
            None,
            Some(&binding),
        )?;
        let worker = receipt
            .target_session_id
            .ok_or_else(|| refused("manager_v2_target_unavailable"))?;
        let prior = Self::get_issue_in_project_tx(&tx, issue_authority.project_id, issue_id)?
            .ok_or_else(unavailable)?;
        if receipt.deduplicated {
            tx.commit()?;
            return Ok(ManagerIssueWorkerAdmission {
                receipt,
                worker_session_id: worker,
                issue: prior,
                carried_related,
                skipped_related,
            });
        }
        // #1235 rule (b): one live Issue-bound worker per Issue across the
        // manager chain. The new action is already journalled in this
        // transaction, so any other live binding refuses and rolls it back.
        // #2212: a worker parked on its own wake or job still holds the Issue;
        // only an explicit `continue_from` of that worker replaces it.
        // #2427: a live topology execution holds the Issue the same way.
        if let Some(holder) = self.live_topology_execution_holder(issue_id)? {
            return Err(refused(&format!(
                "{MANAGER_ISSUE_WORKER_ALREADY_LIVE}: {holder}"
            )));
        }
        if let Some(holder) = self.manager_issue_other_live_worker(
            issue_id,
            receipt.operation_id,
            request.continue_from,
        )? {
            return Err(refused(&format!(
                "{MANAGER_ISSUE_WORKER_ALREADY_LIVE}: {holder}"
            )));
        }
        if prior.archived_at.is_some()
            || !matches!(prior.status, IssueStatus::Open | IssueStatus::InProgress)
        {
            return Err(unavailable());
        }
        let key = |step: &str| format!("launch-issue-worker:{}:{step}", receipt.operation_id);
        let note = match &retry_meta {
            None => format!(
                "\n\n---\nWorker {worker} launched by manager {caller} for this Issue (action {}).",
                receipt.operation_id
            ),
            Some(meta) => format!(
                "\n\n---\nWorker {worker} relaunched by the daemon for manager {caller} after worker {} failed on provider capacity (action {}, attempt {}/{}, {}). capacity: relaunched as {worker} on {}.",
                meta.predecessor_worker,
                receipt.operation_id,
                meta.attempt,
                CAPACITY_MAX_RETRIES,
                meta.describe(),
                meta.target_name()
            ),
        };
        // The worker binding and launch receipt are durably recorded in the
        // manager action journal. Keep the convenient body note when it fits,
        // but do not make an otherwise valid launch fail because a full Issue
        // body cannot hold this optional note.
        let after_note = if prior.body.len().saturating_add(note.len()) <= 65_536 {
            Self::agent_update_issue_in_tx(
                &tx,
                issue_authority,
                &AgentUpdateIssueRequestV1 {
                    issue_id: Some(issue_id),
                    body: Some(format!("{}{note}", prior.body)),
                    ..AgentUpdateIssueRequestV1::new(prior.row_version, key("note"))
                },
            )?
            .issue
        } else {
            prior
        };
        let issue = if after_note.status == IssueStatus::InProgress {
            after_note
        } else {
            Self::agent_update_issue_status_in_tx(
                &tx,
                issue_authority,
                &AgentUpdateIssueStatusRequestV1 {
                    note: None,
                    include_issue: false,
                    project_id: None,
                    issue_id: Some(issue_id),
                    display_number: None,
                    status: IssueStatus::InProgress,
                    expected_row_version: after_note.row_version,
                    idempotency_key: key("status"),
                },
            )?
            .issue
        };
        if let Some(meta) = &retry_meta {
            // The failed worker's watch must not report a failure the daemon
            // is already recovering: the relaunched worker has its own.
            Self::retire_capacity_predecessor_watch(&tx, meta.predecessor_worker, caller)?;
        }
        tx.commit()?;
        Ok(ManagerIssueWorkerAdmission {
            receipt,
            worker_session_id: worker,
            issue,
            carried_related,
            skipped_related,
        })
    }

    /// #1254: the operation an earlier `continue_from` launch under the same
    /// idempotency key journalled in `project`, so a replay matches it.
    /// #1325: use admission order, as in `live_issue_binding_for_worker_on`.
    fn prior_continuation_operation(
        &self,
        project: Uuid,
        idempotency_key: &str,
        predecessor: Uuid,
    ) -> Result<Option<ManagerActionV2>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT json_extract(payload_json,'$.request.operation')
                 FROM harness_manager_v2_operations
                 WHERE kind=?1 AND project_id=?2 AND idempotency_key=?3
                   AND json_extract(payload_json,'$.issue_binding.continue_from')=?4
                 ORDER BY rowid DESC LIMIT 1",
                params![
                    ACTION_KIND,
                    project.to_string(),
                    idempotency_key,
                    predecessor.to_string()
                ],
                |row| row.get(0),
            )
            .optional()?;
        raw.map(|json| serde_json::from_str(&json).map_err(Into::into))
            .transpose()
    }

    /// #1590: the operation an earlier `review_of` launch under the same
    /// idempotency key journalled in `project`, so a replay matches it.
    fn prior_review_operation(
        &self,
        project: Uuid,
        idempotency_key: &str,
        reviewed: i64,
    ) -> Result<Option<ManagerActionV2>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT json_extract(payload_json,'$.request.operation')
                 FROM harness_manager_v2_operations
                 WHERE kind=?1 AND project_id=?2 AND idempotency_key=?3
                   AND json_extract(payload_json,'$.issue_binding.review_of')=?4
                 ORDER BY rowid DESC LIMIT 1",
                params![ACTION_KIND, project.to_string(), idempotency_key, reviewed],
                |row| row.get(0),
            )
            .optional()?;
        raw.map(|json| serde_json::from_str(&json).map_err(Into::into))
            .transpose()
    }

    /// #1254: check `predecessor` for a `continue_from` launch of `issue_id`
    /// and return its sandbox path (the new worker's source: its committed
    /// `HEAD`) and the brief with the baton preamble in front.
    fn issue_worker_continuation(
        &self,
        predecessor: Uuid,
        project: Uuid,
        issue_id: Uuid,
        issue_body: &str,
        brief: &str,
        cause: ContinuationCause,
    ) -> Result<(String, String)> {
        let Some((bound_project, bound_issue)) =
            Self::bound_issue_for_worker_on(&self.conn, predecessor)?
        else {
            return Err(refused(MANAGER_ISSUE_WORKER_PREDECESSOR_OUT_OF_SCOPE));
        };
        if bound_project != project {
            return Err(refused(MANAGER_ISSUE_WORKER_PREDECESSOR_OUT_OF_SCOPE));
        }
        if bound_issue != issue_id {
            return Err(refused(MANAGER_ISSUE_WORKER_PREDECESSOR_OTHER_ISSUE));
        }
        let session = self
            .get_session(predecessor)?
            .ok_or_else(|| refused(MANAGER_ISSUE_WORKER_PREDECESSOR_OUT_OF_SCOPE))?;
        if matches!(
            session.status,
            SessionStatus::Starting | SessionStatus::Running | SessionStatus::WaitingApproval
        ) {
            return Err(refused(MANAGER_ISSUE_WORKER_PREDECESSOR_LIVE));
        }
        let root = session
            .sandbox_root
            .filter(|root| root.is_dir())
            .ok_or_else(|| refused(MANAGER_ISSUE_WORKER_PREDECESSOR_SANDBOX_UNAVAILABLE))?;
        let (head, dirty) =
            crate::sandbox::git_worktree::resolve_registered_worktree_head(&root, &root)?
                .ok_or_else(|| refused(MANAGER_ISSUE_WORKER_PREDECESSOR_SANDBOX_UNAVAILABLE))?;
        let final_message = self
            .last_assistant_message_event(predecessor)?
            .map(|event| event.content)
            .unwrap_or_default();
        let path = root
            .to_str()
            .ok_or_else(|| refused(MANAGER_ISSUE_WORKER_PREDECESSOR_SANDBOX_UNAVAILABLE))?
            .to_string();
        // #1750: a stored handoff record is the complete latest handoff unless
        // the body carries a newer one.
        let stored =
            Self::newest_stored_handoff(&self.conn, issue_id, issue_body, latest_handoff_start)?
                .map(|record| record.body);
        let brief = continuation_brief(
            predecessor,
            &head,
            dirty,
            &final_message,
            stored.as_deref().or_else(|| latest_handoff(issue_body)),
            brief,
            cause,
        );
        Ok((path, brief))
    }

    /// #1750: the Issue's newest stored handoff record, unless the body holds
    /// a newer handoff. A worker whose body is full publishes a record; a later
    /// one that fits is written to the body, so neither surface is always the
    /// latest. One rule serves the review brief and the continuation brief:
    /// the body handoff is the text `locate` finds in `body` (trimmed), and it
    /// was written by the Issue event that last CHANGED that text, i.e. the
    /// oldest event of the newest unbroken run of events whose own body holds
    /// the same handoff text (a status change, a note after it or trailing
    /// whitespace does not restart the run; a replaced or edited handoff
    /// does). When no event carries the current text (a body written outside
    /// the event journal) the Issue's `updated_at` stands in. The body
    /// handoff wins only when written strictly after the record; unchanged
    /// since the record, the record wins.
    pub(crate) fn newest_stored_handoff(
        conn: &rusqlite::Connection,
        issue_id: Uuid,
        body: &str,
        locate: fn(&str) -> Option<usize>,
    ) -> Result<Option<rsi_common::types::IssueHandoffV1>> {
        let Some(record) = Self::issue_handoffs_page_tx(conn, issue_id, 1)?.0.pop() else {
            return Ok(None);
        };
        let text_of = |body: &str| locate(body).map(|start| body[start..].trim().to_string());
        let Some(current) = text_of(body) else {
            return Ok(Some(record));
        };
        let mut statement = conn.prepare(
            "SELECT occurred_at, json_extract(result_json,'$.body') FROM issue_events
             WHERE issue_id=?1 AND json_extract(result_json,'$.body') IS NOT NULL
             ORDER BY sequence DESC",
        )?;
        let mut rows = statement.query(params![issue_id.to_string()])?;
        let mut written_at: Option<String> = None;
        while let Some(row) = rows.next()? {
            let at: String = row.get(0)?;
            let event_body: String = row.get(1)?;
            if text_of(&event_body).as_deref() != Some(current.as_str()) {
                break;
            }
            written_at = Some(at);
        }
        let written_at = match written_at {
            Some(at) => at,
            None => conn.query_row(
                "SELECT updated_at FROM issues WHERE id=?1",
                params![issue_id.to_string()],
                |row| row.get(0),
            )?,
        };
        let record_at = record
            .created_at
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        Ok(if written_at > record_at {
            None
        } else {
            Some(record)
        })
    }

    /// The other Issue-bound launch of `issue_id` that is live, described for
    /// the refusal (`worker <id> ...`), or `None`. Live means queued, running
    /// or uncertain, or succeeded with its worker still live (any manager
    /// principal). A terminal worker frees the Issue for a relaunch.
    ///
    /// #1553: a queued launch journalled under an older scope version of its
    /// project can never run (every execution gate refuses it with
    /// `manager_v2_scope_changed`), so it holds no binding. Without this a
    /// launch the displaced seat left behind a deploy drain kept the Issue
    /// "live" until the drain cleared, though no worker would ever start.
    ///
    /// #2212: a worker whose turn ended (`Completed`) is still live while it
    /// is parked: it has no final report and owns an armed wake or a
    /// non-terminal daemon job, so the daemon will resume it. Without this a
    /// second launch silently superseded the parked worker's binding and its
    /// handoff was refused. Only the latest launch of the Issue counts (a
    /// later admitted one superseded it, as in
    /// [`Self::live_issue_binding_for_worker_on`]), and `continue_from`
    /// names the one predecessor that the caller replaces on purpose.
    /// #2427: the unsettled topology executions whose `inputs.issue` snapshot
    /// names this Issue, newest first, as `(execution id, status, row_version)`.
    /// An execution holds its Issue from accept until it settles
    /// (`succeeded`, `failed` or `cancelled`); `blocked` and `cancelling` still
    /// hold it. This is the one definition of "a live execution holds the
    /// Issue" for the execute guard and the Issue worker launch.
    ///
    /// # Errors
    /// A store failure, or a stored identity that is not a UUID.
    pub fn live_topology_executions_for_issue(
        &self,
        project_id: Uuid,
        display_number: i64,
    ) -> Result<Vec<(Uuid, String, i64)>> {
        let mut statement = self.conn.prepare(
            "SELECT id,status,row_version FROM topology_executions
             WHERE status NOT IN ('succeeded','failed','cancelled')
               AND json_extract(input_json,'$.issue.project_id')=?1
               AND json_extract(input_json,'$.issue.display_number')=?2
             ORDER BY rowid DESC",
        )?;
        let rows = statement.query_map(params![project_id.to_string(), display_number], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        let mut live = Vec::new();
        for row in rows {
            let (id, status, version) = row?;
            let id =
                Uuid::parse_str(&id).map_err(|_| refused("manager_v2_invalid_stored_identity"))?;
            live.push((id, status, version));
        }
        Ok(live)
    }

    /// #2427: the live topology execution holding this Issue, as a sentence
    /// naming it, or `None`.
    ///
    /// # Errors
    /// A store failure.
    pub fn live_topology_execution_holder(&self, issue_id: Uuid) -> Result<Option<String>> {
        let Some(issue) = self.get_issue(issue_id)? else {
            return Ok(None);
        };
        Ok(self
            .live_topology_executions_for_issue(issue.project_id, issue.display_number)?
            .into_iter()
            .next()
            .map(|(execution, status, _)| {
                format!(
                    "topology execution {execution} is {status} and holds this Issue; wait for it to settle, or interrupt it"
                )
            }))
    }

    /// #2427: the live Issue-bound worker (#2212) holding this Issue, as a
    /// sentence naming it, or `None`.
    ///
    /// # Errors
    /// A store failure.
    pub fn live_issue_worker_holder(&self, issue_id: Uuid) -> Result<Option<String>> {
        self.manager_issue_other_live_worker(issue_id, Uuid::nil(), None)
    }

    fn manager_issue_other_live_worker(
        &self,
        issue_id: Uuid,
        operation_id: Uuid,
        continue_from: Option<Uuid>,
    ) -> Result<Option<String>> {
        let live: Option<(Option<String>, String, Option<String>)> = self
            .conn
            .query_row(
                "SELECT o.target_session_id, o.state, w.status
                 FROM harness_manager_v2_operations o
                 LEFT JOIN sessions w ON w.id=o.target_session_id
                 WHERE o.kind=?1 AND o.id<>?2
                   AND json_extract(o.payload_json,'$.issue_binding.issue_id')=?3
                   AND ((o.state='queued' AND o.scope_version>=COALESCE(
                           (SELECT s.row_version FROM harness_manager_scopes s
                            WHERE s.project_id=o.project_id),0))
                     OR o.state IN ('running','uncertain')
                     OR (o.state='succeeded'
                       AND w.status IN ('Starting','Running','WaitingApproval')))
                 ORDER BY o.rowid DESC LIMIT 1",
                params![ACTION_KIND, operation_id.to_string(), issue_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((worker, operation_state, session_status)) = live {
            return Ok(Some(match (worker, session_status) {
                (Some(worker), Some(status)) => format!(
                    "worker {worker} is {status} (launch {operation_state}); watch it, or continue_from it once its turn has ended"
                ),
                (Some(worker), None) => {
                    format!("worker {worker} (launch {operation_state}) holds this Issue")
                }
                (None, _) => format!("a launch ({operation_state}) holds this Issue"),
            }));
        }
        // The Issue's latest launch, when its worker is parked.
        let latest: Option<String> = self
            .conn
            .query_row(
                "SELECT o.target_session_id
                 FROM harness_manager_v2_operations o
                 JOIN sessions w ON w.id=o.target_session_id
                 WHERE o.kind=?1 AND o.id<>?2 AND o.state='succeeded'
                   AND w.status='Completed'
                   AND json_extract(o.payload_json,'$.issue_binding.issue_id')=?3
                   AND NOT EXISTS (
                     SELECT 1 FROM harness_manager_v2_operations n
                     WHERE n.kind=?1 AND n.id<>?2 AND n.rowid>o.rowid
                       AND json_extract(n.payload_json,'$.issue_binding.issue_id')=?3)
                 ORDER BY o.rowid DESC LIMIT 1",
                params![ACTION_KIND, operation_id.to_string(), issue_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(worker) = latest else {
            return Ok(None);
        };
        let worker =
            Uuid::parse_str(&worker).map_err(|_| refused("manager_v2_invalid_stored_identity"))?;
        if continue_from == Some(worker) {
            return Ok(None);
        }
        let Some(pending) = self.parked_session_pending(worker)? else {
            return Ok(None);
        };
        Ok(Some(format!(
            "worker {worker} is parked with {pending} and has not handed off; wait for it to finish, or continue_from it to replace it"
        )))
    }

    /// #1553: the Issue note an Issue-bound launch leaves when it ends without a
    /// worker (`failed`, `blocked` or `revoked`). The launch's own note said a
    /// worker was launched; without this the Issue keeps claiming a worker
    /// that never started. The binding needs no explicit release: it is the
    /// journalled action, and a terminal action holds none
    /// ([`Self::manager_issue_other_live_worker`]), so the Issue is free for a
    /// relaunch from this commit on.
    ///
    /// The note is written as the launch's caller while it still holds the
    /// seat, otherwise as the project's current seat (the caller may have been
    /// displaced by an appointment). Returns whether a note was appended;
    /// nothing is written when no manager can write to the Issue or the Issue
    /// is closed or archived.
    pub(crate) fn note_issue_worker_launch_ended(
        &self,
        operation: &ManagerActionOperationV2,
        state: ManagerActionStateV2,
        outcome: &str,
    ) -> Result<bool> {
        if !matches!(
            state,
            ManagerActionStateV2::Failed
                | ManagerActionStateV2::Blocked
                | ManagerActionStateV2::Revoked
        ) {
            return Ok(false);
        }
        let (Some(binding), ManagerActionOriginV2::Agent { caller }) = (
            operation.context.issue_binding.as_ref(),
            &operation.context.origin,
        ) else {
            return Ok(false);
        };
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let authority =
            match Self::resolve_agent_issue_manager_authority_tx(&tx, *caller, None, true) {
                Ok(authority) => authority,
                Err(_) => {
                    let Some(seat) = self
                        .get_harness_manager(operation.project_id)?
                        .and_then(|config| config.current_session_id)
                    else {
                        return Ok(false);
                    };
                    let Ok(authority) =
                        Self::resolve_agent_issue_manager_authority_tx(&tx, seat, None, true)
                    else {
                        return Ok(false);
                    };
                    authority
                }
            };
        if authority.project_id != operation.project_id {
            return Ok(false);
        }
        let Some(prior) =
            Self::get_issue_in_project_tx(&tx, operation.project_id, binding.issue_id)?
        else {
            return Ok(false);
        };
        if prior.archived_at.is_some()
            || !matches!(prior.status, IssueStatus::Open | IssueStatus::InProgress)
        {
            return Ok(false);
        }
        let worker = operation
            .receipt
            .target_session_id
            .map_or_else(|| "its worker".to_string(), |id| format!("worker {id}"));
        let note = format!(
            "\n\n---\nLaunch (action {}) for this Issue ended {} ({outcome}) before {worker} started; its Issue binding is released and the Issue is free for a relaunch.",
            operation.receipt.operation_id,
            state_name(state),
        );
        Self::agent_update_issue_in_tx(
            &tx,
            authority,
            &AgentUpdateIssueRequestV1 {
                issue_id: Some(binding.issue_id),
                body: Some(format!("{}{note}", prior.body)),
                ..AgentUpdateIssueRequestV1::new(
                    prior.row_version,
                    format!(
                        "launch-issue-worker:{}:ended",
                        operation.receipt.operation_id
                    ),
                )
            },
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// #2091: automatic watches follow the launching manager node's current
    /// seat. The journal pins the launch principal even after the caller loses
    /// its role; caller-facing authorization must not lend it the new seat.
    /// Ordinary agent children retain their original watch owner.
    pub fn automatic_child_watch_owner(&self, owner: Uuid, worker: Uuid) -> Result<Uuid> {
        let principal: Option<(String, i64, String)> = self
            .conn
            .query_row(
                "SELECT manager_session_id,scope_version,project_id FROM harness_manager_v2_operations
             WHERE kind=?1 AND target_session_id=?2
               AND json_extract(payload_json,'$.origin.caller')=?3
               AND json_extract(payload_json,'$.request.operation.action')='create_session'
             ORDER BY rowid DESC LIMIT 1",
                params![ACTION_KIND, worker.to_string(), owner.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((anchor, version, project)) = principal else {
            return Ok(owner);
        };
        if let Some(node) = self.portfolio_origin_node(&anchor, version)? {
            return super::super::portfolio_nodes::node_grant_on(&self.conn, node)?
                .map(|record| record.grant.seat_session_id)
                .ok_or_else(|| refused("manager_node_custody_changed"));
        }
        let anchor =
            Uuid::parse_str(&anchor).map_err(|_| refused("manager_v2_invalid_stored_identity"))?;
        // #1553: an operator replacement also transfers the PM's watches.
        // Area managers retain their own lineage rather than borrowing the PM.
        let area: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM manager_nodes WHERE seat_root_session_id=?1)",
            [anchor.to_string()],
            |row| row.get(0),
        )?;
        if !area {
            let project = Uuid::parse_str(&project)
                .map_err(|_| refused("manager_v2_invalid_stored_identity"))?;
            if let Some(config) = self.get_harness_manager(project)? {
                return config
                    .current_session_id
                    .ok_or_else(|| refused("manager_session_unavailable"));
            }
        }
        self.manager_lineage_tip(anchor)
    }

    /// #2725: `true` while `caller`'s Issue-bound launch of `worker` is still
    /// queued, running or uncertain and the worker session row does not exist
    /// yet. The launch arms the launcher's terminal watch itself the moment it
    /// is established (or ends), so a caller's own `on_terminal` on that id is
    /// already covered and must not be refused `session_not_found`.
    pub fn issue_worker_launch_watch_pending(&self, caller: Uuid, worker: Uuid) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(
               SELECT 1 FROM harness_manager_v2_operations o
               WHERE o.kind=?1 AND o.target_session_id=?2
                 AND o.state IN ('queued','running','uncertain')
                 AND json_extract(o.payload_json,'$.origin.origin')='agent'
                 AND json_extract(o.payload_json,'$.origin.caller')=?3
                 AND json_extract(o.payload_json,'$.issue_binding') IS NOT NULL
                 AND NOT EXISTS(SELECT 1 FROM sessions s WHERE s.id=?2))",
            params![ACTION_KIND, worker.to_string(), caller.to_string()],
            |row| row.get(0),
        )?)
    }

    /// `(manager, worker)` of every succeeded Issue-bound launch whose worker
    /// is still live and has no enabled terminal watch from that manager (#1115).
    /// Coverage is keyed to the launching node's current seat (#2091), just
    /// like arming; a transferred watch counts and a retired-seat watch does
    /// not suppress repair.
    /// A terminal worker is skipped: its watch either fired or cannot be told
    /// apart from a consumed one.
    pub fn list_issue_worker_unwatched(&self) -> Result<Vec<(Uuid, Uuid)>> {
        let mut stmt = self.conn.prepare(
            "SELECT json_extract(o.payload_json,'$.origin.caller'), o.target_session_id
             FROM harness_manager_v2_operations o
             JOIN sessions w ON w.id=o.target_session_id
             WHERE o.kind=?1 AND o.state='succeeded'
               AND json_extract(o.payload_json,'$.origin.origin')='agent'
               AND json_extract(o.payload_json,'$.issue_binding') IS NOT NULL
               AND w.status IN ('Starting','Running','WaitingApproval')",
        )?;
        let rows = stmt
            .query_map(params![ACTION_KIND], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let watches: std::collections::HashSet<_> = self
            .list_enabled_terminal_watches()?
            .into_iter()
            .filter_map(|job| match (job.wake_session_id, job.wake_mode) {
                (Some(owner), rsi_common::types::WakeMode::OnTerminal(worker)) => {
                    Some((owner, worker))
                }
                _ => None,
            })
            .collect();
        let mut out = Vec::new();
        for (caller, worker) in rows {
            let caller = Uuid::parse_str(&caller)
                .map_err(|_| refused("manager_v2_invalid_stored_identity"))?;
            let worker = Uuid::parse_str(&worker)
                .map_err(|_| refused("manager_v2_invalid_stored_identity"))?;
            let owner = match self.automatic_child_watch_owner(caller, worker) {
                Ok(owner) => owner,
                Err(error) => {
                    tracing::warn!(%caller, %worker, %error, "issue worker watch owner unresolved");
                    continue;
                }
            };
            if !watches.contains(&(owner, worker)) {
                out.push((caller, worker));
            }
        }
        Ok(out)
    }

    /// The Issue binding journalled with one admitted action, if any.
    pub(crate) fn manager_action_issue_binding(
        &self,
        operation_id: Uuid,
    ) -> Result<Option<ManagerIssueBindingV1>> {
        let raw: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT json_extract(payload_json,'$.issue_binding')
                 FROM harness_manager_v2_operations WHERE id=?1 AND kind=?2",
                params![operation_id.to_string(), ACTION_KIND],
                |row| row.get(0),
            )
            .optional()?;
        raw.flatten()
            .map(|json| serde_json::from_str(&json).map_err(Into::into))
            .transpose()
    }
}

/// The largest query a `create_session` accepts (`text(query, 32_768)`), less
/// room for the Issue-binding sentence appended after the brief.
const CONTINUATION_QUERY_BUDGET: usize = 32_000;
/// The most bytes of each copied excerpt.
const CONTINUATION_EXCERPT_MAX: usize = 6_000;

/// #1254: the Issue body from its latest handoff on: the last line that is a
/// heading naming a handoff, or a `PIPELINE HANDOFF` line. `None` when the
/// Issue holds no handoff.
fn latest_handoff(body: &str) -> Option<&str> {
    latest_handoff_start(body).map(|start| body[start..].trim())
}

/// Byte offset where [`latest_handoff`] starts.
fn latest_handoff_start(body: &str) -> Option<usize> {
    let mut start = None;
    let mut offset = 0;
    for line in body.split_inclusive('\n') {
        let lower = line.trim().to_ascii_lowercase();
        if (lower.starts_with('#') && lower.contains("handoff"))
            || lower.contains("pipeline handoff")
        {
            start = Some(offset);
        }
        offset += line.len();
    }
    start
}

/// Copied text as a quoted, secret-redacted block of at most `max` bytes
/// plus the truncation marker.
fn quoted_excerpt(text: &str, max: usize) -> String {
    let quoted = rsi_common::daemon_handoff::redact_secrets(text.trim())
        .lines()
        .map(|line| format!("> {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    if quoted.len() <= max {
        return quoted;
    }
    let mut end = max;
    while !quoted.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n> [truncated]", &quoted[..end])
}

/// The most bytes of the reviewed Issue's own text (intent, acceptance
/// criteria) copied into a reviewer's brief.
const REVIEW_ISSUE_TEXT_MAX: usize = 5_000;
/// The largest brief-plus-copied-handoff text: the `create_session` query limit
/// (32768) less the Issue-binding sentence the launch appends after the brief.
const REVIEW_QUERY_BUDGET: usize = 29_000;
/// #218: the largest brief including the related section; the same budget, so
/// the fixed closing section (about 3 KB with the reference rules) still fits.
const RELATED_QUERY_BUDGET: usize = REVIEW_QUERY_BUDGET;
/// The most bytes of the verification evidence block copied into a reviewer's
/// brief (#2157).
const REVIEW_EVIDENCE_MAX: usize = 3_000;
/// The most bytes of the landing-gate block copied into a reviewer's brief
/// (#2727).
const REVIEW_GATE_MAX: usize = 2_600;
/// The most bytes of the preceding handoff copied into a reviewer's brief
/// (#2379).
const REVIEW_PRECEDING_MAX: usize = 6_000;

/// #1590: the Issue body from its latest handoff heading on, preferring a
/// markdown heading naming a handoff (so a `PIPELINE HANDOFF` line inside a
/// handoff does not cut its start off); otherwise [`latest_handoff`].
fn review_handoff_start(body: &str) -> Option<usize> {
    review_handoff_heading_starts(body)
        .last()
        .copied()
        .or_else(|| latest_handoff_start(body))
}

/// Byte offsets of every markdown heading in `body` that names a handoff.
fn review_handoff_heading_starts(body: &str) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut offset = 0;
    for line in body.split_inclusive('\n') {
        let lower = line.trim().to_ascii_lowercase();
        if lower.starts_with('#') && lower.contains("handoff") {
            starts.push(offset);
        }
        offset += line.len();
    }
    starts
}

/// #2379: whether a handoff carries a reviewer's whole verification record: a
/// `scoped_test` run and its test or landing filters. A correction that says
/// "everything else in the previous record stands" does not.
fn handoff_is_self_contained(handoff: &str) -> bool {
    handoff_names_verification(handoff) && handoff.to_ascii_lowercase().contains("filter")
}

/// #2379: the handoff before the one a review brief copies, when that one is
/// not self-contained (a correction refers back to a record the bound reviewer
/// cannot read). `records` are the Issue's newest stored handoff records,
/// oldest first. The handoff before a stored record is the next lower stored
/// record, else the body's latest handoff; before a body handoff it is the
/// previous handoff heading in the body, else the newest stored record.
fn preceding_review_handoff(
    reviewed: &Issue,
    stored: Option<&rsi_common::types::IssueHandoffV1>,
    records: &[rsi_common::types::IssueHandoffV1],
) -> Option<String> {
    let (_, latest) = text_and_handoff(reviewed, stored);
    let latest = latest?;
    if handoff_is_self_contained(latest) {
        return None;
    }
    let body = reviewed.body.as_str();
    let earlier = match stored {
        Some(record) => records
            .iter()
            .rev()
            .find(|earlier| earlier.sequence < record.sequence)
            .map(|earlier| earlier.body.clone())
            .or_else(|| latest_handoff(body).map(str::to_string)),
        None => {
            let starts = review_handoff_heading_starts(body);
            let last = starts.last().copied()?;
            starts
                .iter()
                .rev()
                .find(|start| **start < last)
                .map(|start| body[*start..last].trim().to_string())
                .or_else(|| records.last().map(|record| record.body.clone()))
        }
    }?;
    let earlier = earlier.trim().to_string();
    (!earlier.is_empty() && earlier != latest.trim()).then_some(earlier)
}

/// Copied text as a quoted, secret-redacted block of at most about `max`
/// bytes. An oversized text keeps its head and its TAIL (a handoff ends with
/// its landing filters) and names the omitted byte count between them.
fn quoted_head_tail(text: &str, max: usize) -> String {
    let quote = |text: &str| {
        rsi_common::daemon_handoff::redact_secrets(text.trim())
            .lines()
            .map(|line| format!("> {line}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let quoted = quote(text);
    if quoted.len() <= max {
        return quoted;
    }
    let keep = max.saturating_sub(80);
    let mut head_end = keep * 2 / 5;
    while !quoted.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = quoted.len() - (keep - head_end);
    while !quoted.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let omitted = tail_start - head_end;
    format!(
        "{}\n> [... {omitted} bytes omitted from the middle ...]\n{}",
        &quoted[..head_end],
        &quoted[tail_start..]
    )
}

/// How the daemon wrapped the manager's brief into a journalled launch query.
enum ReplayBrief {
    /// #1845: the brief, plus the latest reviews of this Issue when it has any.
    Plain(i64),
    /// #1590: the reviewed Issue's text and handoff, then the brief.
    Review(i64),
    /// #1254: the baton preamble, then the (review-expanded) brief.
    Continuation { predecessor: Uuid, issue: i64 },
}

/// A replay returns the journalled operation so the original worker is reused
/// (a review or handoff may have grown since), but only when the caller's
/// semantic request is the journalled one: every operation field except the
/// daemon-appended query sections (and, for a continuation, the sandbox source
/// the daemon derived from the predecessor) equals the freshly built
/// `fresh`, and the journalled query carries the caller's brief. Any other
/// change under the same key is `manager_v2_idempotency_conflict`.
fn replayed_operation(
    fresh: &ManagerActionV2,
    prior: ManagerActionV2,
    brief: &str,
    shape: ReplayBrief,
) -> Result<ManagerActionV2> {
    let conflict = || refused("manager_v2_idempotency_conflict");
    let (
        ManagerActionV2::CreateSession {
            query: fresh_query, ..
        },
        ManagerActionV2::CreateSession {
            query: prior_query, ..
        },
    ) = (fresh, &prior)
    else {
        return Err(conflict());
    };
    let continuation = matches!(shape, ReplayBrief::Continuation { .. });
    let strip = |operation: &ManagerActionV2| -> Result<serde_json::Value> {
        let mut value = serde_json::to_value(operation)?;
        if let Some(object) = value.as_object_mut() {
            object.remove("query");
            if continuation {
                object.remove("sandbox_source");
            }
        }
        Ok(value)
    };
    if strip(fresh)? != strip(&prior)? {
        return Err(conflict());
    }
    // The daemon's fixed closing section follows the brief in every query.
    let tail = fresh_query
        .get(brief.trim_end().len()..)
        .ok_or_else(conflict)?;
    // #218: the related section sits between the brief and the closing section.
    let stripped = strip_related_block(prior_query, tail);
    let prior_query = &stripped;
    let trimmed = brief.trim();
    let reviewed = |issue: i64| format!("{trimmed}\n\n---\n\nIssue #{issue} was reviewed. ");
    let matches = prior_query == fresh_query
        || prior_query.ends_with(tail)
            && match shape {
                ReplayBrief::Plain(issue) => prior_query.starts_with(&reviewed(issue)),
                ReplayBrief::Review(reviewed_issue) => {
                    prior_query
                        .starts_with(&format!("You review the work of Issue #{reviewed_issue} ("))
                        && prior_query.ends_with(&format!("\n\n---\n\n{trimmed}{tail}"))
                }
                ReplayBrief::Continuation { predecessor, issue } => {
                    prior_query.starts_with(&format!("You continue worker {predecessor} "))
                        && (prior_query.ends_with(&format!("\n\n---\n\n{trimmed}{tail}"))
                            || prior_query.contains(&format!("\n\n---\n\n{}", reviewed(issue))))
                }
            };
    if matches { Ok(prior) } else { Err(conflict()) }
}

/// An Issue's own text and its complete latest handoff. #1750: a stored
/// handoff record is the complete latest handoff.
fn text_and_handoff<'a>(
    issue: &'a Issue,
    stored: Option<&'a rsi_common::types::IssueHandoffV1>,
) -> (&'a str, Option<&'a str>) {
    match (stored, review_handoff_start(&issue.body)) {
        (Some(record), Some(start)) => (&issue.body[..start], Some(record.body.as_str())),
        (Some(record), None) => (issue.body.as_str(), Some(record.body.as_str())),
        (None, Some(start)) => (&issue.body[..start], Some(&issue.body[start..])),
        (None, None) => (issue.body.as_str(), None),
    }
}

/// #218: the heading that opens the daemon-attached related-Issue section of a
/// brief. A replay removes the section (it sits between the manager's brief and
/// the fixed closing section) before it compares the journalled query.
const RELATED_BLOCK_MARKER: &str =
    "\n\n---\n\n## Related Issues (read-only context, snapshot at launch)";

/// `text` without the daemon-attached related section, when `query` ends with
/// the fixed closing section `tail` and holds that section before it.
fn strip_related_block(query: &str, tail: &str) -> String {
    let Some(head) = query.strip_suffix(tail) else {
        return query.to_string();
    };
    match head.rfind(RELATED_BLOCK_MARKER) {
        Some(start) => format!("{}{tail}", &head[..start]),
        None => query.to_string(),
    }
}

/// Copied text as a quoted, secret-redacted block, whole.
fn quoted_whole(text: &str) -> String {
    rsi_common::daemon_handoff::redact_secrets(text.trim())
        .lines()
        .map(|line| format!("> {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// #218: the brief with each related Issue's COMPLETE text and latest handoff
/// appended as read-only context, in the order the manager listed them. An
/// Issue is copied whole or not at all: one that does not fit the remaining
/// budget is named with its size so the worker reads it in full with
/// `AgentGetIssue` (it may), and a text is never cut mid-way. The copy is the
/// Issue as it stood at launch.
fn related_brief(
    publication: i64,
    related: &[(Issue, Option<rsi_common::types::IssueHandoffV1>, bool)],
    skipped: &[i64],
    brief: &str,
) -> String {
    let mut out = format!(
        "{}{RELATED_BLOCK_MARKER}\n\nThe daemon copied the Issues below as they stood at launch (#218). They are read-only context: you may re-read each live with AgentGetIssue, but you cannot update, change the status of or hand off on any of them. Your only publication Issue is #{publication}, whatever the brief above says.\n\n{}",
        brief.trim_end(),
        rsi_common::daemon_handoff::UNTRUSTED_TEXT_NOTICE
    );
    let carried: Vec<String> = related
        .iter()
        .filter(|(_, _, carried)| *carried)
        .map(|(issue, _, _)| format!("#{}", issue.display_number))
        .collect();
    if !carried.is_empty() {
        out.push_str(&format!(
            "\n\nCarried over from earlier launches of Issue #{publication} (#275): {}.",
            carried.join(", ")
        ));
    }
    if !skipped.is_empty() {
        let names: Vec<String> = skipped.iter().map(|n| format!("#{n}")).collect();
        out.push_str(&format!(
            "\n\nNot carried over from earlier launches (no longer exists, archived or over the bound): {}.",
            names.join(", ")
        ));
    }
    for (issue, stored, carried) in related {
        let (text, handoff) = text_and_handoff(issue, stored.as_ref());
        let heading = format!(
            "\n\n### Related Issue #{} (\"{}\", status {:?}{})\n\n",
            issue.display_number,
            issue.title.replace('\n', " "),
            issue.status,
            if *carried { ", carried over" } else { "" }
        );
        let text_block = quoted_whole(text);
        let handoff_block = handoff.map_or_else(
            || "> (the Issue holds no handoff section)".to_string(),
            quoted_whole,
        );
        let entry = format!(
            "#### Issue #{} text\n\n{text_block}\n\n#### Issue #{} latest handoff\n\n{handoff_block}",
            issue.display_number, issue.display_number
        );
        out.push_str(&heading);
        if out.len() + entry.len() <= RELATED_QUERY_BUDGET {
            out.push_str(&entry);
            continue;
        }
        out.push_str(&format!(
            "> (the text and latest handoff are {} bytes and do not fit this brief: read Issue #{} with AgentGetIssue)",
            entry.len(),
            issue.display_number
        ));
    }
    out
}

/// #1590: a reviewer's brief: the reviewed Issue's text and its complete
/// latest handoff (the bound reviewer cannot read that Issue), then the
/// manager's brief.
fn review_brief(
    reviewed: &Issue,
    stored: Option<&rsi_common::types::IssueHandoffV1>,
    records: &[rsi_common::types::IssueHandoffV1],
    evidence: &str,
    gate: &str,
    brief: &str,
) -> String {
    let (text, handoff) = text_and_handoff(reviewed, stored);
    let header = format!(
        "You review the work of Issue #{} (\"{}\", status {:?}). You cannot read that Issue: the daemon copied its text and latest handoff below (#1590). Check them against the committed code the manager names, use the handoff's test filters and landing filters and the landing gate block below (the queue's derived filters and gate result: do not re-run what it shows ran), and report on this Issue.\n\n{}",
        reviewed.display_number,
        reviewed.title.replace('\n', " "),
        reviewed.status,
        rsi_common::daemon_handoff::UNTRUSTED_TEXT_NOTICE
    );
    let text_block = quoted_head_tail(text, REVIEW_ISSUE_TEXT_MAX);
    // #2157: an empty handoff, or one that names no scoped_test run, leaves the
    // reviewer no verification record (it cannot read the implementer's Issue
    // or jobs): add what the daemon holds, bounded, and name what it lacks.
    let evidence_block = if handoff.is_some_and(handoff_names_verification) {
        String::new()
    } else {
        let quoted = quoted_head_tail(evidence, REVIEW_EVIDENCE_MAX);
        format!(
            "\n\n## Issue #{} verification evidence (daemon-read)\n\nThe handoff above is empty or names no scoped_test run, so the daemon copied the implementer's records (#2157). Read-only; each line says when a record is unavailable.\n\n{quoted}",
            reviewed.display_number
        )
    };
    // #2727: the landed queue entry's derived filters and gate result, whatever
    // the handoff says. A bound reviewer cannot read the merge queue
    // (`queue_not_authorized`), so without this block it re-runs a test the gate
    // already ran.
    let gate_block = format!(
        "\n\n## Issue #{} landing gate (daemon-read)\n\nThe merge queue derives the landing filters itself and stays authoritative over any filters written in the handoff (#2727). The gate records the filters it derived and the tests that failed, not each passing test: a test inside a listed filter ran in that gate and passed unless it is listed under failing tests, so do not re-run it as a job; run only what a filter does not cover. Read-only; a line says when a record is unavailable.\n\n{}",
        reviewed.display_number,
        quoted_head_tail(gate, REVIEW_GATE_MAX)
    );
    // #2379: a latest handoff that is not self-contained (a correction that
    // refers back to "the previous record") is followed by the handoff before
    // it, so the reviewer gets the test and landing filters it points at.
    let preceding_block = preceding_review_handoff(reviewed, stored, records).map_or_else(
        String::new,
        |earlier| {
            format!(
                "\n\n## Issue #{} preceding handoff\n\nThe latest handoff above is not self-contained (it names no scoped_test run or no filters, as a correction does), so the daemon copied the handoff before it (#2379). Where the latest handoff says its earlier record stands, this is that record. The merge queue derives the landing filters itself and stays authoritative over any filters written here.\n\n{}",
                reviewed.display_number,
                quoted_head_tail(&earlier, REVIEW_PRECEDING_MAX)
            )
        },
    );
    let room = REVIEW_QUERY_BUDGET
        .saturating_sub(
            brief.len()
                + header.len()
                + text_block.len()
                + evidence_block.len()
                + gate_block.len()
                + preceding_block.len()
                + 256,
        )
        .max(1_000);
    let handoff_block = handoff.map_or_else(
        || "> (the Issue holds no handoff section)".to_string(),
        |text| quoted_head_tail(text, room),
    );
    format!(
        "{header}\n\n## Issue #{} text\n\n{text_block}\n\n## Issue #{} latest handoff\n\n{handoff_block}{preceding_block}{evidence_block}{gate_block}\n\n---\n\n{}",
        reviewed.display_number,
        reviewed.display_number,
        brief.trim()
    )
}

/// #1845: a revision worker's brief: the manager's brief, then the latest
/// review (verdict and findings: the Issue's latest handoff) of each review
/// Issue, newest first. A review is copied whole or not at all: one that does
/// not fit the remaining budget is named instead, for the worker to read in
/// full with `AgentGetIssue` (a bound worker may read its reviews), so a
/// finding is never cut in the middle. `reviews` empty returns `brief`.
fn revision_brief(reviewed: &Issue, reviews: &[Issue], brief: &str) -> String {
    if reviews.is_empty() {
        return brief.to_string();
    }
    let mut out = format!(
        "{}\n\n---\n\nIssue #{} was reviewed. The daemon copied the latest review of each review Issue below (#1845); you may read those review Issues, and the Issues your own Issue and launch reference (#218), with AgentGetIssue (read-only); publish to your own Issue only.\n\n{}",
        brief.trim(),
        reviewed.display_number,
        rsi_common::daemon_handoff::UNTRUSTED_TEXT_NOTICE
    );
    for review in reviews {
        let heading = format!(
            "\n\n## Review Issue #{} (\"{}\", status {:?}) latest review\n\n",
            review.display_number,
            review.title.replace('\n', " "),
            review.status
        );
        let Some(start) = review_handoff_start(&review.body) else {
            out.push_str(&heading);
            out.push_str("> (the Issue holds no review section yet)");
            continue;
        };
        let quoted = rsi_common::daemon_handoff::redact_secrets(review.body[start..].trim())
            .lines()
            .map(|line| format!("> {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let fits = out.len() + heading.len() + quoted.len() <= REVIEW_QUERY_BUDGET;
        out.push_str(&heading);
        if fits {
            out.push_str(&quoted);
        } else {
            out.push_str(&format!(
                "> (the latest review is {} bytes and does not fit this brief: read Issue #{} with AgentGetIssue)",
                quoted.len(),
                review.display_number
            ));
        }
    }
    out
}

/// Why a worker is being continued; names the reason in its brief.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContinuationCause {
    /// Its context filled and it passed the baton (#1254).
    Baton,
    /// Its provider reported the selected model at capacity (#2109).
    ProviderCapacity,
}

impl ContinuationCause {
    fn describe(self) -> &'static str {
        match self {
            Self::Baton => "its context filled and it passed the baton (#1254)",
            Self::ProviderCapacity => {
                "its provider reported the selected model at capacity and the daemon could not resume it (#2109)"
            }
        }
    }
}

/// The successor's brief: the baton preamble, then the manager's brief.
fn continuation_brief(
    predecessor: Uuid,
    head: &str,
    dirty: bool,
    final_message: &str,
    handoff: Option<&str>,
    brief: &str,
    cause: ContinuationCause,
) -> String {
    let dirty_note = if dirty {
        " Its sandbox also had uncommitted changes; they were NOT copied, so rebuild anything its handoff names as uncommitted."
    } else {
        ""
    };
    let header = format!(
        "You continue worker {predecessor} on this Issue: {}. Your sandbox branches from its committed HEAD {head}.{dirty_note} Read its final message and the Issue's latest handoff below, check them against the code, then carry on without redoing finished work.\n\n{}",
        cause.describe(),
        rsi_common::daemon_handoff::UNTRUSTED_TEXT_NOTICE
    );
    let room = CONTINUATION_QUERY_BUDGET.saturating_sub(brief.len() + header.len() + 256) / 2;
    let max = room.min(CONTINUATION_EXCERPT_MAX);
    let message = if final_message.trim().is_empty() {
        "> (no final message was recorded)".to_string()
    } else {
        quoted_excerpt(final_message, max)
    };
    let handoff = handoff.map_or_else(
        || "> (the Issue holds no handoff section)".to_string(),
        |text| quoted_excerpt(text, max),
    );
    format!(
        "{header}\n\n## Predecessor's final message\n\n{message}\n\n## The Issue's latest handoff\n\n{handoff}\n\n---\n\n{}",
        brief.trim()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::make_test_session;
    use rsi_common::types::NewIssue;

    /// A store with one Issue, a manager session and two live (Running)
    /// worker sessions A and B, all in the seeded test project.
    #[allow(clippy::unwrap_used)]
    fn binding_fixture() -> (Store, Uuid, Uuid, Uuid, Uuid, Uuid) {
        let store = Store::open_in_memory().unwrap();
        let project = crate::store::d04_test_project_id();
        let issue = store
            .create_issue(&NewIssue {
                project_id: project,
                title: "bound Issue".to_string(),
                body: String::new(),
                priority: None,
                labels: Vec::new(),
                created_by_session_id: None,
                assignee: None,
                idea_id: None,
                source_event_id: None,
                source_finding_ref: None,
            })
            .unwrap();
        let manager = make_test_session();
        store.insert_session(&manager).unwrap();
        let mut ids = Vec::new();
        for _ in 0..2 {
            let mut worker = make_test_session();
            worker.status = SessionStatus::Running;
            store.insert_session(&worker).unwrap();
            ids.push(worker.id);
        }
        (store, project, issue.id, manager.id, ids[0], ids[1])
    }

    /// Admits (inserts) one Issue-bound launch of `worker` with an explicit
    /// operation id, state and `created_at`, as the journal row would read.
    #[allow(clippy::unwrap_used)]
    #[allow(clippy::too_many_arguments)]
    fn admit_launch(
        store: &Store,
        project: Uuid,
        issue: Uuid,
        manager: Uuid,
        worker: Uuid,
        operation: Uuid,
        state: &str,
        created_at: &str,
    ) {
        let payload = serde_json::json!({
            "issue_binding": {"issue_id": issue.to_string(), "display_number": 1}
        });
        store
            .conn
            .execute(
                "INSERT INTO harness_manager_v2_operations(id,project_id,manager_session_id,
                    scope_version,policy_version,idempotency_key,fingerprint,kind,payload_json,
                    state,target_session_id,outcome_json,not_before,created_at,updated_at)
                 VALUES(?1,?2,?3,1,1,?1,'fixture',?4,?5,?6,?7,'{}',?8,?8,?8)",
                params![
                    operation.to_string(),
                    project.to_string(),
                    manager.to_string(),
                    ACTION_KIND,
                    payload.to_string(),
                    state,
                    worker.to_string(),
                    created_at
                ],
            )
            .unwrap();
    }

    /// #2448: a session node of a live topology execution may run the project
    /// summary dedupe search of `AgentListIssues`, and nothing wider.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn topology_session_node_dedupes_with_title_contains_only() {
        use rsi_common::agent_control_schema::AgentControlVerbV1 as Verb;
        use rsi_common::rpc::AgentListIssuesRequestV1;
        use rsi_common::types::AgentIssuePageV1;

        let (store, project, issue, _manager, node, bystander) = binding_fixture();
        store
            .conn
            .execute(
                "UPDATE issues SET body='SECRET-BODY' WHERE id=?1",
                [issue.to_string()],
            )
            .unwrap();
        let now = "2026-10-14T00:00:00.000000000Z";
        let execution = Uuid::new_v4();
        store
            .conn
            .execute(
                "INSERT INTO topology_executions (id,topology_name_snapshot,workflow_id,definition_json,definition_digest,project_id,requested_by_kind,repo_root,base_commit,custody_plan_json,status,created_at,updated_at)
                 VALUES (?1,'dedupe','wf','{}','d',?2,'manager','/tmp/repo','0000000000000000000000000000000000000000','{}','running',?3,?3)",
                params![execution.to_string(), project.to_string(), now],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO topology_node_attempts (id,execution_id,node_id,iteration,attempt_no,node_kind,status,dedup_key,session_id,input_json,created_at,updated_at)
                 VALUES (?1,?2,'followup',0,1,'session','running',?3,?4,'{}',?5,?5)",
                params![
                    Uuid::new_v4().to_string(),
                    execution.to_string(),
                    format!("dedup-{execution}"),
                    node.to_string(),
                    now
                ],
            )
            .unwrap();
        let list = |caller: Uuid, title: Option<&str>, project_id: Option<Uuid>| {
            store.agent_list_issues(
                caller,
                &AgentListIssuesRequestV1 {
                    title_contains: title.map(str::to_string),
                    project_id,
                    ..Default::default()
                },
            )
        };

        // The node's catalog lists the dedupe read and no other Issue read.
        let verbs = store.agent_authority_projection(node).unwrap().verbs;
        assert!(verbs.contains(&Verb::ListIssues));
        assert!(!verbs.contains(&Verb::GetIssue));
        assert!(!verbs.contains(&Verb::UpdateIssue));
        assert!(
            !store
                .agent_authority_projection(bystander)
                .unwrap()
                .verbs
                .contains(&Verb::ListIssues)
        );

        // A title search returns summary rows from the execution's project.
        let page = list(node, Some("BOUND issue"), None).unwrap();
        let json = serde_json::to_string(&page).unwrap();
        assert!(!json.contains("SECRET-BODY"), "{json}");
        let AgentIssuePageV1::Summary(page) = page else {
            panic!("expected summary rows: {json}");
        };
        assert_eq!(page.issues.len(), 1);
        assert_eq!(page.issues[0].id, issue);

        // Without a title filter, with a blank one, or with a project_id it is refused.
        for (title, project_id) in [
            (None, None),
            (Some("  "), None),
            (Some("bound"), Some(project)),
        ] {
            assert!(list(node, title, project_id).is_err(), "{title:?}");
        }
        // #2425: the missing-title refusal names the field to supply.
        for title in [None, Some("  ")] {
            let crate::error::DaemonError::StructuredRpc { data, .. } =
                list(node, title, None).unwrap_err()
            else {
                panic!("expected a structured Issue refusal");
            };
            assert_eq!(data["code"], "invalid_request", "{data}");
            assert_eq!(data["validation"]["class"], "missing_field", "{data}");
            assert_eq!(data["validation"]["field"], "title_contains", "{data}");
        }
        // A session that is no node holds no such read.
        let denied = list(bystander, Some("bound"), None)
            .unwrap_err()
            .to_string();
        assert!(denied.contains("authority_denied"), "{denied}");

        // The read ends with the attempt.
        store
            .conn
            .execute(
                "UPDATE topology_node_attempts SET status='succeeded' WHERE session_id=?1",
                [node.to_string()],
            )
            .unwrap();
        let denied = list(node, Some("bound"), None).unwrap_err().to_string();
        assert!(denied.contains("authority_denied"), "{denied}");
    }

    /// #1321: an admitted later launch of the same Issue supersedes the
    /// predecessor whatever its outcome; a successor that failed, blocked or
    /// was revoked does not hand a resumed predecessor its append right back.
    /// #1590: an oversized handoff keeps its head and its tail (the landing
    /// filters), names the omitted bytes, and fits the query budget.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn quoted_head_tail_keeps_the_tail_of_an_oversized_handoff() {
        let text = format!("start\n{}\nLANDING FILTERS: end", "x".repeat(20_000));
        let block = quoted_head_tail(&text, 4_000);
        assert!(block.len() <= 4_000, "{}", block.len());
        assert!(block.starts_with("> start"));
        assert!(block.ends_with("> LANDING FILTERS: end"));
        assert!(block.contains("bytes omitted from the middle"));
        let small = quoted_head_tail("a\nb", 4_000);
        assert_eq!(small, "> a\n> b");
    }

    /// #1845: a revision brief carries each review whole, or names it; it never
    /// cuts one, and a brief with no review is returned unchanged.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn revision_brief_copies_whole_reviews_or_names_them() {
        let issue = |number: i64, body: String| Issue {
            id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            display_number: number,
            title: format!("review {number}"),
            body,
            status: IssueStatus::Open,
            priority: None,
            labels: Vec::new(),
            created_by_session_id: None,
            assignee: None,
            idea_id: None,
            source_event_id: None,
            source_finding_ref: None,
            row_version: 1,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            closed_at: None,
            archived_at: None,
        };
        let reviewed = issue(7, String::new());
        assert_eq!(revision_brief(&reviewed, &[], "Do it."), "Do it.");
        let small = issue(8, "Review.\n\n## Review handoff\nF1 kept\n\nF2 kept".into());
        let huge = issue(
            9,
            format!(
                "Review.\n\n## Review handoff\n{}",
                "finding\n".repeat(5_000)
            ),
        );
        let brief = revision_brief(&reviewed, &[small, huge], "Do it.");
        assert!(brief.starts_with("Do it."));
        assert!(brief.contains("> F1 kept\n> \n> F2 kept"), "{brief}");
        assert!(
            brief.contains("read Issue #9 with AgentGetIssue"),
            "{brief}"
        );
        assert!(!brief.contains("> finding"));
        assert!(brief.len() <= REVIEW_QUERY_BUDGET);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    fn plain_issue(number: i64, body: String) -> Issue {
        Issue {
            id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            display_number: number,
            title: format!("related {number}"),
            body,
            status: IssueStatus::Open,
            priority: None,
            labels: Vec::new(),
            created_by_session_id: None,
            assignee: None,
            idea_id: None,
            source_event_id: None,
            source_finding_ref: None,
            row_version: 1,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            closed_at: None,
            archived_at: None,
        }
    }

    /// #218: a related Issue is copied with its complete text and latest
    /// handoff (end markers included), or named when it does not fit, never cut;
    /// the section is inserted before the closing section and a replay strips it.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn related_brief_copies_whole_texts_with_end_markers_or_names_them() {
        let small = plain_issue(8, "Intent.\n\n## Handoff\nF1 first\n\nF2 LAST_END".into());
        let huge = plain_issue(
            9,
            format!("Intent.\n\n## Handoff\n{}", "finding\n".repeat(5_000)),
        );
        let stored = plain_issue(10, "Intent only.".into());
        let record = rsi_common::types::IssueHandoffV1 {
            sequence: 1,
            author_session_id: Uuid::new_v4(),
            body: "RECORD first\nRECORD_END".into(),
            created_at: chrono::Utc::now(),
        };
        let brief = related_brief(
            7,
            &[
                (small, None, false),
                (huge, None, false),
                (stored, Some(record), true),
            ],
            &[33],
            "Do it.\n",
        );
        assert!(
            brief.contains("Carried over from earlier launches of Issue #7 (#275): #10."),
            "{brief}"
        );
        assert!(brief.contains("Not carried over from earlier launches"));
        assert!(brief.contains("#33."), "{brief}");
        assert!(
            brief.starts_with(&format!("Do it.{RELATED_BLOCK_MARKER}")),
            "{brief}"
        );
        assert!(brief.contains("> F2 LAST_END"));
        assert!(brief.contains("> RECORD_END"));
        assert!(brief.contains("> Intent only."));
        assert!(brief.contains("Your only publication Issue is #7"));
        assert!(
            brief.contains("read Issue #9 with AgentGetIssue"),
            "{brief}"
        );
        assert!(!brief.contains("> finding"));
        assert!(brief.len() <= RELATED_QUERY_BUDGET);

        // The closing section follows the related section; a replay strips it.
        let tail = "\n\nYou are bound to Issue #7: closing.";
        let query = format!("{brief}{tail}");
        assert_eq!(strip_related_block(&query, tail), format!("Do it.{tail}"));
        let plain = format!("Do it.{tail}");
        assert_eq!(strip_related_block(&plain, tail), plain);
        assert_eq!(strip_related_block(&query, "other tail"), query);
    }

    /// #218: a successor keeps its predecessor's reads, attachments included,
    /// and the union stays inside the selector bound.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn launch_read_scope_inherits_and_refuses_overflow() {
        let mut scope = LaunchReadScope {
            related: vec![1],
            brief_refs: vec![(10, 14)],
        };
        scope
            .inherit(LaunchReadScope {
                related: vec![20, 1],
                brief_refs: vec![(10, 14), (30, 30)],
            })
            .unwrap();
        assert_eq!(scope.brief_refs, vec![(10, 14), (30, 30), (20, 20), (1, 1)]);
        assert!(scope.selects(12) && scope.selects(20) && scope.selects(1));
        assert!(!scope.selects(15));
        let mut full = LaunchReadScope {
            related: Vec::new(),
            brief_refs: (1..=MANAGER_ISSUE_REFERENCE_SELECTOR_MAX as i64)
                .map(|n| (n, n))
                .collect(),
        };
        assert_eq!(
            full.inherit(LaunchReadScope {
                related: vec![9_000],
                brief_refs: Vec::new(),
            }),
            Err(MANAGER_ISSUE_WORKER_REFERENCES_OVERFLOW)
        );
    }

    /// #2157: a reviewer's brief for an Issue with no handoff carries the
    /// implementer's final RESULT line, its latest succeeded scoped_test job
    /// receipt and the merge-queue outcome; a handoff that names a scoped_test
    /// run keeps the brief free of the evidence block.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn review_brief_carries_verification_evidence_when_the_handoff_is_empty() {
        use crate::store::agent_jobs::NewAgentJob;
        use crate::store::rolling_queue::NewQueueEntry;
        use rsi_common::agent_jobs::{AgentJobResultV1, JobParams, JobState, JobWake};
        use rsi_common::rolling_queue::RollingQueueBinding;
        use rsi_common::types::{ConversationEvent, EventType, Role};

        let (store, project, issue_id, manager, worker, _other) = binding_fixture();
        let reviewed = store.get_issue(issue_id).unwrap().unwrap();
        let brief_of = |reviewed: &Issue| {
            let evidence = Store::review_verification_evidence(&store.conn, project, issue_id);
            let gate = Store::review_landing_gate_evidence(&store.conn, project, issue_id);
            review_brief(reviewed, None, &[], &evidence, &gate, "Review it.")
        };

        // No implementer session: the brief says so instead of saying nothing.
        let bare = brief_of(&reviewed);
        assert!(
            bare.contains("> (the Issue holds no handoff section)"),
            "{bare}"
        );
        assert!(
            bare.contains("no implementer session is recorded for the Issue"),
            "{bare}"
        );

        // An implementer with a final RESULT line, a scoped_test job, a queue entry.
        let sha = "a".repeat(40);
        admit_launch(
            &store,
            project,
            issue_id,
            manager,
            worker,
            Uuid::new_v4(),
            "succeeded",
            "2026-10-13T10:00:00.000000000Z",
        );
        store
            .insert_event(&ConversationEvent {
                id: 0,
                session_id: worker,
                sequence: 1,
                event_type: EventType::Message,
                role: Some(Role::Assistant),
                content: format!("Done.\nRESULT {sha} cover the review brief\nFriction: none"),
                tool_name: None,
                tool_input: None,
                created_at: chrono::Utc::now(),
                offload_id: None,
                tool_use_id: None,
                metadata: None,
            })
            .unwrap();
        let job_id = Uuid::new_v4();
        store
            .insert_agent_job(
                &NewAgentJob {
                    id: job_id,
                    owner_session_id: worker,
                    project_id: Some(project),
                    name: None,
                    params: serde_json::from_value::<JobParams>(serde_json::json!({
                        "kind": "test",
                        "scoped_test": {"base": "origin/rolling"}
                    }))
                    .unwrap(),
                    cwd: "/tmp/sandbox".into(),
                    unit_name: format!("rsi-job-{job_id}"),
                    log_path: format!("/tmp/jobs/{job_id}.log"),
                    status_path: format!("/tmp/jobs/{job_id}.status"),
                    idempotency_key: None,
                    wake: JobWake::None,
                },
                chrono::Utc::now(),
            )
            .unwrap();
        store
            .settle_agent_job(
                job_id,
                JobState::Succeeded,
                &AgentJobResultV1 {
                    exit_code: Some(0),
                    receipt: Some(serde_json::json!({
                        "ok": true,
                        "base": "origin/rolling",
                        "head": sha,
                        "packages": [{"package": "rsid-store", "status": "passed", "completed": 41}]
                    })),
                    ..AgentJobResultV1::default()
                },
                false,
                chrono::Utc::now(),
            )
            .unwrap();
        let (entry, _) = store
            .enqueue_rolling_queue_source(
                &NewQueueEntry {
                    project_id: Some(project),
                    repo_path: "/tmp/repo".into(),
                    source_commit: sha.clone(),
                    source_session_id: manager,
                    owner_epic_id: None,
                    binding: RollingQueueBinding::Unbound,
                    work_key: None,
                    migration_version: None,
                    hot_files: Vec::new(),
                    test_filters: vec![
                        "rsid=rolling_queue".into(),
                        "rsid-store=issue_worker".into(),
                    ],
                    fixes_tests: Vec::new(),
                    idempotency_key: "land".into(),
                },
                chrono::Utc::now(),
            )
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE rolling_queue_entries SET state='published', outcome_json=?2,
                 finished_at='2026-10-13T11:00:00.000000000Z' WHERE id=?1",
                params![
                    entry.id.to_string(),
                    serde_json::json!({
                        "landed_sha": "b".repeat(40),
                        "refusal": null,
                        "failing_tests": ["store::tests::baseline_red"],
                        "detail": "gate ok\ngate_timing=selection=closure selected=3 reached=3"
                    })
                    .to_string()
                ],
            )
            .unwrap();

        let brief = brief_of(&reviewed);
        assert!(
            brief.contains("verification evidence (daemon-read)"),
            "{brief}"
        );
        assert!(brief.contains(&format!(
            "> final RESULT line: RESULT {sha} cover the review brief"
        )));
        assert!(
            !brief.contains("Friction: none"),
            "only the RESULT line is copied"
        );
        assert!(brief.contains(&format!("> scoped_test job {job_id}: head={sha} base=origin/rolling ok=true completed=rsid-store:41")), "{brief}");
        assert!(
            brief.contains(&format!(
                "> merge queue: source={sha} state=published landed_sha={}",
                "b".repeat(40)
            )),
            "{brief}"
        );
        assert!(
            brief.contains("gate_timing=selection=closure selected=3 reached=3"),
            "{brief}"
        );
        assert!(brief.len() <= REVIEW_QUERY_BUDGET);

        // #2727: the landing gate's derived filters and result ride in the brief.
        assert!(brief.contains("landing gate (daemon-read)"), "{brief}");
        assert!(
            brief.contains("> derived filters (2): rsid=rolling_queue, rsid-store=issue_worker"),
            "{brief}"
        );
        assert!(
            brief.contains("> failing tests (1): store::tests::baseline_red"),
            "{brief}"
        );
        assert!(
            brief.contains(&format!(
                "source={sha} state=published landed_sha={}",
                "b".repeat(40)
            )),
            "{brief}"
        );

        // The gate block rides even when the handoff names its scoped_test run
        // (the evidence block is then left out): the reviewer reads no queue.
        let handoff = rsi_common::types::IssueHandoffV1 {
            sequence: 1,
            author_session_id: worker,
            body: "Tests: scoped_test job abc, ok".to_string(),
            created_at: chrono::Utc::now(),
        };
        let evidence = Store::review_verification_evidence(&store.conn, project, issue_id);
        let gate = Store::review_landing_gate_evidence(&store.conn, project, issue_id);
        let named = review_brief(
            &reviewed,
            Some(&handoff),
            &[],
            &evidence,
            &gate,
            "Review it.",
        );
        assert!(
            !named.contains("verification evidence (daemon-read)"),
            "{named}"
        );
        assert!(
            named.contains("> derived filters (2): rsid=rolling_queue"),
            "{named}"
        );

        // A handoff that names its scoped_test run needs no evidence block.
        assert!(handoff_names_verification("Tests: scoped_test job abc, ok"));
        assert!(!handoff_names_verification("Done, tests green."));
    }

    /// #1590: the handoff starts at the last handoff HEADING, so a
    /// `PIPELINE HANDOFF` line inside it does not cut its start off.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn review_handoff_starts_at_the_last_handoff_heading() {
        let body = "Build it.\n\n## Handoff 1\nold\n\n## Handoff 2\nnew\nPIPELINE HANDOFF — IMPLEMENTATION:\nfilters";
        let start = review_handoff_start(body).unwrap();
        assert!(body[start..].starts_with("## Handoff 2"));
        let bare = "Build it.\nPIPELINE HANDOFF — X\nrest";
        assert!(bare[review_handoff_start(bare).unwrap()..].starts_with("PIPELINE HANDOFF"));
        assert_eq!(review_handoff_start("Build it."), None);
        // The fallback start is exact whatever whitespace trails the body.
        let spaced = "Build it.\nPIPELINE HANDOFF — X\nrest\n \n\n";
        assert_eq!(review_handoff_start(spaced), spaced.find("PIPELINE"));
        assert_eq!(review_handoff_start(spaced), latest_handoff_start(spaced));
    }

    /// #2379: a correction handoff that says the earlier record stands does not
    /// strand the reviewer: the brief copies the handoff before it, from the body
    /// or from the stored records; a self-contained latest handoff adds nothing.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn review_brief_copies_the_preceding_handoff_when_the_latest_is_a_correction() {
        let issue = |body: String| Issue {
            id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            display_number: 2050,
            title: "reviewed".into(),
            body,
            status: IssueStatus::Open,
            priority: None,
            labels: Vec::new(),
            created_by_session_id: None,
            assignee: None,
            idea_id: None,
            source_event_id: None,
            source_finding_ref: None,
            row_version: 1,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            closed_at: None,
            archived_at: None,
        };
        let full = "scoped_test job j1 exit 0\nlanding filters: -E 'test(foo)'";
        let correction =
            "Correction: the commit is abc. Everything else in the previous record stands.";
        let preceding = |brief: &str| brief.contains("preceding handoff");

        // Both handoffs in the body.
        let body = issue(format!(
            "Build it.\n\n## Handoff 1\n{full}\n\n## Handoff 2\n{correction}"
        ));
        let brief = review_brief(&body, None, &[], "evidence", "gate", "Review it.");
        assert!(preceding(&brief), "{brief}");
        assert!(
            brief.contains("> landing filters: -E 'test(foo)'"),
            "{brief}"
        );
        assert!(
            brief.contains("> Correction: the commit is abc."),
            "{brief}"
        );
        assert!(
            brief.contains("queue derives the landing filters itself"),
            "{brief}"
        );

        // A self-contained latest handoff has no preceding section.
        let complete = issue(format!(
            "Build it.\n\n## Handoff 1\nold\n\n## Handoff 2\n{full}"
        ));
        assert!(!preceding(&review_brief(
            &complete,
            None,
            &[],
            "e",
            "g",
            "Review it."
        )));

        // A correction with nothing before it has nothing to copy.
        let alone = issue(format!("Build it.\n\n## Handoff 1\n{correction}"));
        assert!(!preceding(&review_brief(
            &alone,
            None,
            &[],
            "e",
            "g",
            "Review it."
        )));

        // Stored records: the next lower record precedes the newest.
        let record = |sequence: i64, text: &str| rsi_common::types::IssueHandoffV1 {
            sequence,
            author_session_id: Uuid::new_v4(),
            body: text.to_string(),
            created_at: chrono::Utc::now(),
        };
        let first = record(1, full);
        let second = record(2, correction);
        let stored_issue = issue("Build it.".into());
        let brief = review_brief(
            &stored_issue,
            Some(&second),
            &[first.clone(), second.clone()],
            "e",
            "g",
            "Review it.",
        );
        assert!(preceding(&brief), "{brief}");
        assert!(
            brief.contains("> landing filters: -E 'test(foo)'"),
            "{brief}"
        );

        // A stored record with an older body handoff: the body handoff precedes it.
        let body_older = issue(format!("Build it.\n\n## Handoff 1\n{full}"));
        let brief = review_brief(
            &body_older,
            Some(&second),
            &[second.clone()],
            "e",
            "g",
            "Review it.",
        );
        assert!(
            brief.contains("> landing filters: -E 'test(foo)'"),
            "{brief}"
        );

        // A body correction after a stored record: the record precedes it.
        let body_newer = issue(format!("Build it.\n\n## Handoff 2\n{correction}"));
        let brief = review_brief(&body_newer, None, &[first], "e", "g", "Review it.");
        assert!(
            brief.contains("> landing filters: -E 'test(foo)'"),
            "{brief}"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn any_admitted_successor_launch_supersedes_the_predecessor_whatever_its_outcome() {
        for outcome in [
            "queued",
            "running",
            "uncertain",
            "succeeded",
            "failed",
            "blocked",
            "revoked",
        ] {
            let (store, project, issue, manager, a, b) = binding_fixture();
            let a_launch = Uuid::new_v4();
            admit_launch(
                &store,
                project,
                issue,
                manager,
                a,
                a_launch,
                "succeeded",
                "2026-10-06T10:00:00.000000000Z",
            );
            assert_eq!(
                Store::live_issue_binding_for_worker_on(&store.conn, a).unwrap(),
                Some((project, issue, a_launch)),
                "A holds the binding before any successor ({outcome})"
            );
            admit_launch(
                &store,
                project,
                issue,
                manager,
                b,
                Uuid::new_v4(),
                outcome,
                "2026-10-06T11:00:00.000000000Z",
            );
            // A is resumed (Running) and its own launch still succeeded.
            assert_eq!(
                Store::live_issue_binding_for_worker_on(&store.conn, a).unwrap(),
                None,
                "a {outcome} successor launch keeps the resumed predecessor refused"
            );
            assert_eq!(
                Store::bound_issue_for_worker_on(&store.conn, a).unwrap(),
                Some((project, issue)),
                "read authority is unchanged ({outcome})"
            );
        }
    }

    /// #2684: the unfinished-worker hold follows the same admission-order
    /// supersession as the binding: a later launch of the Issue releases the
    /// halted predecessor's hold whatever became of it, and a successor that
    /// goes `running` then `failed` never takes the hold back.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn any_admitted_successor_launch_releases_the_unfinished_worker_hold() {
        for outcome in [
            "queued",
            "running",
            "uncertain",
            "succeeded",
            "failed",
            "blocked",
            "revoked",
        ] {
            let (store, project, issue, manager, a, b) = binding_fixture();
            store
                .conn
                .execute(
                    "UPDATE issues SET status='InProgress' WHERE id=?1",
                    [issue.to_string()],
                )
                .unwrap();
            store
                .update_session_status(a, SessionStatus::Interrupted)
                .unwrap();
            admit_launch(
                &store,
                project,
                issue,
                manager,
                a,
                Uuid::new_v4(),
                "succeeded",
                "2026-10-06T10:00:00.000000000Z",
            );
            assert!(
                store.unfinished_issue_worker_hold(a).unwrap(),
                "a halted worker with no successor holds ({outcome})"
            );
            let successor = Uuid::new_v4();
            admit_launch(
                &store,
                project,
                issue,
                manager,
                b,
                successor,
                outcome,
                "2026-10-06T11:00:00.000000000Z",
            );
            assert!(
                !store.unfinished_issue_worker_hold(a).unwrap(),
                "a {outcome} successor launch releases the predecessor's hold"
            );
        }

        // running -> failed: the release is not undone by the outcome.
        let (store, project, issue, manager, a, b) = binding_fixture();
        store
            .conn
            .execute(
                "UPDATE issues SET status='InProgress' WHERE id=?1",
                [issue.to_string()],
            )
            .unwrap();
        admit_launch(
            &store,
            project,
            issue,
            manager,
            a,
            Uuid::new_v4(),
            "succeeded",
            "2026-10-06T10:00:00.000000000Z",
        );
        let successor = Uuid::new_v4();
        admit_launch(
            &store,
            project,
            issue,
            manager,
            b,
            successor,
            "running",
            "2026-10-06T11:00:00.000000000Z",
        );
        assert!(!store.unfinished_issue_worker_hold(a).unwrap());
        for state in ["failed", "revoked"] {
            store
                .conn
                .execute(
                    "UPDATE harness_manager_v2_operations SET state=?2 WHERE id=?1",
                    params![successor.to_string(), state],
                )
                .unwrap();
            assert!(
                !store.unfinished_issue_worker_hold(a).unwrap(),
                "a running successor that became {state} does not give the hold back"
            );
        }
    }

    /// #1322: supersession is admission order, not wall clock or id order. A
    /// successor admitted after a backward clock step (its `created_at` and
    /// its id both sort before the predecessor's) keeps append authority and
    /// the resumed predecessor stays refused.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn later_admitted_successor_with_an_earlier_timestamp_holds_the_binding() {
        let (store, project, issue, manager, a, b) = binding_fixture();
        let a_launch = Uuid::parse_str("ffffffff-ffff-4fff-bfff-ffffffffffff").unwrap();
        let b_launch = Uuid::parse_str("00000000-0000-4000-8000-000000000001").unwrap();
        admit_launch(
            &store,
            project,
            issue,
            manager,
            a,
            a_launch,
            "succeeded",
            "2026-10-06T12:00:00.000000000Z",
        );
        admit_launch(
            &store,
            project,
            issue,
            manager,
            b,
            b_launch,
            "running",
            "2026-10-06T11:00:00.000000000Z",
        );
        assert_eq!(
            Store::live_issue_binding_for_worker_on(&store.conn, b).unwrap(),
            Some((project, issue, b_launch)),
            "the later-admitted successor keeps append authority"
        );
        assert_eq!(
            Store::live_issue_binding_for_worker_on(&store.conn, a).unwrap(),
            None,
            "the resumed predecessor is refused"
        );
    }

    /// #1325: a replay finds the last admission even after a backward clock
    /// step. The same key can be admitted in different manager scope versions.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn prior_continuation_operation_uses_later_admission_with_an_earlier_timestamp() {
        let (store, project, issue, manager, predecessor, worker) = binding_fixture();
        for (id, timestamp, scope, query) in [
            (
                "ffffffff-ffff-4fff-bfff-ffffffffffff",
                "2026-10-06T12:00:00.000000000Z",
                1,
                "earlier admission",
            ),
            (
                "00000000-0000-4000-8000-000000000001",
                "2026-10-06T11:00:00.000000000Z",
                2,
                "later admission",
            ),
        ] {
            admit_launch(
                &store,
                project,
                issue,
                manager,
                worker,
                Uuid::parse_str(id).unwrap(),
                "succeeded",
                timestamp,
            );
            let operation = ManagerActionV2::CreateSession {
                parent_id: manager,
                kind: SessionKind::Task,
                query: query.into(),
                launch: rsi_common::harness_manager_v2::ManagerLaunchChoiceV2 {
                    provider: rsi_common::types::SessionProvider::Codex,
                    model: "gpt-5".into(),
                    effort: None,
                },
                sandbox_source: None,
            };
            store
                .conn
                .execute(
                    "UPDATE harness_manager_v2_operations
                 SET scope_version=?2, idempotency_key='continuation-replay',
                     payload_json=json_set(payload_json,
                         '$.issue_binding.continue_from',?3,
                         '$.request.operation',json(?4))
                 WHERE id=?1",
                    params![
                        id,
                        scope,
                        predecessor.to_string(),
                        serde_json::to_string(&operation).unwrap()
                    ],
                )
                .unwrap();
        }
        let operation = store
            .prior_continuation_operation(project, "continuation-replay", predecessor)
            .unwrap()
            .unwrap();
        let ManagerActionV2::CreateSession { query, .. } = operation else {
            panic!("expected the journalled create_session operation");
        };
        assert_eq!(query, "later admission");
    }

    /// #1325: baton notices go to the latest admitted Issue-bound launcher,
    /// even when its timestamp and UUID sort before the previous launch.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn worker_baton_launcher_uses_later_admission_with_an_earlier_timestamp() {
        use crate::store::worker_baton::{WorkerBatonLauncher, WorkerLauncherKind};

        let (store, project, issue, manager, worker, later_manager) = binding_fixture();
        for (id, timestamp, launcher) in [
            (
                "ffffffff-ffff-4fff-bfff-ffffffffffff",
                "2026-10-06T12:00:00.000000000Z",
                manager,
            ),
            (
                "00000000-0000-4000-8000-000000000001",
                "2026-10-06T11:00:00.000000000Z",
                later_manager,
            ),
        ] {
            admit_launch(
                &store,
                project,
                issue,
                launcher,
                worker,
                Uuid::parse_str(id).unwrap(),
                "succeeded",
                timestamp,
            );
            store
                .conn
                .execute(
                    "UPDATE harness_manager_v2_operations
                 SET payload_json=json_set(payload_json,
                     '$.origin.origin','agent','$.origin.caller',?2)
                 WHERE id=?1",
                    params![id, launcher.to_string()],
                )
                .unwrap();
        }
        assert_eq!(
            store.worker_baton_launcher(worker).unwrap(),
            Some(WorkerBatonLauncher {
                launcher: later_manager,
                kind: WorkerLauncherKind::Manager,
                project_id: Some(project),
                issue_id: Some(issue),
                issue_display_number: Some(1),
            }),
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn continuation_brief_quotes_the_latest_handoff_and_fits_the_query_limit() {
        let body = "Build it.\n\n## Handoff 1\nold\n\n## Baton handoff\nnew state: tests red\nPIPELINE HANDOFF — BATON abc";
        assert_eq!(latest_handoff(body), Some("PIPELINE HANDOFF — BATON abc"));
        let body = "Build it.\n\n## Handoff 1\nold\n\n## Baton handoff\nnew state: tests red";
        assert_eq!(
            latest_handoff(body),
            Some("## Baton handoff\nnew state: tests red")
        );
        assert_eq!(latest_handoff("Build it."), None);
        let predecessor = Uuid::new_v4();
        let brief = continuation_brief(
            predecessor,
            "0123456789abcdef0123456789abcdef01234567",
            true,
            "Committed WIP.\nPIPELINE HANDOFF — BATON 0123456",
            latest_handoff(body),
            "Finish the Issue.",
            ContinuationCause::Baton,
        );
        assert!(brief.contains(&predecessor.to_string()));
        assert!(brief.contains("0123456789abcdef0123456789abcdef01234567"));
        assert!(brief.contains("NOT copied"));
        assert!(brief.contains("> PIPELINE HANDOFF — BATON 0123456"));
        assert!(brief.contains("> new state: tests red"));
        assert!(brief.ends_with("Finish the Issue."));
        // Huge copied text never pushes the query past create_session's limit.
        let huge = "é".repeat(40_000);
        let long_brief = "b".repeat(24_000);
        let brief = continuation_brief(
            predecessor,
            "h",
            false,
            &huge,
            Some(&huge),
            &long_brief,
            ContinuationCause::Baton,
        );
        assert!(brief.len() <= CONTINUATION_QUERY_BUDGET, "{}", brief.len());
        assert!(brief.contains("[truncated]"));
    }

    /// #1555: a bound worker whose turn ended with no report and no wake of
    /// its own is stranded; the note names the daemon job and unit it still
    /// owns. A pending own wake, a final report or a missing binding is not.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn stranded_worker_note_names_owned_jobs_and_spares_pending_or_reported_workers() {
        use rsi_common::types::{
            ConversationEvent, EventType, Recurrence, Role, ScheduleSpec, ScheduledJob, WakeMode,
        };
        let (store, project, issue, manager, bound, unbound) = binding_fixture();
        admit_launch(
            &store,
            project,
            issue,
            manager,
            bound,
            Uuid::new_v4(),
            "succeeded",
            "2026-10-01T00:00:00.000000000Z",
        );
        store
            .conn
            .execute(
                "UPDATE sessions SET status='Completed' WHERE id IN (?1,?2)",
                params![bound.to_string(), unbound.to_string()],
            )
            .unwrap();
        let note = |id: Uuid| {
            let session = store.get_session(id).unwrap().unwrap();
            store.worker_stranded_note(&session).unwrap()
        };
        let pending = |id: Uuid| {
            let session = store.get_session(id).unwrap().unwrap();
            store.worker_terminal_watch_pending(&session).unwrap()
        };
        let say = |sequence: i32, role: Role, content: &str| {
            store
                .insert_event(&ConversationEvent {
                    id: 0,
                    session_id: bound,
                    sequence,
                    event_type: EventType::Message,
                    role: Some(role),
                    content: content.into(),
                    tool_name: None,
                    tool_input: None,
                    created_at: chrono::Utc::now(),
                    offload_id: None,
                    tool_use_id: None,
                    metadata: None,
                })
                .unwrap();
        };

        // Bound, nothing reported, nothing owned.
        say(1, Role::Assistant, "waiting on my tests");
        assert_eq!(note(bound).as_deref(), Some("no-result: stranded"));
        assert!(!pending(bound));
        // An unbound worker is never flagged.
        assert_eq!(note(unbound), None);

        // A running daemon job without a wake is named with its unit.
        let job = Uuid::new_v4();
        store
            .conn
            .execute(
                "INSERT INTO agent_jobs(id,owner_session_id,kind,params_json,cwd,unit_name,
                    log_path,status_path,state,created_at,row_version)
                 VALUES(?1,?2,'test','{}','/tmp','rsi-job-unit','/tmp/l','/tmp/s','running',
                    '2026-10-01T00:00:00.000000000Z',1)",
                params![job.to_string(), bound.to_string()],
            )
            .unwrap();
        let named = note(bound).unwrap();
        assert!(
            named.starts_with("no-result: stranded; owns job "),
            "{named}"
        );
        assert!(named.contains(&job.to_string()[..8]) && named.contains("unit rsi-job-unit"));
        assert!(
            !pending(bound),
            "a running unit without a wake still flags stranding"
        );

        // An enabled wake of its own means the report is pending.
        let now = chrono::Utc::now();
        let wake = ScheduledJob {
            id: Uuid::new_v4(),
            name: "job-wake".into(),
            message: "m".into(),
            schedule: ScheduleSpec {
                recurrence: Recurrence::Once,
                anchor: now,
            },
            last_fired_at: None,
            next_fire_at: now,
            enabled: true,
            working_dir: None,
            provider: None,
            model: None,
            project_id: None,
            created_at: now,
            updated_at: now,
            wake_mode: WakeMode::Resume,
            wake_session_id: Some(bound),
        };
        store.insert_scheduled_job(&wake).unwrap();
        assert_eq!(note(bound), None);
        assert!(pending(bound));
        // A daemon predicate wake also holds the watch across interim turns.
        store.conn.execute(
            "UPDATE scheduled_jobs SET wake_mode='fresh', schedule_json=json_set(schedule_json,'$.wake_when',json('{}')) WHERE id=?1",
            [wake.id.to_string()],
        ).unwrap();
        assert!(pending(bound));
        store.conn.execute(
            "UPDATE scheduled_jobs SET wake_mode='resume', schedule_json=json_remove(schedule_json,'$.wake_when') WHERE id=?1",
            [wake.id.to_string()],
        ).unwrap();
        // A watch of the manager's on this worker does not count as its wake.
        store
            .conn
            .execute(
                "UPDATE scheduled_jobs SET wake_mode=?2 WHERE id=?1",
                params![wake.id.to_string(), format!("on_terminal:{bound}")],
            )
            .unwrap();
        assert!(note(bound).is_some());
        assert!(!pending(bound));
        // A refused/retired wake is no longer enabled.
        store
            .conn
            .execute(
                "UPDATE scheduled_jobs SET wake_mode='resume', enabled=0 WHERE id=?1",
                params![wake.id.to_string()],
            )
            .unwrap();
        assert!(note(bound).is_some());

        // A final report in the latest turn is not a stranding.
        say(2, Role::Assistant, "PIPELINE HANDOFF — IMPLEMENTATION:");
        assert_eq!(note(bound), None);
        assert!(
            pending(bound),
            "a reported worker's running job still holds its watch"
        );
        store
            .conn
            .execute(
                "UPDATE agent_jobs SET state='succeeded' WHERE id=?1",
                [job.to_string()],
            )
            .unwrap();
        assert!(
            !pending(bound),
            "settled jobs and disabled wakes release the watch"
        );
        // A later user turn starts a new turn without a report.
        say(3, Role::User, "continue");
        say(4, Role::Assistant, "still working");
        assert!(note(bound).is_some());
        // #1616: a reviewer's closing `REVIEW APPROVE|CHANGES` line is a
        // report, annotated with its verdict instead of "stranded".
        let watch = |id: Uuid| {
            let session = store.get_session(id).unwrap().unwrap();
            store.worker_watch_note(&session).unwrap()
        };
        assert_eq!(watch(bound).as_deref(), Some("no-result: stranded"));
        say(
            5,
            Role::Assistant,
            "verdict follows\nREVIEW APPROVE commit=abc",
        );
        assert_eq!(note(bound), None);
        assert_eq!(watch(bound).as_deref(), Some("review: APPROVE"));
        say(6, Role::User, "again");
        say(7, Role::Assistant, "**REVIEW CHANGES** two blockers");
        assert_eq!(watch(bound).as_deref(), Some("review: CHANGES"));

        // #1791: Issue-bound reviewers' lowercase verdicts release the watch
        // and retain the same canonical annotation as uppercase verdicts.
        for (turn, report, annotation) in [
            (
                8,
                "REVIEW approve 0 findings\nFriction: none",
                "review: APPROVE",
            ),
            (10, "REVIEW changes 2 findings", "review: CHANGES"),
            (12, "**REVIEW Approve**", "review: APPROVE"),
        ] {
            say(turn, Role::User, "review again");
            say(turn + 1, Role::Assistant, report);
            assert!(store.latest_turn_reported(bound).unwrap());
            assert_eq!(watch(bound).as_deref(), Some(annotation));
            assert!(!pending(bound));
        }
        // A handoff-only review remains a final report without a verdict.
        say(14, Role::User, "review again");
        say(
            15,
            Role::Assistant,
            "PIPELINE HANDOFF — REVIEW:\nFriction: none",
        );
        assert!(store.latest_turn_reported(bound).unwrap());
        assert_eq!(watch(bound), None);
        // The reviewer brief's `REQUEST_CHANGES` spelling is a CHANGES verdict.
        say(16, Role::User, "review again");
        say(
            17,
            Role::Assistant,
            "REVIEW REQUEST_CHANGES 2 findings\nFriction: none",
        );
        assert!(store.latest_turn_reported(bound).unwrap());
        assert_eq!(watch(bound).as_deref(), Some("review: CHANGES"));
        assert!(!pending(bound));

        // #2525: a near-miss handoff header is a final report, normalized and
        // annotated; prose with no handoff header is still stranded.
        say(18, Role::User, "continue");
        say(
            19,
            Role::Assistant,
            "all done, tests pass, nothing more to say",
        );
        assert_eq!(watch(bound).as_deref(), Some("no-result: stranded"));
        say(20, Role::User, "report");
        say(
            21,
            Role::Assistant,
            "**IMPLEMENTATION HANDOFF — role: research, budget: 250 tokens**\nFriction: none",
        );
        assert!(store.latest_turn_reported(bound).unwrap());
        assert_eq!(note(bound), None);
        assert_eq!(
            watch(bound).as_deref(),
            Some("handoff_header_normalized: IMPLEMENTATION")
        );
        // The exact header is not a normalization.
        say(22, Role::User, "report again");
        say(
            23,
            Role::Assistant,
            "**IMPLEMENTATION HANDOFF — x**\nPIPELINE HANDOFF — IMPLEMENTATION:",
        );
        assert_eq!(watch(bound), None);
    }

    /// #1935: a stale scheduled wake that fires after a worker's handoff and
    /// gets a short no-op reply must not replace the handoff as the final
    /// report; a session with no handoff, or whose later turn did real work,
    /// is still stranded. A `when` wake armed before the handoff is superseded.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn stale_wake_after_handoff_keeps_the_handoff_as_the_result() {
        use rsi_common::types::{ConversationEvent, EventType, Role};
        let (store, project, issue, manager, bound, _unbound) = binding_fixture();
        admit_launch(
            &store,
            project,
            issue,
            manager,
            bound,
            Uuid::new_v4(),
            "succeeded",
            "2026-10-01T00:00:00.000000000Z",
        );
        store
            .conn
            .execute(
                "UPDATE sessions SET status='Completed', parent_id=?2 WHERE id=?1",
                params![bound.to_string(), manager.to_string()],
            )
            .unwrap();
        let say = |sequence: i32, role: Role, content: &str| {
            store
                .insert_event(&ConversationEvent {
                    id: 0,
                    session_id: bound,
                    sequence,
                    event_type: EventType::Message,
                    role: Some(role),
                    content: content.into(),
                    tool_name: None,
                    tool_input: None,
                    created_at: chrono::Utc::now(),
                    offload_id: None,
                    tool_use_id: None,
                    metadata: None,
                })
                .unwrap();
        };
        let watch = || {
            let session = store.get_session(bound).unwrap().unwrap();
            store.worker_watch_note(&session).unwrap()
        };
        let wake = "<rsid-daemon-message source=\"scheduled-wake\">\n\nbatch done?\n\n[wake_when] jobs_terminal (1 jobs): satisfied\n</rsid-daemon-message>";

        // No handoff anywhere: stranded, however many wakes fire.
        say(1, Role::User, "start");
        say(2, Role::Assistant, "waiting on my tests");
        say(3, Role::User, wake);
        say(4, Role::Assistant, "That wake is stale.");
        assert_eq!(watch().as_deref(), Some("no-result: stranded"));

        // Handoff, then a stale wake answered with a short no-op: still reported.
        say(5, Role::User, "go on");
        say(
            6,
            Role::Assistant,
            "PIPELINE HANDOFF — IMPLEMENTATION:\nFriction: none",
        );
        let armed_before = chrono::Utc::now() - chrono::Duration::hours(1);
        let armed_after = chrono::Utc::now() + chrono::Duration::hours(1);
        assert!(
            store
                .wake_superseded_by_handoff(bound, armed_before, Some(armed_before))
                .unwrap(),
            "a wake armed and settled before the handoff is superseded"
        );
        assert!(
            !store
                .wake_superseded_by_handoff(bound, armed_before, Some(armed_after))
                .unwrap(),
            "a wake whose predicate settled after the handoff is live (#2054)"
        );
        assert!(
            !store
                .wake_superseded_by_handoff(bound, armed_before, None)
                .unwrap(),
            "a wake with an unknown settle time is live"
        );
        assert!(
            !store
                .wake_superseded_by_handoff(bound, armed_after, Some(armed_after))
                .unwrap(),
            "a wake armed after the handoff is live"
        );
        say(7, Role::User, wake);
        say(8, Role::Assistant, "That wake is stale; the work is done.");
        assert!(store.latest_turn_reported(bound).unwrap());
        assert_eq!(watch(), None);
        // A second stale wake chains back to the same handoff.
        say(9, Role::User, wake);
        say(10, Role::Assistant, "Stale again.");
        assert!(store.latest_turn_reported(bound).unwrap());

        // A real follow-up (manager message) that ends with no report is stranded.
        say(11, Role::User, "please rework the tests");
        say(12, Role::Assistant, "reworking");
        assert_eq!(watch().as_deref(), Some("no-result: stranded"));

        // A long no-op-looking reply to a wake is real work: stranded.
        say(13, Role::User, wake);
        say(14, Role::Assistant, &"x".repeat(4000));
        assert_eq!(watch().as_deref(), Some("no-result: stranded"));

        // A reviewer's verdict also survives a stale wake.
        say(15, Role::User, "review");
        say(16, Role::Assistant, "REVIEW APPROVE commit=abc");
        say(17, Role::User, wake);
        say(18, Role::Assistant, "stale");
        assert_eq!(watch().as_deref(), Some("review: APPROVE"));
    }

    /// #187: a Completed worker with no marker in its final turn is "stranded"
    /// only while its bound Issue holds no handoff of its own. A handoff
    /// record (a held, untested or finished one) changes the label to the
    /// non-verdict `no-marker` note; another session's handoff on the Issue does
    /// not, and a marker line still clears the note entirely.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn issue_handoff_without_marker_is_not_called_stranded() {
        use rsi_common::types::{ConversationEvent, EventType, Role};
        let (store, project, issue, manager, bound, other) = binding_fixture();
        admit_launch(
            &store,
            project,
            issue,
            manager,
            bound,
            Uuid::new_v4(),
            "succeeded",
            "2026-10-01T00:00:00.000000000Z",
        );
        store
            .conn
            .execute(
                "UPDATE sessions SET status='Completed' WHERE id=?1",
                params![bound.to_string()],
            )
            .unwrap();
        let say = |sequence: i32, content: &str| {
            store
                .insert_event(&ConversationEvent {
                    id: 0,
                    session_id: bound,
                    sequence,
                    event_type: EventType::Message,
                    role: Some(Role::Assistant),
                    content: content.into(),
                    tool_name: None,
                    tool_input: None,
                    created_at: chrono::Utc::now(),
                    offload_id: None,
                    tool_use_id: None,
                    metadata: None,
                })
                .unwrap();
        };
        let handoff_by = |author: Uuid, sequence: i64| {
            store
                .conn
                .execute(
                    "INSERT INTO issue_handoffs
                         (issue_id, sequence, project_id, author_session_id, idempotency_key,
                          body, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, 'capacity hold: UNTESTED',
                             '2026-10-01T00:00:00.000000000Z')",
                    params![
                        issue.to_string(),
                        sequence,
                        project.to_string(),
                        author.to_string(),
                        format!("k{sequence}")
                    ],
                )
                .unwrap();
        };
        let watch = || {
            let session = store.get_session(bound).unwrap().unwrap();
            store.worker_watch_note(&session).unwrap()
        };

        // Missing report, no handoff anywhere: stranded.
        say(1, "waiting on capacity");
        assert_eq!(watch().as_deref(), Some("no-result: stranded"));

        // A handoff by some other session on the same Issue is not this worker's.
        handoff_by(other, 1);
        assert_eq!(watch().as_deref(), Some("no-result: stranded"));

        // The worker's own handoff: distinct, never a verdict.
        handoff_by(bound, 2);
        assert_eq!(
            watch().as_deref(),
            Some(crate::store::worker_no_result::NO_MARKER_ISSUE_HANDOFF_NOTE)
        );

        // A marker line in the final turn is a report: no note at all.
        say(2, "PIPELINE HANDOFF — IMPLEMENTATION:\nFriction: none");
        assert_eq!(watch(), None);
    }

    /// #1553: a queued launch journalled under an older scope version of its
    /// project can never run, so it holds no Issue binding; one journalled
    /// under the current scope version still does.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn a_queued_launch_of_a_superseded_scope_holds_no_issue_binding() {
        let (store, project, issue, manager, worker, _) = binding_fixture();
        let queued = Uuid::new_v4();
        admit_launch(
            &store,
            project,
            issue,
            manager,
            worker,
            queued,
            "queued",
            "2026-10-07T12:00:00.000000000Z",
        );
        let relaunch = Uuid::new_v4();
        let held = |store: &Store| {
            store
                .manager_issue_other_live_worker(issue, relaunch, None)
                .unwrap()
                .is_some()
        };
        // The launch's scope version is 1; the project's scope is still at 1.
        store
            .conn
            .execute(
                "INSERT INTO harness_manager_scopes(project_id,manager_session_id,epic_ids_json,
                    row_version,updated_at,scope_mode,group_ids_json)
                 VALUES(?1,?2,'[]',1,'2026-10-07T12:00:00.000000000Z','project','[]')",
                params![project.to_string(), manager.to_string()],
            )
            .unwrap();
        assert!(held(&store), "a launch of the current scope can still run");
        // An appointment advances the scope: the queued launch is dead.
        store
            .conn
            .execute(
                "UPDATE harness_manager_scopes SET row_version=2 WHERE project_id=?1",
                [project.to_string()],
            )
            .unwrap();
        assert!(!held(&store), "a superseded queued launch binds nothing");
        // A running launch of the old scope is already past the gate: it stays.
        store
            .conn
            .execute(
                "UPDATE harness_manager_v2_operations SET state='running' WHERE id=?1",
                [queued.to_string()],
            )
            .unwrap();
        assert!(held(&store));
    }
    /// #2212: a bound worker whose turn ended (Completed) still holds the Issue
    /// while it is parked on a wake or a job of its own and has not handed
    /// off. The refusal names the worker and what it waits on; its final
    /// handoff, a settled wake or job, a later launch or an explicit
    /// `continue_from` of that worker free the Issue.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn a_parked_worker_holds_the_issue_until_it_hands_off_or_settles() {
        use rsi_common::types::{
            ConversationEvent, EventType, Recurrence, Role, ScheduleSpec, ScheduledJob, WakeMode,
        };
        let (store, project, issue, manager, parked, other) = binding_fixture();
        admit_launch(
            &store,
            project,
            issue,
            manager,
            parked,
            Uuid::new_v4(),
            "succeeded",
            "2026-10-13T16:50:00.000000000Z",
        );
        let relaunch = Uuid::new_v4();
        let holder = |continue_from: Option<Uuid>| {
            store
                .manager_issue_other_live_worker(issue, relaunch, continue_from)
                .unwrap()
        };
        // A Running worker holds the Issue and is named.
        let running = holder(None).unwrap();
        assert!(running.contains(&parked.to_string()), "{running}");
        // Its turn ended with nothing of its own pending: the Issue is free.
        store
            .conn
            .execute(
                "UPDATE sessions SET status='Completed' WHERE id=?1",
                [parked.to_string()],
            )
            .unwrap();
        assert_eq!(holder(None), None, "a finished worker holds nothing");

        // Parked on a `when` wake that waits for its job: live, named.
        let now = chrono::Utc::now();
        let wake = ScheduledJob {
            id: Uuid::new_v4(),
            name: "job-wake".into(),
            message: "m".into(),
            schedule: ScheduleSpec {
                recurrence: Recurrence::Once,
                anchor: now,
            },
            last_fired_at: None,
            next_fire_at: now,
            enabled: true,
            working_dir: None,
            provider: None,
            model: None,
            project_id: None,
            created_at: now,
            updated_at: now,
            wake_mode: WakeMode::Resume,
            wake_session_id: Some(parked),
        };
        store.insert_scheduled_job(&wake).unwrap();
        store
            .conn
            .execute(
                "UPDATE scheduled_jobs SET schedule_json=json_set(schedule_json,'$.wake_when',
                    json('{\"predicate\":{\"jobs_terminal\":[]}}')) WHERE id=?1",
                [wake.id.to_string()],
            )
            .unwrap();
        let waiting = holder(None).unwrap();
        assert!(
            waiting.contains(&parked.to_string()) && waiting.contains(&wake.id.to_string()),
            "{waiting}"
        );
        // An explicit continue_from of that worker replaces it; another does not.
        assert_eq!(holder(Some(parked)), None);
        assert!(holder(Some(other)).is_some());

        // The wake is gone but a job it owns is still running: still live.
        store
            .conn
            .execute(
                "UPDATE scheduled_jobs SET enabled=0 WHERE id=?1",
                [wake.id.to_string()],
            )
            .unwrap();
        assert_eq!(holder(None), None);
        let job = Uuid::new_v4();
        store
            .conn
            .execute(
                "INSERT INTO agent_jobs(id,owner_session_id,kind,params_json,cwd,unit_name,
                    log_path,status_path,state,created_at,row_version)
                 VALUES(?1,?2,'test','{}','/tmp','rsi-job-unit','/tmp/l','/tmp/s','running',
                    '2026-10-13T16:51:00.000000000Z',1)",
                params![job.to_string(), parked.to_string()],
            )
            .unwrap();
        let running_job = holder(None).unwrap();
        assert!(running_job.contains(&job.to_string()), "{running_job}");
        // The job settles: nothing is pending.
        store
            .conn
            .execute(
                "UPDATE agent_jobs SET state='succeeded' WHERE id=?1",
                [job.to_string()],
            )
            .unwrap();
        assert_eq!(holder(None), None);

        // A stale wake that stays armed after the final handoff holds nothing.
        store
            .conn
            .execute(
                "UPDATE scheduled_jobs SET enabled=1 WHERE id=?1",
                [wake.id.to_string()],
            )
            .unwrap();
        assert!(holder(None).is_some());
        store
            .insert_event(&ConversationEvent {
                id: 0,
                session_id: parked,
                sequence: 1,
                event_type: EventType::Message,
                role: Some(Role::Assistant),
                content: "PIPELINE HANDOFF — IMPLEMENTATION:\nFriction: none".into(),
                tool_name: None,
                tool_input: None,
                created_at: chrono::Utc::now(),
                offload_id: None,
                tool_use_id: None,
                metadata: None,
            })
            .unwrap();
        assert_eq!(holder(None), None, "the final handoff frees the Issue");
    }

    /// #2212: only the Issue's latest launch can hold it parked; a later
    /// admitted launch (a `continue_from` successor, whatever became of it)
    /// superseded the parked worker, as for its append authority (#1321).
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn a_later_launch_supersedes_a_parked_worker_for_the_liveness_check() {
        let (store, project, issue, manager, parked, successor) = binding_fixture();
        admit_launch(
            &store,
            project,
            issue,
            manager,
            parked,
            Uuid::new_v4(),
            "succeeded",
            "2026-10-13T16:50:00.000000000Z",
        );
        store
            .conn
            .execute(
                "UPDATE sessions SET status='Completed' WHERE id=?1",
                [parked.to_string()],
            )
            .unwrap();
        let job = Uuid::new_v4();
        store
            .conn
            .execute(
                "INSERT INTO agent_jobs(id,owner_session_id,kind,params_json,cwd,unit_name,
                    log_path,status_path,state,created_at,row_version)
                 VALUES(?1,?2,'test','{}','/tmp','rsi-job-unit','/tmp/l','/tmp/s','running',
                    '2026-10-13T16:51:00.000000000Z',1)",
                params![job.to_string(), parked.to_string()],
            )
            .unwrap();
        let relaunch = Uuid::new_v4();
        let holder = || {
            store
                .manager_issue_other_live_worker(issue, relaunch, None)
                .unwrap()
        };
        assert!(holder().is_some());
        admit_launch(
            &store,
            project,
            issue,
            manager,
            successor,
            Uuid::new_v4(),
            "failed",
            "2026-10-13T17:00:00.000000000Z",
        );
        assert_eq!(holder(), None);
    }
}
