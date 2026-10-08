//! #1641 S1b: a topology review node's requests on the owning manager's
//! ledger.
//!
//! The durable executor is the actor. It creates (or revisits) one Work
//! record per review node, `topology:<execution>:<node>`, reserves a DB-native
//! review assignment for the commit under review through the same checks a
//! manager's `RequestReview` passes (`manager_review_reserve_authorized`), and
//! later reads the verdict back from the receipts. Nothing here calls an agent
//! and no verdict is cached: `topology_review_outcome` re-derives it from the
//! assignment and receipt rows every time.

use super::*;
use crate::store::manager_ledger::LedgerObservation;

/// Settlement code when no live project-manager ledger can own the review.
pub const REVIEW_NO_MANAGER_LEDGER: &str = "review_no_manager_ledger";

/// Ledger event kind of an on-call manager's acceptance of an exhausted review
/// (#1740). Keyed by the accepted assignment; the payload names the exact
/// commit, so it admits that commit only.
pub const TOPOLOGY_ONCALL_ACCEPTANCE: &str = "topology_oncall_acceptance";

/// The ruling an on-call acceptance records: which execution, review attempt
/// and decision produced it (audit only; the admission keys are the assignment
/// and the commit).
#[derive(Debug, Clone)]
pub struct TopologyOncallAcceptance {
    pub execution_id: Uuid,
    pub attempt_id: Uuid,
    pub node_id: String,
    pub decision_key: String,
}

/// Longest chain of supersessions followed from one assignment.
const MAX_SUPERSESSION_HOPS: usize = 16;

/// One topology review request.
#[derive(Debug, Clone)]
pub struct TopologyReviewRequest {
    pub execution_id: Uuid,
    pub attempt_id: Uuid,
    pub node_id: String,
    pub execution_name: String,
    pub project_id: Uuid,
    pub epic_id: Uuid,
    /// The session that authored the first reviewed commit: the review's
    /// identity (self-review refusal, custody holder for the reviewer fork).
    pub author_session_id: Uuid,
    /// The commit under review, pinned by the executor in the author's
    /// repository.
    pub source_commit: String,
    pub reviewer: ManagerLaunchChoiceV2,
    pub query: String,
    /// The previous round's assignment when this round re-reviews a fix.
    pub previous_assignment: Option<Uuid>,
    /// Sessions that authored later fix commits; their families bar the
    /// reviewer like the author's.
    pub extra_contributors: Vec<Uuid>,
}

/// What the review service reports for one assignment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TopologyReviewOutcome {
    /// The reviewer has not finished.
    Pending,
    /// A receipt accepted exactly `reviewed_commit`.
    Accepted {
        reviewed_commit: String,
        findings: Vec<String>,
    },
    /// The reviewer asked for changes.
    ChangesRequested { findings: Vec<String> },
    /// No usable verdict will arrive; a short machine reason.
    Unsettled(String),
}

/// What one more review round of a topology review node may be (#1715). The
/// store's round policy is unchanged: the third counted round must use a model
/// none of the earlier rounds used (the closure specialist), and a fourth is
/// never opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TopologyExtraRound {
    /// The node's own reviewer may run the round.
    SameReviewer,
    /// The round must be the closure specialist: this launch is allowed by the
    /// manager's policy, uses a model no counted round used, and its family is
    /// not barred by the reviewed range's contributors.
    Closure(ManagerLaunchChoiceV2),
    /// No policy-valid round exists; a short machine reason.
    Unavailable(String),
}

/// The question "may this review node run one more round?".
#[derive(Debug, Clone)]
pub struct TopologyExtraRoundQuery<'a> {
    pub execution_id: Uuid,
    pub node_id: &'a str,
    pub project_id: Uuid,
    /// The commit the extra round would review.
    pub source_commit: &'a str,
    /// The node's configured reviewer.
    pub reviewer: &'a ManagerLaunchChoiceV2,
    pub author_session_id: Uuid,
    pub extra_contributors: &'a [Uuid],
}

/// The Work key of one review node.
pub fn topology_review_work_key(execution_id: Uuid, node_id: &str) -> String {
    format!("topology:{execution_id}:{node_id}")
}

impl Store {
    /// The current assignment already opened for `attempt_id`, if any.
    fn topology_review_assignment_for_attempt(&self, attempt_id: Uuid) -> Result<Option<Uuid>> {
        let found: Option<String> = self
            .conn
            .query_row(
                "SELECT assignment_id FROM manager_review_assignments
                  WHERE json_extract(request_json,'$.topology_attempt_id')=?1
                    AND superseded_by_assignment_id IS NULL
                  ORDER BY created_at DESC,assignment_id DESC LIMIT 1",
                [attempt_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        found.map(parse_uuid).transpose()
    }

    /// Follow supersession (infra relaunch, re-request) to the current
    /// assignment.
    fn topology_review_head(&self, assignment_id: Uuid) -> Result<Uuid> {
        let mut at = assignment_id;
        for _ in 0..MAX_SUPERSESSION_HOPS {
            let next: Option<Option<String>> = self
                .conn
                .query_row(
                    "SELECT superseded_by_assignment_id FROM manager_review_assignments
                      WHERE assignment_id=?1",
                    [at.to_string()],
                    |row| row.get(0),
                )
                .optional()?;
            match next {
                None => return Err(refused("manager_review_assignment_missing")),
                Some(None) => return Ok(at),
                Some(Some(next)) => at = parse_uuid(next)?,
            }
        }
        Err(refused("manager_review_supersession_unbounded"))
    }

    /// The finding keys a delta round must resolve: the previous receipt's
    /// blocking findings (all of them when none blocks).
    fn topology_review_finding_keys(&self, assignment_id: Uuid) -> Result<Vec<String>> {
        let mut statement = self.conn.prepare(
            "SELECT f.finding_key,f.blocking FROM manager_review_findings f
               JOIN manager_review_receipts r ON r.receipt_id=f.receipt_id
              WHERE r.assignment_id=?1
              ORDER BY f.blocking DESC,f.finding_key LIMIT ?2",
        )?;
        let rows = statement
            .query_map(
                params![
                    assignment_id.to_string(),
                    MANAGER_REVIEW_MAX_FINDINGS as i64
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?)),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let blocking: Vec<String> = rows
            .iter()
            .filter(|(_, blocking)| *blocking)
            .map(|(key, _)| key.clone())
            .collect();
        Ok(if blocking.is_empty() {
            rows.into_iter().map(|(key, _)| key).collect()
        } else {
            blocking
        })
    }

    /// Reserve a DB-native review for a topology review node on the project
    /// manager's ledger and return the assignment id. Idempotent per
    /// `request.attempt_id`: a crash between this call and the attempt's
    /// record never opens a second assignment. The reviewer's launch is the
    /// reserved assignment's allocation (`allocate_manager_review_assignment`).
    ///
    /// Refused with [`REVIEW_NO_MANAGER_LEDGER`] when the project has no live
    /// manager ledger to own the Work.
    pub fn manager_review_reserve_for_topology(
        &self,
        request: &TopologyReviewRequest,
    ) -> Result<Uuid> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if let Some(existing) = self.topology_review_assignment_for_attempt(request.attempt_id)? {
            tx.commit()?;
            return Ok(existing);
        }
        let no_ledger = || refused(REVIEW_NO_MANAGER_LEDGER);
        let config = self
            .get_harness_manager(request.project_id)?
            .ok_or_else(no_ledger)?;
        let manager = config.current_session_id.ok_or_else(no_ledger)?;
        let policy = self
            .get_harness_manager_policy(request.project_id)?
            .filter(|policy| {
                !policy.revoked
                    && policy.manager_session_id == config.manager_session_id
                    && policy.scope_version == config.row_version
            })
            .ok_or_else(no_ledger)?;
        let fence = ManagerFenceV2 {
            scope_version: config.row_version,
            policy_version: policy.row_version,
        };
        self.manager_v2_authorize(manager, &fence, Some(ManagerCapabilityV2::WorkPlan))?;
        if !config.epic_ids.contains(&request.epic_id) {
            return Err(refused("manager_v2_epic_out_of_scope"));
        }
        if self
            .manager_v2_descendant_epic(&config, request.author_session_id)
            .map_err(|_| refused("manager_review_author_out_of_scope"))?
            != request.epic_id
        {
            return Err(refused("manager_review_author_out_of_scope"));
        }
        if !canonical_sha(&request.source_commit) {
            return Err(refused("manager_review_source_changed"));
        }
        let key = topology_review_work_key(request.execution_id, &request.node_id);
        let actor = json!({
            "kind": "topology_executor",
            "execution_id": request.execution_id,
            "attempt_id": request.attempt_id,
            "node_id": request.node_id,
        });
        let (work, work_row_version) = match self.manager_v2_record(&config, "work", &key)? {
            Some(record) => (decode::<WorkRecord>(&record)?, record.row_version),
            None => {
                if self.manager_v2_records(&config, "work")?.len() >= MANAGER_V2_MAX_WORK {
                    return Err(refused("manager_v2_work_limit"));
                }
                let work = WorkRecord {
                    key: key.clone(),
                    epic_id: request.epic_id,
                    title: format!(
                        "topology {} review {}",
                        request.execution_name, request.node_id
                    ),
                    kind: ManagerWorkKindV2::Program,
                    priority: 5,
                    weight: 1,
                    required_gates: vec![
                        ManagerWorkStageV2::Implementation,
                        ManagerWorkStageV2::Review,
                    ],
                    risk_tier: ManagerWorkRiskTierV2::Tier1,
                    spec_revision: 1,
                    source_session_id: Some(request.author_session_id),
                    source_commit: Some(request.source_commit.clone()),
                    stages: super::super::manager_ledger::STAGES
                        .into_iter()
                        .map(|stage| super::super::manager_ledger::StageRecord {
                            stage,
                            state: ManagerStageStateV2::Unknown,
                            note: String::new(),
                            evidence: None,
                            admission: None,
                            updated_at: now(),
                        })
                        .collect(),
                    acceptance: None,
                    integration: None,
                    pending_acceptance: None,
                };
                let value = serde_json::to_value(&work)?;
                let row = self.manager_v2_put_record(
                    &config,
                    "work",
                    &key,
                    Some(request.epic_id),
                    0,
                    &value,
                )?;
                self.manager_v2_event(&config, None, "work", &key, row.row_version, &value)?;
                self.manager_v2_event(
                    &config,
                    None,
                    "topology_review_work",
                    &key,
                    row.row_version,
                    &json!({"actor": actor, "work_key": key, "epic_id": request.epic_id,
                        "author_session_id": request.author_session_id,
                        "source_commit": request.source_commit}),
                )?;
                (work, row.row_version)
            }
        };
        if work.source_session_id != Some(request.author_session_id) {
            return Err(refused("manager_review_work_changed"));
        }
        let (delta_of, finding_keys) = match request.previous_assignment {
            Some(previous) => {
                let head = self.topology_review_head(previous)?;
                let keys = self.topology_review_finding_keys(head)?;
                if keys.is_empty() {
                    (None, Vec::new())
                } else {
                    (Some(head), keys)
                }
            }
            None => (None, Vec::new()),
        };
        // The executor pinned this exact commit in the repository the
        // reviewer forks from (`topology::custody::pin_commit`); the daemon
        // effect proved the pin before calling. No author-custody git proof
        // applies to a fix round's commit, which another node session made.
        let observed = LedgerObservation {
            source_commit: Some(request.source_commit.clone()),
            ..LedgerObservation::default()
        };
        // A fix author's whole custody cohort could have touched the revised
        // bytes, so each cohort member bars its family like the author's.
        let mut fix_authors = BTreeSet::new();
        for session in &request.extra_contributors {
            fix_authors.extend(self.review_lineage(*session)?);
        }
        let receipt = self.manager_review_reserve_authorized(&ReviewReservation {
            caller: manager,
            config: &config,
            fence: &fence,
            work: &work,
            work_row_version,
            observed: &observed,
            expected_row_version: work_row_version,
            source_commit: &request.source_commit,
            query: &request.query,
            launch: &request.reviewer,
            delta_of,
            finding_keys: &finding_keys,
            topology: Some(&TopologyReservation {
                actor,
                attempt_id: request.attempt_id,
                contributors: fix_authors.into_iter().collect(),
            }),
        })?;
        tx.commit()?;
        parse_uuid(receipt.key)
    }

    /// Whether one more review round of this node is policy-valid and, when
    /// the closure-specialist rule applies, which launch it must use. Derived
    /// from the same rows and rules `manager_review_reserve_authorized` applies
    /// (round budget, closure specialist, contributor families), so a round
    /// offered here is not refused later for those reasons (#1715).
    pub fn topology_review_extra_round(
        &self,
        query: &TopologyExtraRoundQuery<'_>,
    ) -> Result<TopologyExtraRound> {
        let unavailable = |reason: &str| Ok(TopologyExtraRound::Unavailable(reason.to_owned()));
        let Some(config) = self.get_harness_manager(query.project_id)? else {
            return unavailable(REVIEW_NO_MANAGER_LEDGER);
        };
        let key = topology_review_work_key(query.execution_id, query.node_id);
        let Some(record) = self.manager_v2_record(&config, "work", &key)? else {
            return unavailable("manager_review_work_missing");
        };
        let work = decode::<WorkRecord>(&record)?;
        let rows = self.review_attempt_rows(
            config.project_id,
            work.epic_id,
            &work.key,
            work.spec_revision,
        )?;
        // The extra round re-reviews the same commit, so it supersedes the
        // current row for that commit, exactly as the reservation computes.
        let prior: Option<String> = self
            .conn
            .query_row(
                "SELECT assignment_id FROM manager_review_assignments
                  WHERE project_id=?1 AND epic_id=?2 AND work_key=?3
                    AND spec_revision=?4 AND source_sha=?5
                    AND superseded_by_assignment_id IS NULL",
                params![
                    config.project_id.to_string(),
                    work.epic_id.to_string(),
                    work.key,
                    work.spec_revision,
                    query.source_commit
                ],
                |row| row.get(0),
            )
            .optional()?;
        let prior = prior.map(parse_uuid).transpose()?;
        let counted = counted_review_attempts(&rows, prior);
        if counted.len() >= MAX_REVIEW_ROUNDS_PER_REVISION {
            return unavailable("manager_review_round_budget");
        }
        let used: BTreeSet<&str> = counted
            .iter()
            .flatten()
            .map(|row| row.request.launch.model.as_str())
            .collect();
        if counted.len() + 1 < MAX_REVIEW_ROUNDS_PER_REVISION
            || !used.contains(query.reviewer.model.as_str())
        {
            return Ok(TopologyExtraRound::SameReviewer);
        }
        // The closing round: another model, allowed by the manager's policy,
        // whose family the reviewed range's authors do not bar.
        let Some(grant) = self.get_harness_manager_policy(query.project_id)? else {
            return unavailable(REVIEW_NO_MANAGER_LEDGER);
        };
        let mut fix_authors = BTreeSet::new();
        for session in query.extra_contributors {
            fix_authors.extend(self.review_lineage(*session)?);
        }
        let fix_authors: Vec<Uuid> = fix_authors.into_iter().collect();
        let contributors = self.review_contributors(
            query.project_id,
            query.author_session_id,
            &BTreeSet::new(),
            &fix_authors,
            &[],
            None,
        )?;
        let override_key =
            self.review_family_override(&config, &work.key, work.spec_revision, None)?;
        for choice in &grant.policy.allowed_launches {
            if used.contains(choice.model.as_str()) {
                continue;
            }
            let family = review_model_family(choice.provider, Some(choice.model.as_str()));
            if self
                .require_review_contributor_family(
                    query.project_id,
                    &work.key,
                    work.spec_revision,
                    &contributors,
                    override_key.as_deref(),
                    family,
                )
                .is_ok()
            {
                return Ok(TopologyExtraRound::Closure(choice.clone()));
            }
        }
        unavailable("manager_review_closure_specialist_required")
    }

    /// The current verdict of one topology review assignment, re-derived from
    /// the assignment and receipt rows (never cached).
    pub fn topology_review_outcome(&self, assignment_id: Uuid) -> Result<TopologyReviewOutcome> {
        let head = self.topology_review_head(assignment_id)?;
        let assignment = self.manager_review_assignment(head)?;
        match assignment.state.as_str() {
            "reserved" | "allocating" | "active" => Ok(TopologyReviewOutcome::Pending),
            "failed" => {
                let code: Option<String> = self.conn.query_row(
                    "SELECT failure_code FROM manager_review_assignments WHERE assignment_id=?1",
                    [head.to_string()],
                    |row| row.get(0),
                )?;
                Ok(TopologyReviewOutcome::Unsettled(
                    code.unwrap_or_else(|| "manager_review_failed".into()),
                ))
            }
            "submitted" => self.topology_review_receipt_outcome(&assignment),
            other => Ok(TopologyReviewOutcome::Unsettled(format!(
                "manager_review_{other}"
            ))),
        }
    }

    /// Record, as one immutable review-ledger event, that the owning manager's
    /// on-call ruling accepted the exhausted review `assignment_id` for exactly
    /// `commit` (#1740). It admits only that assignment's own commit, only
    /// while the assignment is still the current head under the manager scope
    /// it was reserved in, and only for a review that asked for changes. A
    /// reject ruling records nothing. Idempotent.
    pub fn topology_record_oncall_acceptance(
        &self,
        assignment_id: Uuid,
        commit: &str,
        ruling: &TopologyOncallAcceptance,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let head = self.topology_review_head(assignment_id)?;
        let assignment = self.manager_review_assignment(head)?;
        if assignment.source_sha != commit {
            return Err(refused("topology_oncall_acceptance_commit_mismatch"));
        }
        let expected_key = topology_review_work_key(ruling.execution_id, &ruling.node_id);
        if assignment.work_key != expected_key {
            return Err(refused("topology_oncall_acceptance_work_mismatch"));
        }
        let config = self
            .get_harness_manager(assignment.project_id)?
            .filter(|config| {
                config.manager_session_id == assignment.manager_session_id
                    && config.row_version == assignment.scope_version
            })
            .ok_or_else(|| refused("topology_oncall_acceptance_scope_changed"))?;
        match self.topology_review_outcome(head)? {
            TopologyReviewOutcome::ChangesRequested { .. } => {}
            TopologyReviewOutcome::Accepted { .. } => {
                // Already admitted (a prior tick recorded it, or a receipt did).
                tx.commit()?;
                return Ok(());
            }
            _ => return Err(refused("topology_oncall_acceptance_not_exhausted")),
        }
        self.manager_v2_event(
            &config,
            None,
            TOPOLOGY_ONCALL_ACCEPTANCE,
            &head.to_string(),
            0,
            &json!({
                "actor": {
                    "kind": "topology_executor",
                    "execution_id": ruling.execution_id,
                    "attempt_id": ruling.attempt_id,
                    "node_id": ruling.node_id,
                },
                "ruling": "accept",
                "decision_key": ruling.decision_key,
                "assignment_id": head,
                "work_key": assignment.work_key,
                "source_commit": assignment.source_sha,
            }),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Whether an on-call acceptance event admits exactly this assignment's
    /// commit under the manager scope the assignment was reserved in.
    fn topology_oncall_accepted(&self, assignment: &ReviewAssignment) -> Result<bool> {
        let in_scope = self
            .get_harness_manager(assignment.project_id)?
            .is_some_and(|config| {
                config.manager_session_id == assignment.manager_session_id
                    && config.row_version == assignment.scope_version
            });
        if !in_scope {
            return Ok(false);
        }
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM harness_manager_v2_events
                  WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3
                    AND kind=?4 AND record_key=?5
                    AND json_extract(payload_json,'$.source_commit')=?6 LIMIT 1",
                params![
                    assignment.project_id.to_string(),
                    assignment.manager_session_id.to_string(),
                    assignment.scope_version,
                    TOPOLOGY_ONCALL_ACCEPTANCE,
                    assignment.assignment_id.to_string(),
                    assignment.source_sha
                ],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Whether `assignment_id` (the review attempt's recorded assignment,
    /// followed to its head) is still an admitted acceptance of exactly
    /// `commit` (#1641 S2). A land node re-derives this from rows right
    /// before it enqueues, so a changed manager scope, a revoked receipt or a
    /// different reviewed commit stops the landing.
    pub fn topology_land_admitted(&self, assignment_id: Uuid, commit: &str) -> Result<bool> {
        Ok(matches!(
            self.topology_review_outcome(assignment_id)?,
            TopologyReviewOutcome::Accepted { reviewed_commit, .. } if reviewed_commit == commit
        ))
    }

    fn topology_review_receipt_outcome(
        &self,
        assignment: &ReviewAssignment,
    ) -> Result<TopologyReviewOutcome> {
        let receipt: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT receipt_id,verdict FROM manager_review_receipts
                  WHERE assignment_id=?1 AND source_sha=?2",
                params![assignment.assignment_id.to_string(), assignment.source_sha],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((receipt_id, verdict)) = receipt else {
            return Ok(TopologyReviewOutcome::Unsettled(
                "manager_review_receipt_missing".into(),
            ));
        };
        let mut statement = self.conn.prepare(
            "SELECT finding_key,severity,summary,location,blocking
               FROM manager_review_findings WHERE receipt_id=?1
              ORDER BY blocking DESC,finding_key",
        )?;
        let mut blocking = false;
        let findings = statement
            .query_map([receipt_id], |row| {
                let key: String = row.get(0)?;
                let severity: String = row.get(1)?;
                let summary: String = row.get(2)?;
                let location: Option<String> = row.get(3)?;
                let is_blocking: bool = row.get(4)?;
                let text = match location {
                    Some(location) => format!("{key} [{severity}] {summary} ({location})"),
                    None => format!("{key} [{severity}] {summary}"),
                };
                Ok((text, is_blocking))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .map(|(text, is_blocking)| {
                blocking |= is_blocking;
                text
            })
            .collect::<Vec<_>>();
        // An "accepted" receipt that still carries a blocking finding is not an
        // acceptance (`manager_v2_accepted_source` refuses it too); delta
        // eligibility reads the same effective verdict.
        match effective_review_verdict(&verdict, blocking) {
            "accepted" => {
                let admitted = self
                    .get_harness_manager(assignment.project_id)?
                    .filter(|config| {
                        config.manager_session_id == assignment.manager_session_id
                            && config.row_version == assignment.scope_version
                    })
                    .map(|config| -> Result<bool> {
                        let (_, work) = self.manager_v2_work(&config, &assignment.work_key)?;
                        Ok(self
                            .manager_v2_accepted_source(&config, &work, &assignment.source_sha)?
                            .is_some())
                    })
                    .transpose()?;
                if admitted == Some(true) {
                    return Ok(TopologyReviewOutcome::Accepted {
                        reviewed_commit: assignment.source_sha.clone(),
                        findings,
                    });
                }
                // The receipt lands before the reviewer's invocation
                // completes, and admission needs both: wait while the
                // reviewer can still finish, give up once it cannot.
                let reviewer_gone = match assignment.reviewer_session_id {
                    Some(reviewer) => self.get_session(reviewer)?.is_none_or(|session| {
                        matches!(
                            session.status,
                            SessionStatus::Failed
                                | SessionStatus::Interrupted
                                | SessionStatus::Archived
                                | SessionStatus::Deleted
                        )
                    }),
                    None => true,
                };
                Ok(if reviewer_gone || admitted.is_none() {
                    TopologyReviewOutcome::Unsettled(
                        "manager_review_acceptance_not_admitted".into(),
                    )
                } else {
                    TopologyReviewOutcome::Pending
                })
            }
            "changes_requested" if self.topology_oncall_accepted(assignment)? => {
                Ok(TopologyReviewOutcome::Accepted {
                    reviewed_commit: assignment.source_sha.clone(),
                    findings,
                })
            }
            "changes_requested" => Ok(TopologyReviewOutcome::ChangesRequested { findings }),
            other => Ok(TopologyReviewOutcome::Unsettled(format!(
                "manager_review_{other}"
            ))),
        }
    }
}
