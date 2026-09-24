//! D01 Idea reads and the single D02 event-plus-projection transaction kernel.

use super::Store;
use super::row_mappers::{
    CAPTURE_COLUMNS, IDEA_COLUMNS, IDEA_EVENT_COLUMNS, IDEA_RELATIONSHIP_COLUMNS, map_capture_row,
    map_idea_event_row, map_idea_relationship_row, map_idea_row, str_to_session_provider,
    str_to_session_status,
};
use crate::error::{DaemonError, Result};
use crate::idea_control::{
    BoundControllerWriteAuthority, BoundIdeaControllerTransferAuthority, BoundIdeaWriteAuthority,
    IdeaControlError, IssueLinkConstraintClass,
};
use chrono::{DateTime, SecondsFormat, Utc};
use rsi_common::program_runs::{
    canonical_program_run_json, deterministic_program_run_transition_id, program_run_fingerprint,
};
use rsi_common::rpc::{LinkIssueToIdeaParams, LinkIssueToIdeaResult};
use rsi_common::types::{
    ControllerReleaseReasonV1, CreateIdeaRequestV1, IDEA_CONTROLLER_CONTROL_V1,
    IDEA_CONTROLLER_RECONCILIATION_BATCH_SIZE, IDEA_CONTROLLER_RESERVATION_LEASE_SECONDS,
    IDEA_EVENT_PAGE_MAX_LIMIT, Idea, IdeaActorKind, IdeaControllerControlOperationV1,
    IdeaControllerControlOutcomeV1, IdeaControllerControlRequestV1, IdeaControllerControlResultV1,
    IdeaControllerEventPayloadV1, IdeaControllerLaunchConfirmationV1,
    IdeaControllerMutationOutcomeV1, IdeaControllerMutationPayloadV1,
    IdeaControllerMutationRequestV1, IdeaControllerMutationResultV1, IdeaControllerReservationV1,
    IdeaEvent, IdeaEventPageRequestV1, IdeaEventPageV1, IdeaEventType, IdeaLifecycle,
    IdeaMutationActionV1, IdeaMutationResultV1, IdeaOptionalStringV1, IdeaRelationship,
    IdeaRelationshipKind, IdeaSemanticOperationV1, IdeaSemanticRequestV1, IdeaStage,
    IdeaTransitionViolation, IdeaWithGenesis, MutateIdeaAsControllerRequestV1, MutateIdeaRequestV1,
    ReleaseAssignedIdeaControllerRequestV1, ReleaseIdeaControllerReservationRequestV1,
    ReserveIdeaControllerRequestV1, SessionProvider, SessionStatus,
    evaluate_idea_lifecycle_transition, evaluate_idea_stage_transition,
    idea_controller_assign_stage_key, idea_controller_assigned_release_stage_key,
    idea_controller_candidate_session_id, idea_controller_event_id, idea_controller_reservation_id,
    idea_controller_reservation_release_stage_key, idea_controller_reserve_stage_key,
};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use uuid::Uuid;

type IdeaResult<T> = std::result::Result<T, IdeaControlError>;

struct ControllerReservationIdentityV1 {
    reservation_id: Uuid,
    candidate_session_id: Uuid,
    stage_key: String,
    event_id: Uuid,
}

struct PreparedControllerReservationV1 {
    committed_idea: Idea,
    event: IdeaEvent,
    payload: IdeaControllerEventPayloadV1,
    reservation: IdeaControllerReservationV1,
}

#[cfg(test)]
macro_rules! d02_fault {
    ($fault:expr) => {
        d02_fault_impl($fault)
    };
}

#[cfg(not(test))]
macro_rules! d02_fault {
    ($fault:expr) => {
        Ok::<(), IdeaControlError>(())
    };
}

#[cfg(test)]
macro_rules! d03_fault {
    ($fault:expr) => {
        d03_fault_impl($fault)
    };
}

#[cfg(not(test))]
macro_rules! d03_fault {
    ($fault:expr) => {
        Ok::<(), IdeaControlError>(())
    };
}

#[cfg(test)]
macro_rules! d04_link_fault {
    ($fault:expr) => {
        d04_link_fault_impl($fault)
    };
}

#[cfg(test)]
macro_rules! d04_link_constraint_fault {
    () => {
        d04_link_constraint_fault_impl()
    };
}

impl Store {
    /// Read the process-local semantic grant for one controller session.
    pub(crate) fn controller_grant_v1(
        &self,
        session_id: Uuid,
    ) -> Option<BoundControllerWriteAuthority> {
        self.controller_grants.borrow().get(&session_id).cloned()
    }

    /// Return the current semantic grant together with its process-local
    /// incarnation. ProgramRun capabilities retain this witness so a
    /// revoke/reinstall cycle cannot recreate an authority they already held.
    pub(crate) fn controller_grant_witness_v1(
        &self,
        session_id: Uuid,
    ) -> Option<(BoundControllerWriteAuthority, Uuid)> {
        let grant = self.controller_grants.borrow().get(&session_id).cloned()?;
        let incarnation = self
            .controller_grant_incarnations
            .borrow()
            .get(&session_id)
            .copied()?;
        Some((grant, incarnation))
    }

    /// Install a process-local semantic grant after all reconstruction or
    /// assignment witnesses have been verified.
    pub(crate) fn install_controller_grant_v1(&self, grant: BoundControllerWriteAuthority) {
        let session_id = grant.controller_session_id();
        self.controller_grants
            .borrow_mut()
            .insert(session_id, grant);
        self.controller_grant_incarnations
            .borrow_mut()
            .insert(session_id, Uuid::new_v4());
    }

    /// Remove one process-local semantic grant. Durable ownership is unchanged.
    pub(crate) fn remove_controller_grant_v1(&self, session_id: Uuid) {
        self.controller_grants.borrow_mut().remove(&session_id);
        self.controller_grant_incarnations
            .borrow_mut()
            .remove(&session_id);
    }

    /// Clear every process-local semantic grant at daemon reconciliation.
    pub(crate) fn clear_controller_grants_v1(&self) {
        self.controller_grants.borrow_mut().clear();
        self.controller_grant_incarnations.borrow_mut().clear();
    }

    /// Atomically publish the candidate semantic grant and retire the former
    /// process-local grant after the durable assignment has committed.
    pub(crate) fn transfer_controller_grant_v1(
        &self,
        former_session_id: Option<Uuid>,
        candidate: BoundControllerWriteAuthority,
    ) {
        let candidate_session_id = candidate.controller_session_id();
        let mut grants = self.controller_grants.borrow_mut();
        grants.insert(candidate_session_id, candidate);
        self.controller_grant_incarnations
            .borrow_mut()
            .insert(candidate_session_id, Uuid::new_v4());
        if let Some(former_session_id) = former_session_id
            && former_session_id != candidate_session_id
        {
            grants.remove(&former_session_id);
            self.controller_grant_incarnations
                .borrow_mut()
                .remove(&former_session_id);
        }
    }

    /// Return exactly one Idea projection and its immutable genesis Capture.
    ///
    /// D01 intentionally exposes no event replay, collection traversal, or
    /// production mutation path.
    ///
    /// # Errors
    ///
    /// Returns an error when `SQLite` access fails or persisted data violates
    /// the strict D01 value contracts.
    pub fn get_idea_with_genesis(&self, idea_id: Uuid) -> Result<Option<IdeaWithGenesis>> {
        let idea = self
            .conn
            .query_row(
                &format!("SELECT {IDEA_COLUMNS} FROM ideas WHERE id = ?1"),
                [idea_id.to_string()],
                map_idea_row,
            )
            .optional()?;
        let Some(idea) = idea else {
            return Ok(None);
        };
        let idea = idea.into_idea()?;

        let capture = self
            .conn
            .query_row(
                &format!(
                    "SELECT {CAPTURE_COLUMNS}
                     FROM captures
                     WHERE id = ?1 AND project_id = ?2"
                ),
                params![
                    idea.genesis_capture_id.to_string(),
                    idea.project_id.to_string()
                ],
                map_capture_row,
            )
            .optional()?
            .ok_or_else(|| {
                DaemonError::Store(format!(
                    "Idea {} has no same-project genesis Capture",
                    idea.id
                ))
            })?
            .into_capture()?;

        Ok(Some(IdeaWithGenesis {
            idea,
            genesis: capture,
        }))
    }

    pub(crate) fn create_idea_v1(
        &self,
        authority: &BoundIdeaWriteAuthority,
        request: &CreateIdeaRequestV1,
    ) -> IdeaResult<IdeaMutationResultV1> {
        validate_authority(authority)?;
        let request = request
            .normalized()
            .map_err(IdeaControlError::InvalidRequest)?;
        let idea_id = deterministic_idea_id(authority.project_id(), &request.idempotency_key);
        let envelope = request
            .semantic_envelope(authority.project_id(), idea_id, authority.actor_id())
            .map_err(IdeaControlError::InvalidRequest)?;
        let canonical_json = envelope
            .canonical_json()
            .map_err(IdeaControlError::InvalidRequest)?;
        let event_id = deterministic_event_id(idea_id, &request.idempotency_key);

        let tx = immediate_transaction(&self.conn)?;
        if let Some(replay) = load_exact_replay_tx(&tx, &envelope, &request.idempotency_key)? {
            tx.commit().map_err(map_sql_error)?;
            return Ok(replay);
        }
        if load_idea_by_id_tx(&tx, idea_id)?.is_some() {
            return Err(IdeaControlError::IdempotencyConflict);
        }

        require_capture_tx(&tx, authority.project_id(), request.genesis_capture_id)?;
        if let Some(origin_id) = request.derived_from_idea_id {
            if origin_id == idea_id {
                return Err(IdeaControlError::RelationshipConflict(
                    "derived_from cannot target the new Idea itself".to_string(),
                ));
            }
            require_same_project_idea_tx(&tx, authority.project_id(), origin_id)?;
        }

        let timestamp = Utc::now();
        let idea = Idea {
            id: idea_id,
            project_id: authority.project_id(),
            slug: request.slug.clone(),
            sigil: request.sigil.clone(),
            genesis_capture_id: request.genesis_capture_id,
            genesis_span_start: request.genesis_span.as_ref().map(|span| span.start),
            genesis_span_end: request.genesis_span.as_ref().map(|span| span.end),
            genesis_span_digest: request
                .genesis_span
                .as_ref()
                .map(|span| span.digest.clone()),
            title: request.title.clone(),
            description: request.description.clone(),
            portfolio_summary: request.portfolio_summary.clone(),
            lifecycle: IdeaLifecycle::Open,
            stage: IdeaStage::Captured,
            priority: request.priority,
            autonomy_policy: request.autonomy_policy,
            integration_target_ref: request.integration_target_ref.clone(),
            program_template_policy_id: request.program_template_policy_id.clone(),
            current_controller_session_id: None,
            controller_epoch: 0,
            row_version: 1,
            next_event_sequence: 2,
            created_at: timestamp,
            updated_at: timestamp,
            terminal_at: None,
            superseded_at: None,
        };
        idea.validate().map_err(IdeaControlError::InvalidRequest)?;
        insert_idea_tx(&tx, &idea)?;
        d02_fault!(IdeaWriteFault::CreateAfterIdea)?;

        let event = build_event(
            event_id,
            &envelope,
            &request.idempotency_key,
            1,
            1,
            timestamp,
            &canonical_json,
        )?;
        insert_event_tx(&tx, &event, &canonical_json)?;
        d02_fault!(IdeaWriteFault::CreateAfterEvent)?;

        let mut relationships = Vec::new();
        if let Some(origin_id) = request.derived_from_idea_id {
            let relationship = build_relationship(
                event.id,
                authority.project_id(),
                idea_id,
                origin_id,
                IdeaRelationshipKind::DerivedFrom,
                timestamp,
            );
            insert_relationship_tx(&tx, &relationship)?;
            relationships.push(relationship);
            d02_fault!(IdeaWriteFault::CreateAfterRelationship)?;
        }
        d02_fault!(IdeaWriteFault::CreateBeforeCommit)?;
        tx.commit().map_err(map_sql_error)?;
        Ok(IdeaMutationResultV1 {
            idea,
            event,
            relationships,
            deduplicated: false,
        })
    }

    pub(crate) fn mutate_idea_v1(
        &self,
        authority: &BoundIdeaWriteAuthority,
        idea_id: Uuid,
        request: &MutateIdeaRequestV1,
    ) -> IdeaResult<IdeaMutationResultV1> {
        validate_authority(authority)?;
        let request = request
            .normalized()
            .map_err(IdeaControlError::InvalidRequest)?;
        let envelope = request
            .semantic_envelope(authority.project_id(), idea_id, authority.actor_id())
            .map_err(IdeaControlError::InvalidRequest)?;
        let canonical_json = envelope
            .canonical_json()
            .map_err(IdeaControlError::InvalidRequest)?;
        let event_id = deterministic_event_id(idea_id, &request.idempotency_key);

        let tx = immediate_transaction(&self.conn)?;
        if let Some(replay) = load_exact_replay_tx(&tx, &envelope, &request.idempotency_key)? {
            tx.commit().map_err(map_sql_error)?;
            return Ok(replay);
        }
        let mut idea = require_same_project_idea_tx(&tx, authority.project_id(), idea_id)?;
        let relationship_write = prepare_mutation_tx(&tx, &idea, &request.action)?;
        if request.expected_row_version != idea.row_version {
            return Err(IdeaControlError::StaleVersion {
                expected: request.expected_row_version,
                actual: idea.row_version,
            });
        }

        let timestamp = Utc::now();
        apply_action_to_projection(&mut idea, &request.action, timestamp);
        let sequence = idea.next_event_sequence;
        let resulting_version = idea.row_version + 1;
        let changed = update_idea_projection_tx(
            &tx,
            &idea,
            request.expected_row_version,
            sequence,
            timestamp,
        )?;
        if changed != 1 {
            let actual = load_idea_for_project_tx(&tx, authority.project_id(), idea_id)?
                .map_or(request.expected_row_version, |current| current.row_version);
            return Err(IdeaControlError::StaleVersion {
                expected: request.expected_row_version,
                actual,
            });
        }
        idea.row_version = resulting_version;
        idea.next_event_sequence = sequence + 1;
        idea.updated_at = timestamp;
        match request.action {
            IdeaMutationActionV1::Abandon { .. } => idea.terminal_at = Some(timestamp),
            IdeaMutationActionV1::AcceptSupersession { .. } => {
                idea.terminal_at = Some(timestamp);
                idea.superseded_at = Some(timestamp);
            }
            _ => {}
        }
        d02_fault!(relationship_write.after_cas_fault())?;

        let event = build_event(
            event_id,
            &envelope,
            &request.idempotency_key,
            sequence,
            resulting_version,
            timestamp,
            &canonical_json,
        )?;
        insert_event_tx(&tx, &event, &canonical_json)?;
        d02_fault!(relationship_write.after_event_fault())?;

        #[cfg(test)]
        let before_commit_fault = relationship_write.before_commit_fault();
        let relationships =
            apply_relationship_write_tx(&tx, relationship_write, &event, timestamp)?;
        d02_fault!(before_commit_fault)?;
        tx.commit().map_err(map_sql_error)?;
        Ok(IdeaMutationResultV1 {
            idea,
            event,
            relationships,
            deduplicated: false,
        })
    }

    /// Atomically link a previously-unlinked project-owned Issue, advance the
    /// Idea's semantic version, and append the corresponding `issue_linked`
    /// event.  Exact event replay deliberately happens before stale-version
    /// or link-emptiness checks.
    pub(crate) fn link_issue_to_idea_v1(
        &self,
        authority: &BoundIdeaWriteAuthority,
        request: &LinkIssueToIdeaParams,
    ) -> IdeaResult<LinkIssueToIdeaResult> {
        validate_authority(authority)?;
        if request.idempotency_key.is_empty()
            || request.idempotency_key.len() > 128
            || request.idempotency_key.contains('\0')
            || request.expected_idea_row_version < 1
            || request.issue_id.is_nil()
        {
            return Err(IdeaControlError::InvalidRequest(
                "invalid Issue link request".to_string(),
            ));
        }
        let evidence_digests = request
            .source_finding_ref
            .as_ref()
            .map(|finding| vec![finding.artifact_digest()])
            .unwrap_or_default();
        let envelope = IdeaSemanticRequestV1 {
            domain: rsi_common::types::IDEA_SEMANTIC_REQUEST_V1.to_string(),
            project_id: authority.project_id(),
            idea_id: request.idea_id,
            expected_row_version: request.expected_idea_row_version,
            actor_kind: IdeaActorKind::Operator,
            actor_id: authority.actor_id().to_string(),
            operation: IdeaSemanticOperationV1::LinkIssue {
                issue_id: request.issue_id,
                source_event_id: request.source_event_id,
                source_finding_ref: request.source_finding_ref.clone(),
            },
            artifact_digests: Vec::new(),
            evidence_digests,
        };
        envelope
            .validate()
            .map_err(IdeaControlError::InvalidRequest)?;
        let canonical_json = envelope
            .canonical_json()
            .map_err(IdeaControlError::InvalidRequest)?;
        let event_id = deterministic_event_id(request.idea_id, &request.idempotency_key);
        let tx = immediate_transaction(&self.conn)?;
        #[cfg(test)]
        d04_link_constraint_fault!()?;
        let issue = Self::get_issue_tx(&tx, request.issue_id)
            .map_err(|error| IdeaControlError::StorageFailure(error.to_string()))?
            .ok_or(IdeaControlError::IssueNotFound)?;
        if issue.project_id != authority.project_id() {
            return Err(IdeaControlError::ProjectScopeMismatch);
        }
        if let Some(replay) = load_exact_replay_tx(&tx, &envelope, &request.idempotency_key)? {
            let expected_tuple = (
                Some(request.idea_id),
                request.source_event_id,
                request.source_finding_ref.as_ref(),
            );
            if (
                issue.idea_id,
                issue.source_event_id,
                issue.source_finding_ref.as_ref(),
            ) != expected_tuple
            {
                return Err(IdeaControlError::CorruptStoredIssueLink);
            }
            tx.commit().map_err(map_sql_error)?;
            return Ok(LinkIssueToIdeaResult {
                issue,
                idea: replay.idea,
                event: replay.event,
                deduplicated: true,
            });
        }
        match (
            issue.idea_id,
            issue.source_event_id,
            issue.source_finding_ref.as_ref(),
        ) {
            (None, None, None) => {}
            (Some(_), _, _) => return Err(IdeaControlError::IssueAlreadyLinked),
            _ => return Err(IdeaControlError::CorruptStoredIssueLink),
        }
        let mut idea = require_same_project_idea_tx(&tx, authority.project_id(), request.idea_id)?;
        if let Some(source_event_id) = request.source_event_id {
            let valid_source: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM idea_events
                 WHERE id = ?1 AND idea_id = ?2 AND project_id = ?3)",
                    params![
                        source_event_id.to_string(),
                        request.idea_id.to_string(),
                        authority.project_id().to_string()
                    ],
                    |row| row.get(0),
                )
                .map_err(map_sql_error)?;
            if !valid_source {
                return Err(IdeaControlError::SourceEventScopeMismatch);
            }
        }
        if request.expected_idea_row_version != idea.row_version {
            return Err(IdeaControlError::StaleVersion {
                expected: request.expected_idea_row_version,
                actual: idea.row_version,
            });
        }
        let timestamp = Utc::now();
        let sequence = idea.next_event_sequence;
        let resulting_version = idea.row_version + 1;
        if update_idea_projection_tx(
            &tx,
            &idea,
            request.expected_idea_row_version,
            sequence,
            timestamp,
        )? != 1
        {
            return stale_version_after_failed_cas(&tx, &idea, request.expected_idea_row_version);
        }
        idea.row_version = resulting_version;
        idea.next_event_sequence = sequence + 1;
        idea.updated_at = timestamp;
        #[cfg(test)]
        d04_link_fault!(IssueLinkWriteFault::AfterIdeaCas)?;
        let changed = tx
            .execute(
                "UPDATE issues
             SET idea_id = ?1, source_event_id = ?2, source_finding_ref = ?3, updated_at = ?4,
                 row_version = row_version + 1
             WHERE id = ?5 AND project_id = ?6
               AND row_version = ?7
               AND idea_id IS NULL AND source_event_id IS NULL AND source_finding_ref IS NULL",
                params![
                    request.idea_id.to_string(),
                    request.source_event_id.map(|id| id.to_string()),
                    request.source_finding_ref.as_ref().map(ToString::to_string),
                    canonical_timestamp(timestamp),
                    request.issue_id.to_string(),
                    authority.project_id().to_string(),
                    issue.row_version,
                ],
            )
            .map_err(map_sql_error)?;
        if changed != 1 {
            return Err(IdeaControlError::IssueAlreadyLinked);
        }
        super::issues::issue_projection_written()
            .map_err(|error| IdeaControlError::StorageFailure(error.to_string()))?;
        #[cfg(test)]
        d04_link_fault!(IssueLinkWriteFault::AfterIssueUpdate)?;
        let event = build_event(
            event_id,
            &envelope,
            &request.idempotency_key,
            sequence,
            resulting_version,
            timestamp,
            &canonical_json,
        )?;
        insert_event_tx(&tx, &event, &canonical_json)?;
        #[cfg(test)]
        d04_link_fault!(IssueLinkWriteFault::AfterEventInsert)?;
        let mut linked_issue = issue;
        linked_issue.idea_id = Some(request.idea_id);
        linked_issue.source_event_id = request.source_event_id;
        linked_issue.source_finding_ref = request.source_finding_ref.clone();
        linked_issue.updated_at = timestamp;
        linked_issue.row_version += 1;
        super::issues::append_issue_idea_link_event_tx(
            &tx,
            linked_issue.clone(),
            linked_issue.row_version - 1,
            request.idea_id,
            request.source_event_id,
            request.source_finding_ref.as_ref(),
        )
        .map_err(|error| IdeaControlError::StorageFailure(error.to_string()))?;
        #[cfg(test)]
        d04_link_fault!(IssueLinkWriteFault::BeforeCommit)?;
        super::issues::issue_write_before_commit()
            .map_err(|error| IdeaControlError::StorageFailure(error.to_string()))?;
        tx.commit().map_err(map_sql_error)?;
        Ok(LinkIssueToIdeaResult {
            issue: linked_issue,
            idea,
            event,
            deduplicated: false,
        })
    }

    pub(crate) fn list_idea_events_v1(
        &self,
        project_id: Uuid,
        idea_id: Uuid,
        page: IdeaEventPageRequestV1,
    ) -> IdeaResult<IdeaEventPageV1> {
        let limit = page
            .validated_limit()
            .map_err(IdeaControlError::InvalidRequest)?;
        require_same_project_idea_connection(&self.conn, project_id, idea_id)?;
        let query_limit = i64::try_from(limit + 1).map_err(|_| {
            IdeaControlError::InvalidRequest("Idea event page limit is out of range".to_string())
        })?;
        let mut statement = self
            .conn
            .prepare(&format!(
                "SELECT {IDEA_EVENT_COLUMNS}
                 FROM idea_events
                 WHERE project_id = ?1 AND idea_id = ?2 AND sequence > ?3
                 ORDER BY sequence ASC
                 LIMIT ?4"
            ))
            .map_err(map_sql_error)?;
        let rows = statement
            .query_map(
                params![
                    project_id.to_string(),
                    idea_id.to_string(),
                    page.after_sequence,
                    query_limit,
                ],
                map_idea_event_row,
            )
            .map_err(map_sql_error)?;
        let mut events = Vec::with_capacity(limit + 1);
        for row in rows {
            let event = row
                .map_err(map_sql_error)?
                .into_idea_event()
                .map_err(corrupt_event)?;
            validate_strict_idea_event(&event)?;
            events.push(event);
        }
        let has_more = events.len() > limit;
        events.truncate(limit);
        let next_after_sequence = has_more
            .then(|| events.last().map(|event| event.sequence))
            .flatten();
        Ok(IdeaEventPageV1 {
            events,
            next_after_sequence,
        })
    }
}

/// One stable Idea cursor used by the 64-Idea startup reconciliation pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(
    clippy::redundant_pub_crate,
    reason = "explicit crate barrier shares only the bounded reconciliation cursor"
)]
pub(crate) struct IdeaControllerReconciliationCursor {
    pub(crate) project_id: Uuid,
    pub(crate) idea_id: Uuid,
}

/// Committed events that startup reconciliation must publish by event ID.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(
    clippy::redundant_pub_crate,
    reason = "explicit crate barrier shares only bounded reconciliation output"
)]
pub(crate) struct IdeaControllerReconciliationResult {
    pub(crate) next_cursor: Option<IdeaControllerReconciliationCursor>,
    pub(crate) committed_event_ids: Vec<Uuid>,
    pub(crate) corrupt_idea_ids: Vec<Uuid>,
}

struct AssignedReleaseAuthority<'a> {
    project_id: Uuid,
    idea_id: Uuid,
    actor_kind: IdeaActorKind,
    actor_id: &'a str,
    observed_epoch: i64,
    bound_session_id: Option<Uuid>,
}

impl Store {
    /// Resolve the one durable Idea currently assigned to a Session. Lineage,
    /// status, and active-map state are intentionally irrelevant.
    pub(crate) fn assigned_idea_for_session_v1(
        &self,
        session_id: Uuid,
    ) -> IdeaResult<Option<Idea>> {
        let mut statement = self
            .conn
            .prepare(&format!(
                "SELECT {IDEA_COLUMNS}
                 FROM ideas
                 WHERE current_controller_session_id = ?1
                 ORDER BY project_id, id
                 LIMIT 2"
            ))
            .map_err(map_sql_error)?;
        let rows = statement
            .query_map(params![session_id.to_string()], map_idea_row)
            .map_err(map_sql_error)?;
        let mut ideas = Vec::new();
        for row in rows {
            ideas.push(
                row.map_err(map_sql_error)?
                    .into_idea()
                    .map_err(corrupt_event)?,
            );
        }
        if ideas.len() > 1 {
            return Err(corrupt_event(
                "one Session is assigned to more than one Idea",
            ));
        }
        Ok(ideas.pop())
    }

    /// Load the exact same-project Idea projection used to bind a transfer
    /// handle's observed epoch.
    pub(crate) fn load_idea_controller_projection_v1(
        &self,
        project_id: Uuid,
        idea_id: Uuid,
    ) -> IdeaResult<Idea> {
        require_same_project_idea_connection(&self.conn, project_id, idea_id)
    }

    /// Read-only exact reservation confirmation used immediately before a
    /// provider establishment signal is emitted.
    pub(crate) fn confirm_controller_candidate_reservation_v1(
        &self,
        project_id: Uuid,
        idea_id: Uuid,
        reservation: &IdeaControllerReservationV1,
        now: DateTime<Utc>,
    ) -> IdeaResult<()> {
        let idea = require_same_project_idea_connection(&self.conn, project_id, idea_id)?;
        let event = load_latest_controller_event_connection(&self.conn, idea_id)?
            .ok_or(IdeaControlError::ControllerReservationNotFound)?;
        let payload = event
            .controller_control_payload_v1()
            .map_err(corrupt_event)?;
        if !controller_tail_matches_projection(&payload, &idea) {
            return Err(corrupt_event(
                "latest controller event disagrees with assigned projection",
            ));
        }
        let current = match payload.request.operation {
            IdeaControllerControlOperationV1::Reserve { .. } => payload.outcome.reservation,
            _ => None,
        }
        .ok_or(IdeaControlError::ControllerReservationResolved)?;
        if current != *reservation {
            return Err(IdeaControlError::ControllerReservationNotFound);
        }
        if !current.is_live_at(now) {
            return Err(IdeaControlError::ControllerReservationExpired);
        }
        Ok(())
    }

    /// Reserve one deterministic candidate without changing assigned owner or
    /// assigned epoch.
    pub(crate) fn reserve_idea_controller_v1(
        &self,
        authority: &BoundIdeaControllerTransferAuthority,
        request: &ReserveIdeaControllerRequestV1,
        now: DateTime<Utc>,
    ) -> IdeaResult<IdeaControllerControlResultV1> {
        let request = request
            .normalized()
            .map_err(IdeaControlError::InvalidRequest)?;
        self.reserve_idea_controller_from_base_v1(authority, &request, now, None)
    }

    fn reserve_idea_controller_from_base_v1(
        &self,
        authority: &BoundIdeaControllerTransferAuthority,
        request: &ReserveIdeaControllerRequestV1,
        now: DateTime<Utc>,
        post_expiry_base_row_version: Option<i64>,
    ) -> IdeaResult<IdeaControllerControlResultV1> {
        let mut post_expiry_base_row_version = post_expiry_base_row_version;
        let reservation_id =
            idea_controller_reservation_id(authority.idea_id(), &request.transfer_intent_key);
        let identity = ControllerReservationIdentityV1 {
            reservation_id,
            candidate_session_id: idea_controller_candidate_session_id(reservation_id),
            stage_key: idea_controller_reserve_stage_key(reservation_id),
            event_id: idea_controller_event_id(
                authority.idea_id(),
                &idea_controller_reserve_stage_key(reservation_id),
            ),
        };

        let tx = immediate_transaction(&self.conn)?;
        if let Some(replay) = load_reserve_replay_tx(&tx, authority, request, &identity.stage_key)?
        {
            let reservation = replay
                .reservation
                .as_ref()
                .ok_or_else(|| corrupt_event("reserved replay omitted reservation outcome"))?;
            if reservation.is_live_at(now) {
                tx.commit().map_err(map_sql_error)?;
                return Ok(replay);
            }
            let reservation = reservation.clone();
            drop(tx);
            match self.release_controller_reservation_snapshot_v1(
                authority.project_id(),
                authority.idea_id(),
                IdeaActorKind::System,
                "rsid:controller-transfer",
                &reservation,
                ControllerReleaseReasonV1::Expired,
                now,
            ) {
                Ok(_)
                | Err(
                    IdeaControlError::ControllerReservationResolved
                    | IdeaControlError::IdempotencyConflict,
                ) => {
                    return Err(IdeaControlError::ControllerReservationResolved);
                }
                Err(error) => return Err(error),
            }
        }

        let idea = require_same_project_idea_tx(&tx, authority.project_id(), authority.idea_id())?;
        if let Some(tail) = load_latest_controller_event_tx(&tx, idea.id)? {
            let payload = tail
                .controller_control_payload_v1()
                .map_err(corrupt_event)?;
            match payload.request.operation {
                IdeaControllerControlOperationV1::Reserve { .. } => {
                    let reservation = payload.outcome.reservation.ok_or_else(|| {
                        corrupt_event("reserved event omitted reservation outcome")
                    })?;
                    if reservation.is_live_at(now) {
                        return Err(IdeaControlError::ControllerReservationConflict);
                    }
                    drop(tx);
                    let released = self.release_controller_reservation_snapshot_v1(
                        authority.project_id(),
                        authority.idea_id(),
                        IdeaActorKind::System,
                        "rsid:controller-transfer",
                        &reservation,
                        ControllerReleaseReasonV1::Expired,
                        now,
                    )?;
                    return self.reserve_idea_controller_from_base_v1(
                        authority,
                        request,
                        now,
                        Some(released.idea.row_version),
                    );
                }
                IdeaControllerControlOperationV1::ReleaseReservation {
                    reason: ControllerReleaseReasonV1::Expired,
                    ..
                } if request.expected_row_version.checked_add(1) == Some(idea.row_version) => {
                    post_expiry_base_row_version = Some(idea.row_version);
                }
                _ => {}
            }
        }
        validate_reservation_base_v1(&idea, authority, request, post_expiry_base_row_version)?;
        let prepared =
            prepare_controller_reservation_v1(authority, request, &idea, now, &identity)?;
        Self::commit_prepared_controller_reservation_v1(tx, &idea, prepared, now)
    }

    fn commit_prepared_controller_reservation_v1(
        tx: Transaction<'_>,
        idea: &Idea,
        prepared: PreparedControllerReservationV1,
        now: DateTime<Utc>,
    ) -> IdeaResult<IdeaControllerControlResultV1> {
        let base_row_version = prepared.reservation.base_row_version;
        let changed = update_idea_controller_projection_tx(
            &tx,
            idea,
            idea.current_controller_session_id,
            idea.controller_epoch,
            base_row_version,
            prepared.reservation.reserved_sequence,
            now,
        )?;
        if changed != 1 {
            return stale_version_after_failed_cas(&tx, idea, base_row_version);
        }
        d03_fault!(IdeaControllerWriteFault::ReserveAfterProjection)?;

        insert_controller_event_tx(
            &tx,
            &prepared.event,
            &prepared
                .payload
                .canonical_json()
                .map_err(IdeaControlError::InvalidRequest)?,
        )?;
        d03_fault!(IdeaControllerWriteFault::ReserveAfterEvent)?;
        d03_fault!(IdeaControllerWriteFault::ReserveBeforeCommit)?;
        tx.commit().map_err(map_sql_error)?;
        Ok(IdeaControllerControlResultV1 {
            idea: prepared.committed_idea,
            event: prepared.event,
            reservation: Some(prepared.reservation),
            deduplicated: false,
        })
    }

    /// Assign the candidate only from the exact live reservation and a durable
    /// provider-neutral confirmation.
    pub(crate) fn assign_idea_controller_v1(
        &self,
        authority: &BoundIdeaControllerTransferAuthority,
        reservation: &IdeaControllerReservationV1,
        confirmation: &IdeaControllerLaunchConfirmationV1,
        now: DateTime<Utc>,
    ) -> IdeaResult<IdeaControllerControlResultV1> {
        reservation
            .validate(authority.idea_id())
            .map_err(IdeaControlError::InvalidRequest)?;
        confirmation
            .validate(reservation, authority.project_id())
            .map_err(|_| IdeaControlError::LaunchNotConfirmed)?;
        if authority.actor_kind() != IdeaActorKind::System {
            return Err(IdeaControlError::ForbiddenActor);
        }
        let stage_key = idea_controller_assign_stage_key(reservation.reservation_id);
        let request = IdeaControllerControlRequestV1 {
            domain: IDEA_CONTROLLER_CONTROL_V1.to_string(),
            project_id: authority.project_id(),
            idea_id: authority.idea_id(),
            actor_kind: IdeaActorKind::System,
            actor_id: authority.actor_id().to_string(),
            operation: IdeaControllerControlOperationV1::Assign {
                reservation: reservation.clone(),
                confirmation: confirmation.clone(),
            },
        };
        request
            .validate()
            .map_err(IdeaControlError::InvalidRequest)?;

        let tx = immediate_transaction(&self.conn)?;
        if let Some(replay) = load_controller_control_replay_tx(&tx, &request, &stage_key)? {
            tx.commit().map_err(map_sql_error)?;
            return Ok(replay);
        }
        let idea = require_same_project_idea_tx(&tx, authority.project_id(), authority.idea_id())?;
        let tail = require_live_reservation_tail_tx(&tx, &idea, reservation, now)?;
        d03_fault!(IdeaControllerWriteFault::AssignAfterTailRead)?;
        let reserved_row_version =
            reservation.base_row_version.checked_add(1).ok_or_else(|| {
                IdeaControlError::InvalidRequest(
                    "controller reservation row version overflow".to_string(),
                )
            })?;
        if tail.id != reservation.reserved_event_id {
            return Err(IdeaControlError::ControllerReservationResolved);
        }
        if idea.current_controller_session_id != reservation.base_controller_session_id
            || idea.controller_epoch != reservation.base_controller_epoch
            || idea.row_version != reserved_row_version
        {
            drop(tx);
            let _ = self.release_controller_reservation_snapshot_v1(
                authority.project_id(),
                authority.idea_id(),
                IdeaActorKind::System,
                authority.actor_id(),
                reservation,
                ControllerReleaseReasonV1::StaleAssignmentBase,
                now,
            );
            return Err(IdeaControlError::StaleVersion {
                expected: reserved_row_version,
                actual: idea.row_version,
            });
        }
        require_confirmed_session_row_tx(&tx, reservation, confirmation)?;
        if !reservation.is_live_at(now) {
            return Err(IdeaControlError::ControllerReservationExpired);
        }
        let sequence = idea.next_event_sequence;
        let event_id = idea_controller_event_id(idea.id, &stage_key);
        let changed = update_idea_controller_projection_tx(
            &tx,
            &idea,
            Some(reservation.candidate_session_id),
            reservation.proposed_epoch,
            idea.row_version,
            sequence,
            now,
        )?;
        if changed != 1 {
            return stale_version_after_failed_cas(&tx, &idea, idea.row_version);
        }
        d03_fault!(IdeaControllerWriteFault::AssignAfterProjection)?;
        let mut committed_idea = idea;
        committed_idea.current_controller_session_id = Some(reservation.candidate_session_id);
        committed_idea.controller_epoch = reservation.proposed_epoch;
        advance_idea_counters(&mut committed_idea, now)?;
        let payload = IdeaControllerEventPayloadV1 {
            request,
            outcome: IdeaControllerControlOutcomeV1 {
                idea: committed_idea.clone(),
                event_id,
                sequence,
                resulting_row_version: committed_idea.row_version,
                occurred_at: now,
                reservation: Some(reservation.clone()),
            },
        };
        let event = build_controller_control_event(&payload, &stage_key)?;
        insert_controller_event_tx(
            &tx,
            &event,
            &payload
                .canonical_json()
                .map_err(IdeaControlError::InvalidRequest)?,
        )?;
        rebind_active_program_run_for_controller_assignment_tx(
            &tx,
            reservation,
            &committed_idea,
            event_id,
            self.program_run_boot_id(),
            now,
        )?;
        d03_fault!(IdeaControllerWriteFault::AssignAfterEvent)?;
        d03_fault!(IdeaControllerWriteFault::AssignBeforeCommit)?;
        tx.commit().map_err(map_sql_error)?;
        Ok(IdeaControllerControlResultV1 {
            idea: committed_idea,
            event,
            reservation: Some(reservation.clone()),
            deduplicated: false,
        })
    }

    /// Release the exact reservation derived from a caller-bound transfer
    /// intent.
    pub(crate) fn release_idea_controller_reservation_v1(
        &self,
        authority: &BoundIdeaControllerTransferAuthority,
        request: &ReleaseIdeaControllerReservationRequestV1,
        now: DateTime<Utc>,
    ) -> IdeaResult<IdeaControllerControlResultV1> {
        let reservation_id =
            idea_controller_reservation_id(authority.idea_id(), &request.transfer_intent_key);
        let stage_key = idea_controller_reservation_release_stage_key(reservation_id);
        let tx = immediate_transaction(&self.conn)?;
        if let Some(replay) =
            load_reservation_release_replay_tx(&tx, authority, request, &stage_key)?
        {
            tx.commit().map_err(map_sql_error)?;
            return Ok(replay);
        }
        let idea = require_same_project_idea_tx(&tx, authority.project_id(), authority.idea_id())?;
        let reservation = current_unresolved_reservation_tx(&tx, &idea)?
            .ok_or(IdeaControlError::ControllerReservationNotFound)?;
        if reservation.reservation_id != reservation_id {
            return Err(IdeaControlError::ControllerReservationNotFound);
        }
        drop(tx);
        self.release_controller_reservation_snapshot_v1(
            authority.project_id(),
            authority.idea_id(),
            authority.actor_kind(),
            authority.actor_id(),
            &reservation,
            request.reason,
            now,
        )
    }

    fn release_controller_reservation_snapshot_v1(
        &self,
        project_id: Uuid,
        idea_id: Uuid,
        actor_kind: IdeaActorKind,
        actor_id: &str,
        reservation: &IdeaControllerReservationV1,
        reason: ControllerReleaseReasonV1,
        now: DateTime<Utc>,
    ) -> IdeaResult<IdeaControllerControlResultV1> {
        let stage_key = idea_controller_reservation_release_stage_key(reservation.reservation_id);
        let tx = immediate_transaction(&self.conn)?;
        if let Some(event) = load_event_by_key_tx(&tx, idea_id, &stage_key)? {
            let payload = event
                .controller_control_payload_v1()
                .map_err(corrupt_event)?;
            let requested = IdeaControllerControlRequestV1 {
                domain: IDEA_CONTROLLER_CONTROL_V1.to_string(),
                project_id,
                idea_id,
                actor_kind,
                actor_id: actor_id.to_string(),
                operation: IdeaControllerControlOperationV1::ReleaseReservation {
                    reservation: reservation.clone(),
                    expected_row_version: payload
                        .request
                        .operation
                        .expected_row_version()
                        .map_err(IdeaControlError::InvalidRequest)?,
                    reason,
                },
            };
            if payload.request != requested {
                return Err(IdeaControlError::IdempotencyConflict);
            }
            tx.commit().map_err(map_sql_error)?;
            return Ok(controller_control_result(event, payload, true));
        }
        let idea = require_same_project_idea_tx(&tx, project_id, idea_id)?;
        let current = current_unresolved_reservation_tx(&tx, &idea)?
            .ok_or(IdeaControlError::ControllerReservationResolved)?;
        if current != *reservation {
            return Err(IdeaControlError::ControllerReservationResolved);
        }
        let sequence = idea.next_event_sequence;
        let expected_row_version = idea.row_version;
        let request = IdeaControllerControlRequestV1 {
            domain: IDEA_CONTROLLER_CONTROL_V1.to_string(),
            project_id,
            idea_id,
            actor_kind,
            actor_id: actor_id.to_string(),
            operation: IdeaControllerControlOperationV1::ReleaseReservation {
                reservation: reservation.clone(),
                expected_row_version,
                reason,
            },
        };
        request
            .validate()
            .map_err(IdeaControlError::InvalidRequest)?;
        let changed = update_idea_controller_projection_tx(
            &tx,
            &idea,
            idea.current_controller_session_id,
            idea.controller_epoch,
            expected_row_version,
            sequence,
            now,
        )?;
        if changed != 1 {
            return stale_version_after_failed_cas(&tx, &idea, expected_row_version);
        }
        d03_fault!(IdeaControllerWriteFault::ReservationReleaseAfterProjection)?;
        let mut committed_idea = idea;
        advance_idea_counters(&mut committed_idea, now)?;
        let event_id = idea_controller_event_id(idea_id, &stage_key);
        let payload = IdeaControllerEventPayloadV1 {
            request,
            outcome: IdeaControllerControlOutcomeV1 {
                idea: committed_idea.clone(),
                event_id,
                sequence,
                resulting_row_version: committed_idea.row_version,
                occurred_at: now,
                reservation: Some(reservation.clone()),
            },
        };
        let event = build_controller_control_event(&payload, &stage_key)?;
        insert_controller_event_tx(
            &tx,
            &event,
            &payload
                .canonical_json()
                .map_err(IdeaControlError::InvalidRequest)?,
        )?;
        d03_fault!(IdeaControllerWriteFault::ReservationReleaseAfterEvent)?;
        d03_fault!(IdeaControllerWriteFault::ReservationReleaseBeforeCommit)?;
        tx.commit().map_err(map_sql_error)?;
        Ok(IdeaControllerControlResultV1 {
            idea: committed_idea,
            event,
            reservation: Some(reservation.clone()),
            deduplicated: false,
        })
    }

    /// Operator/system release of the exact assigned controller.
    pub(crate) fn release_assigned_idea_controller_v1(
        &self,
        authority: &BoundIdeaControllerTransferAuthority,
        request: &ReleaseAssignedIdeaControllerRequestV1,
        now: DateTime<Utc>,
    ) -> IdeaResult<IdeaControllerControlResultV1> {
        self.release_assigned_controller_inner_v1(
            &AssignedReleaseAuthority {
                project_id: authority.project_id(),
                idea_id: authority.idea_id(),
                actor_kind: authority.actor_kind(),
                actor_id: authority.actor_id(),
                observed_epoch: authority.observed_controller_epoch(),
                bound_session_id: None,
            },
            request,
            now,
        )
    }

    /// Exact controller self-release. Identity and epoch come only from the
    /// bound semantic grant.
    pub(crate) fn release_assigned_idea_controller_by_controller_v1(
        &self,
        authority: &BoundControllerWriteAuthority,
        request: &ReleaseAssignedIdeaControllerRequestV1,
        now: DateTime<Utc>,
    ) -> IdeaResult<IdeaControllerControlResultV1> {
        let actor_id = authority.controller_session_id().to_string();
        self.release_assigned_controller_inner_v1(
            &AssignedReleaseAuthority {
                project_id: authority.project_id(),
                idea_id: authority.idea_id(),
                actor_kind: IdeaActorKind::Session,
                actor_id: &actor_id,
                observed_epoch: authority.controller_epoch(),
                bound_session_id: Some(authority.controller_session_id()),
            },
            request,
            now,
        )
    }

    fn release_assigned_controller_inner_v1(
        &self,
        authority: &AssignedReleaseAuthority<'_>,
        request: &ReleaseAssignedIdeaControllerRequestV1,
        now: DateTime<Utc>,
    ) -> IdeaResult<IdeaControllerControlResultV1> {
        let stage_key = idea_controller_assigned_release_stage_key(
            authority.idea_id,
            &request.release_intent_key,
        );
        let tx = immediate_transaction(&self.conn)?;
        if let Some(replay) = load_assigned_release_replay_tx(&tx, authority, request, &stage_key)?
        {
            tx.commit().map_err(map_sql_error)?;
            return Ok(replay);
        }
        let idea = require_same_project_idea_tx(&tx, authority.project_id, authority.idea_id)?;
        if current_unresolved_reservation_tx(&tx, &idea)?.is_some() {
            return Err(IdeaControlError::ControllerReservationConflict);
        }
        let assigned_session = idea
            .current_controller_session_id
            .ok_or(IdeaControlError::ControllerMismatch)?;
        if authority
            .bound_session_id
            .is_some_and(|bound| bound != assigned_session)
        {
            return Err(IdeaControlError::ControllerMismatch);
        }
        if idea.controller_epoch != authority.observed_epoch {
            return Err(IdeaControlError::StaleControllerEpoch {
                expected: authority.observed_epoch,
                actual: idea.controller_epoch,
            });
        }
        if idea.row_version != request.expected_row_version {
            return Err(IdeaControlError::StaleVersion {
                expected: request.expected_row_version,
                actual: idea.row_version,
            });
        }
        let semantic_request = IdeaControllerControlRequestV1 {
            domain: IDEA_CONTROLLER_CONTROL_V1.to_string(),
            project_id: authority.project_id,
            idea_id: authority.idea_id,
            actor_kind: authority.actor_kind,
            actor_id: authority.actor_id.to_string(),
            operation: IdeaControllerControlOperationV1::ReleaseAssigned {
                release_intent_key: request.release_intent_key.clone(),
                expected_row_version: request.expected_row_version,
                observed_controller_epoch: authority.observed_epoch,
                controller_session_id: assigned_session,
                controller_epoch: idea.controller_epoch,
                reason: request.reason,
            },
        };
        semantic_request
            .validate()
            .map_err(IdeaControlError::InvalidRequest)?;
        let sequence = idea.next_event_sequence;
        let changed = update_idea_controller_projection_tx(
            &tx,
            &idea,
            None,
            idea.controller_epoch,
            idea.row_version,
            sequence,
            now,
        )?;
        if changed != 1 {
            return stale_version_after_failed_cas(&tx, &idea, idea.row_version);
        }
        d03_fault!(IdeaControllerWriteFault::AssignedReleaseAfterProjection)?;
        let mut committed_idea = idea;
        committed_idea.current_controller_session_id = None;
        advance_idea_counters(&mut committed_idea, now)?;
        let event_id = idea_controller_event_id(authority.idea_id, &stage_key);
        let payload = IdeaControllerEventPayloadV1 {
            request: semantic_request,
            outcome: IdeaControllerControlOutcomeV1 {
                idea: committed_idea.clone(),
                event_id,
                sequence,
                resulting_row_version: committed_idea.row_version,
                occurred_at: now,
                reservation: None,
            },
        };
        let event = build_controller_control_event(&payload, &stage_key)?;
        insert_controller_event_tx(
            &tx,
            &event,
            &payload
                .canonical_json()
                .map_err(IdeaControlError::InvalidRequest)?,
        )?;
        d03_fault!(IdeaControllerWriteFault::AssignedReleaseAfterEvent)?;
        d03_fault!(IdeaControllerWriteFault::AssignedReleaseBeforeCommit)?;
        tx.commit().map_err(map_sql_error)?;
        Ok(IdeaControllerControlResultV1 {
            idea: committed_idea,
            event,
            reservation: None,
            deduplicated: false,
        })
    }

    /// Controller-authorized projection/stage mutation under exact
    /// project/Idea/session/epoch/row-version fencing.
    pub(crate) fn mutate_idea_as_controller_v1(
        &self,
        authority: &BoundControllerWriteAuthority,
        request: &MutateIdeaAsControllerRequestV1,
        now: DateTime<Utc>,
    ) -> IdeaResult<IdeaControllerMutationResultV1> {
        let semantic_request = IdeaControllerMutationRequestV1::from_bound(
            authority.project_id(),
            authority.idea_id(),
            authority.controller_session_id(),
            authority.controller_epoch(),
            request,
        )
        .map_err(IdeaControlError::InvalidRequest)?;
        let canonical_request = semantic_request
            .canonical_json()
            .map_err(IdeaControlError::InvalidRequest)?;
        let tx = immediate_transaction(&self.conn)?;
        if let Some(replay) =
            load_controller_mutation_replay_tx(&tx, &semantic_request, &request.idempotency_key)?
        {
            tx.commit().map_err(map_sql_error)?;
            return Ok(replay);
        }
        let mut idea =
            require_same_project_idea_tx(&tx, authority.project_id(), authority.idea_id())?;
        if idea.current_controller_session_id != Some(authority.controller_session_id()) {
            return Err(IdeaControlError::ControllerMismatch);
        }
        if idea.controller_epoch != authority.controller_epoch() {
            return Err(IdeaControlError::StaleControllerEpoch {
                expected: authority.controller_epoch(),
                actual: idea.controller_epoch,
            });
        }
        if idea.row_version != request.expected_row_version {
            return Err(IdeaControlError::StaleVersion {
                expected: request.expected_row_version,
                actual: idea.row_version,
            });
        }
        let action = request
            .action
            .normalized_idea_action()
            .map_err(IdeaControlError::InvalidRequest)?;
        let relationship_write = prepare_mutation_tx(&tx, &idea, &action)?;
        if !matches!(relationship_write, RelationshipWrite::None) {
            return Err(IdeaControlError::ForbiddenActor);
        }
        let sequence = idea.next_event_sequence;
        apply_action_to_projection(&mut idea, &action, now);
        let changed =
            update_idea_projection_tx(&tx, &idea, request.expected_row_version, sequence, now)?;
        if changed != 1 {
            return stale_version_after_failed_cas(&tx, &idea, request.expected_row_version);
        }
        advance_idea_counters(&mut idea, now)?;
        d03_fault!(IdeaControllerWriteFault::MutationAfterProjection)?;
        let event_id = deterministic_event_id(idea.id, &request.idempotency_key);
        let payload = IdeaControllerMutationPayloadV1 {
            request: semantic_request,
            outcome: IdeaControllerMutationOutcomeV1 {
                idea: idea.clone(),
                event_id,
                sequence,
                resulting_row_version: idea.row_version,
                occurred_at: now,
            },
        };
        let canonical_payload = payload
            .canonical_json()
            .map_err(IdeaControlError::InvalidRequest)?;
        let event = build_controller_mutation_event(
            &payload,
            &request.idempotency_key,
            &canonical_payload,
        )?;
        insert_controller_event_tx(&tx, &event, &canonical_payload)?;
        d03_fault!(IdeaControllerWriteFault::MutationAfterEvent)?;
        d03_fault!(IdeaControllerWriteFault::MutationBeforeCommit)?;
        tx.commit().map_err(map_sql_error)?;
        let _ = canonical_request;
        Ok(IdeaControllerMutationResultV1 {
            idea,
            event,
            deduplicated: false,
        })
    }

    /// Reconstruct a same-ID semantic grant without changing durable state.
    pub(crate) fn reestablish_idea_controller_v1(
        &self,
        session_id: Uuid,
        project_id: Uuid,
        provider: SessionProvider,
    ) -> IdeaResult<Option<BoundControllerWriteAuthority>> {
        let mut statement = self
            .conn
            .prepare(&format!(
                "SELECT {IDEA_COLUMNS}
                 FROM ideas
                 WHERE project_id = ?1 AND current_controller_session_id = ?2
                 ORDER BY id
                 LIMIT 2"
            ))
            .map_err(map_sql_error)?;
        let rows = statement
            .query_map(
                params![project_id.to_string(), session_id.to_string()],
                map_idea_row,
            )
            .map_err(map_sql_error)?;
        let mut ideas = Vec::new();
        for row in rows {
            ideas.push(
                row.map_err(map_sql_error)?
                    .into_idea()
                    .map_err(corrupt_event)?,
            );
        }
        if ideas.len() > 1 {
            return Err(corrupt_event(
                "one Session is assigned to more than one Idea",
            ));
        }
        let Some(idea) = ideas.pop() else {
            return Ok(None);
        };
        let (stored_provider, stored_status) = self
            .conn
            .query_row(
                "SELECT provider, status FROM sessions
                 WHERE id = ?1 AND project_id = ?2",
                params![session_id.to_string(), project_id.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(map_sql_error)?
            .ok_or(IdeaControlError::ControllerMismatch)?;
        let stored_provider = str_to_session_provider(&stored_provider).map_err(corrupt_event)?;
        let stored_status = str_to_session_status(&stored_status).map_err(corrupt_event)?;
        if stored_provider != provider
            || !matches!(
                stored_status,
                SessionStatus::Starting | SessionStatus::Running | SessionStatus::WaitingApproval
            )
        {
            return Err(IdeaControlError::ControllerMismatch);
        }
        BoundControllerWriteAuthority::new(project_id, idea.id, session_id, idea.controller_epoch)
            .map(Some)
    }

    /// Reconcile one stable 64-Idea page. Pre-boot unresolved reservations are
    /// released with zero grace; committed assignments remain unchanged.
    pub(crate) fn reconcile_idea_controllers_page_v1(
        &self,
        cursor: Option<IdeaControllerReconciliationCursor>,
        boot_at: DateTime<Utc>,
    ) -> IdeaResult<IdeaControllerReconciliationResult> {
        let mut statement = self
            .conn
            .prepare(&format!(
                "SELECT {IDEA_COLUMNS}
                 FROM ideas
                 WHERE (?1 IS NULL)
                    OR project_id > ?1
                    OR (project_id = ?1 AND id > ?2)
                 ORDER BY project_id, id
                 LIMIT ?3"
            ))
            .map_err(map_sql_error)?;
        let cursor_project = cursor.map(|cursor| cursor.project_id.to_string());
        let cursor_idea = cursor.map(|cursor| cursor.idea_id.to_string());
        let rows = statement
            .query_map(
                params![
                    cursor_project,
                    cursor_idea,
                    i64::try_from(IDEA_CONTROLLER_RECONCILIATION_BATCH_SIZE)
                        .map_err(|_| corrupt_event("controller page size overflow"))?,
                ],
                map_idea_row,
            )
            .map_err(map_sql_error)?;
        let mut ideas = Vec::new();
        for row in rows {
            ideas.push(
                row.map_err(map_sql_error)?
                    .into_idea()
                    .map_err(corrupt_event)?,
            );
        }
        drop(statement);

        let mut committed_event_ids = Vec::new();
        let mut corrupt_idea_ids = Vec::new();
        for idea in &ideas {
            let tail = match load_latest_controller_event_connection(&self.conn, idea.id) {
                Ok(tail) => tail,
                Err(IdeaControlError::CorruptStoredEvent(_)) => {
                    corrupt_idea_ids.push(idea.id);
                    continue;
                }
                Err(error) => return Err(error),
            };
            let Some(tail) = tail else {
                if idea.current_controller_session_id.is_some() || idea.controller_epoch != 0 {
                    corrupt_idea_ids.push(idea.id);
                }
                continue;
            };
            let Ok(payload) = tail.controller_control_payload_v1() else {
                corrupt_idea_ids.push(idea.id);
                continue;
            };
            if !controller_tail_matches_projection(&payload, idea) {
                corrupt_idea_ids.push(idea.id);
                continue;
            }
            if let IdeaControllerControlOperationV1::Reserve { .. } = payload.request.operation {
                let Some(reservation) = payload.outcome.reservation else {
                    corrupt_idea_ids.push(idea.id);
                    continue;
                };
                if reservation.reserved_at < boot_at {
                    match self.release_controller_reservation_snapshot_v1(
                        idea.project_id,
                        idea.id,
                        IdeaActorKind::System,
                        "rsid:controller-transfer",
                        &reservation,
                        ControllerReleaseReasonV1::RestartRecovery,
                        boot_at,
                    ) {
                        Ok(result) => committed_event_ids.push(result.event.id),
                        Err(IdeaControlError::ControllerReservationResolved) => {}
                        Err(error) => return Err(error),
                    }
                }
            } else if payload.request.operation.event_type() == IdeaEventType::ControllerAssigned {
                committed_event_ids.push(tail.id);
            }
        }
        let next_cursor = (ideas.len() == IDEA_CONTROLLER_RECONCILIATION_BATCH_SIZE)
            .then(|| ideas.last())
            .flatten()
            .map(|idea| IdeaControllerReconciliationCursor {
                project_id: idea.project_id,
                idea_id: idea.id,
            });
        Ok(IdeaControllerReconciliationResult {
            next_cursor,
            committed_event_ids,
            corrupt_idea_ids,
        })
    }
}

#[allow(clippy::too_many_lines)]
fn rebind_active_program_run_for_controller_assignment_tx(
    tx: &Transaction<'_>,
    reservation: &IdeaControllerReservationV1,
    committed_idea: &Idea,
    idea_event_id: Uuid,
    daemon_boot_id: Uuid,
    now: DateTime<Utc>,
) -> IdeaResult<()> {
    type ActiveRun = (
        String,
        String,
        Option<i64>,
        Option<String>,
        Option<String>,
        i64,
        i64,
        i64,
        String,
        i64,
    );
    let run: Option<ActiveRun> = tx
        .query_row(
            "SELECT id,status,cursor_ordinal,cursor_key,cursor_phase,revision_no,row_version,
                    next_transition_sequence,controller_session_id,controller_epoch
             FROM idea_program_runs WHERE idea_id=?1 AND status NOT IN ('settled','cancelled','failed')",
            [committed_idea.id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?,
                row.get(5)?, row.get(6)?, row.get(7)?, row.get(8)?, row.get(9)?)),
        )
        .optional()
        .map_err(map_sql_error)?;
    let Some((
        run_id,
        status,
        cursor_ordinal,
        cursor_key,
        cursor_phase,
        revision_no,
        row_version,
        sequence,
        former_controller,
        former_epoch,
    )) = run
    else {
        return Ok(());
    };
    let base_controller = reservation
        .base_controller_session_id
        .map(|value| value.to_string());
    if Some(former_controller.as_str()) != base_controller.as_deref()
        || former_epoch != reservation.base_controller_epoch
    {
        return Err(IdeaControlError::ControllerMismatch);
    }
    let run_uuid = Uuid::parse_str(&run_id).map_err(|_| {
        IdeaControlError::CorruptStoredEvent("invalid active ProgramRun UUID".into())
    })?;
    let idempotency_key = format!("controller-rebound:{idea_event_id}");
    let transition_id = deterministic_program_run_transition_id(run_uuid, &idempotency_key);
    let request_json = canonical_program_run_json(&serde_json::json!({
        "program_run_id": run_uuid,
        "former_controller_session_id": reservation.base_controller_session_id,
        "candidate_session_id": reservation.candidate_session_id,
        "former_epoch": reservation.base_controller_epoch,
        "proposed_epoch": reservation.proposed_epoch,
        "idea_event_id": idea_event_id,
    }))
    .map_err(IdeaControlError::InvalidRequest)?;
    let fingerprint =
        program_run_fingerprint("program-run-controller-rebound:v1", request_json.as_bytes());
    let changed = tx.execute(
        "UPDATE idea_program_runs SET controller_session_id=?1,controller_epoch=?2,idea_row_version=?3,
         row_version=row_version+1,next_transition_sequence=next_transition_sequence+1,updated_at=?4
         WHERE id=?5 AND row_version=?6 AND controller_session_id=?7 AND controller_epoch=?8",
        params![reservation.candidate_session_id.to_string(), reservation.proposed_epoch,
            committed_idea.row_version, canonical_timestamp(now), run_id, row_version,
            former_controller, former_epoch],
    ).map_err(map_sql_error)?;
    if changed != 1 {
        return Err(IdeaControlError::ControllerMismatch);
    }
    tx.execute(
        "UPDATE idea_program_run_locks SET controller_session_id=?1,controller_epoch=?2,
         lease_generation=lease_generation+1,
         owner_boot_id=CASE WHEN state='held' THEN ?3 ELSE NULL END,
         heartbeat_at=CASE WHEN state='held' THEN ?4 ELSE NULL END,
         expires_at=CASE WHEN state='held' THEN ?5 ELSE NULL END
         WHERE program_run_id=?6 AND state IN ('requested','held')",
        params![
            reservation.candidate_session_id.to_string(),
            reservation.proposed_epoch,
            daemon_boot_id.to_string(),
            canonical_timestamp(now),
            canonical_timestamp(now + chrono::Duration::seconds(30)),
            run_id
        ],
    )
    .map_err(map_sql_error)?;
    tx.execute(
        "UPDATE idea_program_run_actions SET controller_session_id=?1,controller_epoch=?2,
         state=CASE WHEN state='claimed' THEN 'reserved' ELSE state END,
         claim_boot_id=CASE WHEN state IN ('published','acknowledged') THEN ?3 ELSE NULL END,
         claim_generation=claim_generation+CASE WHEN state IN ('claimed','published','acknowledged') THEN 1 ELSE 0 END,
         claim_run_version=CASE WHEN state IN ('published','acknowledged') THEN ?4 ELSE NULL END,
         claim_lease_generation=CASE WHEN state IN ('published','acknowledged') THEN
           (SELECT COALESCE(MAX(lease_generation),0) FROM idea_program_run_locks
            WHERE program_run_id=idea_program_run_actions.program_run_id AND state='held') ELSE NULL END,
         claimed_at=CASE WHEN state IN ('published','acknowledged') THEN claimed_at ELSE NULL END,
         claim_expires_at=CASE WHEN state IN ('published','acknowledged') THEN ?5 ELSE NULL END,
         updated_at=?6 WHERE program_run_id=?7 AND state IN ('reserved','claimed','published','acknowledged')",
        params![
            reservation.candidate_session_id.to_string(),
            reservation.proposed_epoch,
            daemon_boot_id.to_string(),
            row_version + 1,
            canonical_timestamp(now + chrono::Duration::seconds(30)),
            canonical_timestamp(now),
            run_id,
        ],
    ).map_err(map_sql_error)?;
    tx.execute(
        "INSERT INTO idea_program_run_transitions
         (id,program_run_id,sequence,operation,from_status,to_status,old_cursor_ordinal,old_cursor_key,
          old_cursor_phase,new_cursor_ordinal,new_cursor_key,new_cursor_phase,old_revision_no,new_revision_no,
          actor_kind,actor_session_id,controller_epoch,expected_run_version,resulting_run_version,
          expected_idea_version,resulting_idea_version,idea_event_id,idempotency_key,request_json,
          request_fingerprint,created_at)
         VALUES (?1,?2,?3,'controller_rebound',?4,?4,?5,?6,?7,?5,?6,?7,?8,?8,'system',NULL,
                 ?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
        params![transition_id.to_string(), run_id, sequence, status, cursor_ordinal, cursor_key,
            cursor_phase, revision_no, reservation.proposed_epoch, row_version, row_version + 1,
            committed_idea.row_version - 1, committed_idea.row_version, idea_event_id.to_string(),
            idempotency_key, request_json, fingerprint, canonical_timestamp(now)],
    ).map_err(map_sql_error)?;
    Ok(())
}

#[derive(Debug, Clone)]
enum RelationshipWrite {
    None,
    Add {
        kind: IdeaRelationshipKind,
        target_id: Uuid,
    },
    Remove {
        relationship: IdeaRelationship,
    },
    Supersede {
        target_ids: Vec<Uuid>,
    },
}

impl RelationshipWrite {
    #[cfg(test)]
    const fn after_cas_fault(&self) -> IdeaWriteFault {
        match self {
            Self::None => IdeaWriteFault::MutationAfterCas,
            Self::Add { .. } => IdeaWriteFault::RelationshipAddAfterCas,
            Self::Remove { .. } => IdeaWriteFault::RelationshipRemoveAfterCas,
            Self::Supersede { .. } => IdeaWriteFault::SupersessionAfterCas,
        }
    }

    #[cfg(test)]
    const fn after_event_fault(&self) -> IdeaWriteFault {
        match self {
            Self::None => IdeaWriteFault::MutationAfterEvent,
            Self::Add { .. } => IdeaWriteFault::RelationshipAddAfterEvent,
            Self::Remove { .. } => IdeaWriteFault::RelationshipRemoveAfterEvent,
            Self::Supersede { .. } => IdeaWriteFault::SupersessionAfterEvent,
        }
    }

    #[cfg(test)]
    const fn before_commit_fault(&self) -> IdeaWriteFault {
        match self {
            Self::None => IdeaWriteFault::MutationBeforeCommit,
            Self::Add { .. } => IdeaWriteFault::RelationshipAddBeforeCommit,
            Self::Remove { .. } => IdeaWriteFault::RelationshipRemoveBeforeCommit,
            Self::Supersede { .. } => IdeaWriteFault::SupersessionBeforeCommit,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(test)]
#[allow(
    clippy::redundant_pub_crate,
    reason = "D02 fault identity is intentionally visible only to crate test modules"
)]
pub(crate) enum IdeaWriteFault {
    CreateAfterIdea,
    CreateAfterEvent,
    CreateAfterRelationship,
    CreateBeforeCommit,
    MutationAfterCas,
    MutationAfterEvent,
    MutationBeforeCommit,
    RelationshipAddAfterCas,
    RelationshipAddAfterEvent,
    RelationshipAddAfterEdge,
    RelationshipAddBeforeCommit,
    RelationshipRemoveAfterCas,
    RelationshipRemoveAfterEvent,
    RelationshipRemoveAfterTombstone,
    RelationshipRemoveBeforeCommit,
    SupersessionAfterCas,
    SupersessionAfterEvent,
    SupersessionAfterEachEdge,
    SupersessionBeforeCommit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(test)]
#[allow(
    clippy::redundant_pub_crate,
    reason = "fault identity is intentionally visible only to crate test modules"
)]
pub(crate) enum IdeaControllerWriteFault {
    ReserveAfterProjection,
    ReserveAfterEvent,
    ReserveBeforeCommit,
    AssignAfterTailRead,
    AssignAfterProjection,
    AssignAfterEvent,
    AssignBeforeCommit,
    ReservationReleaseAfterProjection,
    ReservationReleaseAfterEvent,
    ReservationReleaseBeforeCommit,
    AssignedReleaseAfterProjection,
    AssignedReleaseAfterEvent,
    AssignedReleaseBeforeCommit,
    MutationAfterProjection,
    MutationAfterEvent,
    MutationBeforeCommit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(test)]
pub(crate) enum IssueLinkWriteFault {
    AfterIdeaCas,
    AfterIssueUpdate,
    AfterEventInsert,
    BeforeCommit,
    ConstraintForeignKey,
    ConstraintCheck,
    ConstraintNotNull,
    ConstraintUniquePrimary,
}

#[cfg(test)]
thread_local! {
    static D02_IDEA_WRITE_FAULT: std::cell::Cell<Option<IdeaWriteFault>> =
        const { std::cell::Cell::new(None) };
    static D03_IDEA_CONTROLLER_WRITE_FAULT: std::cell::Cell<Option<IdeaControllerWriteFault>> =
        const { std::cell::Cell::new(None) };
    static D04_ISSUE_LINK_WRITE_FAULT: std::cell::Cell<Option<IssueLinkWriteFault>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
#[allow(
    clippy::redundant_pub_crate,
    reason = "D02 fault injection is intentionally visible only to crate test modules"
)]
pub(crate) fn inject_d02_idea_write_fault(fault: IdeaWriteFault) {
    D02_IDEA_WRITE_FAULT.with(|slot| slot.set(Some(fault)));
}

#[cfg(test)]
#[allow(
    clippy::redundant_pub_crate,
    reason = "fault injection is intentionally visible only to crate test modules"
)]
pub(crate) fn inject_d03_idea_controller_write_fault(fault: IdeaControllerWriteFault) {
    D03_IDEA_CONTROLLER_WRITE_FAULT.with(|slot| slot.set(Some(fault)));
}

#[cfg(test)]
pub(crate) fn inject_d04_issue_link_write_fault(fault: IssueLinkWriteFault) {
    D04_ISSUE_LINK_WRITE_FAULT.with(|slot| slot.set(Some(fault)));
}

#[cfg(test)]
fn d02_fault_impl(fault: IdeaWriteFault) -> IdeaResult<()> {
    let injected = D02_IDEA_WRITE_FAULT.with(|slot| {
        if slot.get() == Some(fault) {
            slot.set(None);
            true
        } else {
            false
        }
    });
    if injected {
        return Err(IdeaControlError::StorageFailure(format!(
            "injected D02 fault at {fault:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
fn d03_fault_impl(fault: IdeaControllerWriteFault) -> IdeaResult<()> {
    let injected = D03_IDEA_CONTROLLER_WRITE_FAULT.with(|slot| {
        if slot.get() == Some(fault) {
            slot.set(None);
            true
        } else {
            false
        }
    });
    if injected {
        return Err(IdeaControlError::StorageFailure(format!(
            "injected D03 fault at {fault:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
fn d04_link_fault_impl(fault: IssueLinkWriteFault) -> IdeaResult<()> {
    let injected = D04_ISSUE_LINK_WRITE_FAULT.with(|slot| {
        if slot.get() == Some(fault) {
            slot.set(None);
            true
        } else {
            false
        }
    });
    if injected {
        return Err(IdeaControlError::StorageFailure(format!(
            "injected D04 link fault at {fault:?}"
        )));
    }
    Ok(())
}

/// Test-only transaction-seam injection for `SQLite`'s four stable integrity
/// identities. This enters the link transaction after `BEGIN IMMEDIATE`; the
/// synthetic `SQLite` message must never cross the RPC boundary.
#[cfg(test)]
fn d04_link_constraint_fault_impl() -> IdeaResult<()> {
    let extended_code = D04_ISSUE_LINK_WRITE_FAULT.with(|slot| match slot.get() {
        Some(IssueLinkWriteFault::ConstraintForeignKey) => {
            slot.set(None);
            Some(rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY)
        }
        Some(IssueLinkWriteFault::ConstraintCheck) => {
            slot.set(None);
            Some(rusqlite::ffi::SQLITE_CONSTRAINT_CHECK)
        }
        Some(IssueLinkWriteFault::ConstraintNotNull) => {
            slot.set(None);
            Some(rusqlite::ffi::SQLITE_CONSTRAINT_NOTNULL)
        }
        Some(IssueLinkWriteFault::ConstraintUniquePrimary) => {
            slot.set(None);
            Some(rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE)
        }
        _ => None,
    });
    extended_code.map_or_else(
        || Ok(()),
        |code| {
            Err(map_sql_error(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(code),
                Some("injected D04 link constraint must remain redacted".to_string()),
            )))
        },
    )
}

fn load_latest_controller_event_tx(
    tx: &Transaction<'_>,
    idea_id: Uuid,
) -> IdeaResult<Option<IdeaEvent>> {
    tx.query_row(
        &format!(
            "SELECT {IDEA_EVENT_COLUMNS}
             FROM idea_events
             WHERE idea_id = ?1
               AND event_type IN (
                   'controller_reserved',
                   'controller_assigned',
                   'controller_released'
               )
             ORDER BY sequence DESC
             LIMIT 1"
        ),
        [idea_id.to_string()],
        map_idea_event_row,
    )
    .optional()
    .map_err(map_sql_error)?
    .map(|row| row.into_idea_event().map_err(corrupt_event))
    .transpose()
}

fn load_latest_controller_event_connection(
    connection: &Connection,
    idea_id: Uuid,
) -> IdeaResult<Option<IdeaEvent>> {
    connection
        .query_row(
            &format!(
                "SELECT {IDEA_EVENT_COLUMNS}
                 FROM idea_events
                 WHERE idea_id = ?1
                   AND event_type IN (
                       'controller_reserved',
                       'controller_assigned',
                       'controller_released'
                   )
                 ORDER BY sequence DESC
                 LIMIT 1"
            ),
            [idea_id.to_string()],
            map_idea_event_row,
        )
        .optional()
        .map_err(map_sql_error)?
        .map(|row| row.into_idea_event().map_err(corrupt_event))
        .transpose()
}

fn current_unresolved_reservation_tx(
    tx: &Transaction<'_>,
    idea: &Idea,
) -> IdeaResult<Option<IdeaControllerReservationV1>> {
    let Some(event) = load_latest_controller_event_tx(tx, idea.id)? else {
        return Ok(None);
    };
    let payload = event
        .controller_control_payload_v1()
        .map_err(corrupt_event)?;
    if !controller_tail_matches_projection(&payload, idea) {
        return Err(corrupt_event(
            "latest controller event disagrees with assigned projection",
        ));
    }
    if matches!(
        payload.request.operation,
        IdeaControllerControlOperationV1::Reserve { .. }
    ) {
        return payload
            .outcome
            .reservation
            .ok_or_else(|| corrupt_event("controller reserve omitted reservation"))
            .map(Some);
    }
    Ok(None)
}

fn require_live_reservation_tail_tx(
    tx: &Transaction<'_>,
    idea: &Idea,
    reservation: &IdeaControllerReservationV1,
    now: DateTime<Utc>,
) -> IdeaResult<IdeaEvent> {
    let event = load_latest_controller_event_tx(tx, idea.id)?
        .ok_or(IdeaControlError::ControllerReservationNotFound)?;
    let payload = event
        .controller_control_payload_v1()
        .map_err(corrupt_event)?;
    if !controller_tail_matches_projection(&payload, idea) {
        return Err(corrupt_event(
            "latest controller event disagrees with assigned projection",
        ));
    }
    let current = match payload.request.operation {
        IdeaControllerControlOperationV1::Reserve { .. } => payload.outcome.reservation,
        _ => None,
    }
    .ok_or(IdeaControlError::ControllerReservationResolved)?;
    if current != *reservation {
        return Err(IdeaControlError::ControllerReservationNotFound);
    }
    if !current.is_live_at(now) {
        return Err(IdeaControlError::ControllerReservationExpired);
    }
    Ok(event)
}

fn controller_tail_matches_projection(
    payload: &IdeaControllerEventPayloadV1,
    current: &Idea,
) -> bool {
    if payload.request.project_id != current.project_id
        || payload.request.idea_id != current.id
        || payload.outcome.idea.project_id != current.project_id
        || payload.outcome.idea.id != current.id
    {
        return false;
    }
    match &payload.request.operation {
        IdeaControllerControlOperationV1::Reserve {
            base_controller_session_id,
            base_controller_epoch,
            ..
        }
        | IdeaControllerControlOperationV1::ReleaseReservation {
            reservation:
                IdeaControllerReservationV1 {
                    base_controller_session_id,
                    base_controller_epoch,
                    ..
                },
            ..
        } => {
            current.current_controller_session_id == *base_controller_session_id
                && current.controller_epoch == *base_controller_epoch
        }
        IdeaControllerControlOperationV1::Assign { reservation, .. } => {
            current.current_controller_session_id == Some(reservation.candidate_session_id)
                && current.controller_epoch == reservation.proposed_epoch
        }
        IdeaControllerControlOperationV1::ReleaseAssigned {
            controller_epoch, ..
        } => {
            current.current_controller_session_id.is_none()
                && current.controller_epoch == *controller_epoch
        }
    }
}

fn require_confirmed_session_row_tx(
    tx: &Transaction<'_>,
    reservation: &IdeaControllerReservationV1,
    confirmation: &IdeaControllerLaunchConfirmationV1,
) -> IdeaResult<()> {
    let row = tx
        .query_row(
            "SELECT project_id, provider, status, model_invocation_id
             FROM sessions WHERE id = ?1",
            [reservation.candidate_session_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .optional()
        .map_err(map_sql_error)?
        .ok_or(IdeaControlError::LaunchNotConfirmed)?;
    let project_id = row
        .0
        .as_deref()
        .and_then(|value| Uuid::parse_str(value).ok())
        .ok_or(IdeaControlError::LaunchNotConfirmed)?;
    let provider =
        str_to_session_provider(&row.1).map_err(|_| IdeaControlError::LaunchNotConfirmed)?;
    let status = str_to_session_status(&row.2).map_err(|_| IdeaControlError::LaunchNotConfirmed)?;
    let model_invocation_id = row
        .3
        .as_deref()
        .and_then(|value| Uuid::parse_str(value).ok())
        .ok_or(IdeaControlError::LaunchNotConfirmed)?;
    if project_id != confirmation.project_id
        || provider != confirmation.provider
        || model_invocation_id != confirmation.admission_invocation_id
        || !matches!(
            status,
            SessionStatus::Starting | SessionStatus::Running | SessionStatus::WaitingApproval
        )
    {
        return Err(IdeaControlError::LaunchNotConfirmed);
    }
    let invocation_is_live = tx
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM model_invocations
                 WHERE id = ?1
                   AND session_id = ?2
                   AND project_id = ?3
                   AND admission_status = 'admitted'
                   AND status = 'running'
             )",
            params![
                confirmation.admission_invocation_id.to_string(),
                reservation.candidate_session_id.to_string(),
                confirmation.project_id.to_string(),
            ],
            |row| row.get::<_, bool>(0),
        )
        .map_err(map_sql_error)?;
    if !invocation_is_live {
        return Err(IdeaControlError::LaunchNotConfirmed);
    }
    Ok(())
}

fn update_idea_controller_projection_tx(
    tx: &Transaction<'_>,
    idea: &Idea,
    new_controller_session_id: Option<Uuid>,
    new_controller_epoch: i64,
    expected_version: i64,
    expected_sequence: i64,
    timestamp: DateTime<Utc>,
) -> IdeaResult<usize> {
    tx.execute(
        "UPDATE ideas
         SET current_controller_session_id = ?1,
             controller_epoch = ?2,
             row_version = row_version + 1,
             next_event_sequence = next_event_sequence + 1,
             updated_at = ?3
         WHERE id = ?4 AND project_id = ?5
           AND current_controller_session_id IS ?6
           AND controller_epoch = ?7
           AND row_version = ?8
           AND next_event_sequence = ?9",
        params![
            new_controller_session_id.map(|id| id.to_string()),
            new_controller_epoch,
            canonical_timestamp(timestamp),
            idea.id.to_string(),
            idea.project_id.to_string(),
            idea.current_controller_session_id.map(|id| id.to_string()),
            idea.controller_epoch,
            expected_version,
            expected_sequence,
        ],
    )
    .map_err(map_sql_error)
}

fn advance_idea_counters(idea: &mut Idea, timestamp: DateTime<Utc>) -> IdeaResult<()> {
    idea.row_version = idea
        .row_version
        .checked_add(1)
        .ok_or(IdeaControlError::ControllerEpochExhausted)?;
    idea.next_event_sequence = idea
        .next_event_sequence
        .checked_add(1)
        .ok_or(IdeaControlError::ControllerEpochExhausted)?;
    idea.updated_at = timestamp;
    Ok(())
}

fn validate_reservation_base_v1(
    idea: &Idea,
    authority: &BoundIdeaControllerTransferAuthority,
    request: &ReserveIdeaControllerRequestV1,
    post_expiry_base_row_version: Option<i64>,
) -> IdeaResult<()> {
    let base_row_version = post_expiry_base_row_version.unwrap_or(request.expected_row_version);
    if idea.row_version != base_row_version {
        return Err(IdeaControlError::StaleVersion {
            expected: base_row_version,
            actual: idea.row_version,
        });
    }
    if !matches!(
        request.expected_row_version.checked_add(1),
        Some(after_expiry_release)
            if base_row_version == request.expected_row_version
                || base_row_version == after_expiry_release
    ) {
        return Err(IdeaControlError::StaleVersion {
            expected: request.expected_row_version,
            actual: base_row_version,
        });
    }
    if idea.controller_epoch != authority.observed_controller_epoch() {
        return Err(IdeaControlError::StaleControllerEpoch {
            expected: authority.observed_controller_epoch(),
            actual: idea.controller_epoch,
        });
    }
    Ok(())
}

fn prepare_controller_reservation_v1(
    authority: &BoundIdeaControllerTransferAuthority,
    request: &ReserveIdeaControllerRequestV1,
    idea: &Idea,
    now: DateTime<Utc>,
    identity: &ControllerReservationIdentityV1,
) -> IdeaResult<PreparedControllerReservationV1> {
    let proposed_epoch = idea
        .controller_epoch
        .checked_add(1)
        .ok_or(IdeaControlError::ControllerEpochExhausted)?;
    let expires_at = now
        .checked_add_signed(chrono::Duration::seconds(
            IDEA_CONTROLLER_RESERVATION_LEASE_SECONDS,
        ))
        .ok_or(IdeaControlError::ControllerEpochExhausted)?;
    let sequence = idea.next_event_sequence;
    let reservation = IdeaControllerReservationV1 {
        reservation_id: identity.reservation_id,
        candidate_session_id: identity.candidate_session_id,
        base_controller_session_id: idea.current_controller_session_id,
        base_controller_epoch: idea.controller_epoch,
        base_row_version: idea.row_version,
        proposed_epoch,
        reserved_event_id: identity.event_id,
        reserved_sequence: sequence,
        reserved_at: now,
        expires_at,
        transfer_intent_key: request.transfer_intent_key.clone(),
    };
    reservation
        .validate(idea.id)
        .map_err(IdeaControlError::InvalidRequest)?;

    let mut committed_idea = idea.clone();
    advance_idea_counters(&mut committed_idea, now)?;
    let semantic_request = IdeaControllerControlRequestV1 {
        domain: IDEA_CONTROLLER_CONTROL_V1.to_string(),
        project_id: authority.project_id(),
        idea_id: authority.idea_id(),
        actor_kind: authority.actor_kind(),
        actor_id: authority.actor_id().to_string(),
        operation: IdeaControllerControlOperationV1::Reserve {
            transfer_intent_key: request.transfer_intent_key.clone(),
            expected_row_version: request.expected_row_version,
            observed_controller_epoch: authority.observed_controller_epoch(),
            base_controller_session_id: idea.current_controller_session_id,
            base_controller_epoch: idea.controller_epoch,
            base_row_version: idea.row_version,
            reservation_id: identity.reservation_id,
            candidate_session_id: identity.candidate_session_id,
            proposed_epoch,
            lease_seconds: IDEA_CONTROLLER_RESERVATION_LEASE_SECONDS,
        },
    };
    let payload = IdeaControllerEventPayloadV1 {
        request: semantic_request,
        outcome: IdeaControllerControlOutcomeV1 {
            idea: committed_idea.clone(),
            event_id: identity.event_id,
            sequence,
            resulting_row_version: committed_idea.row_version,
            occurred_at: now,
            reservation: Some(reservation.clone()),
        },
    };
    let event = build_controller_control_event(&payload, &identity.stage_key)?;
    Ok(PreparedControllerReservationV1 {
        committed_idea,
        event,
        payload,
        reservation,
    })
}

fn stale_version_after_failed_cas<T>(
    tx: &Transaction<'_>,
    idea: &Idea,
    expected: i64,
) -> IdeaResult<T> {
    let actual = load_idea_for_project_tx(tx, idea.project_id, idea.id)?
        .map_or(expected, |current| current.row_version);
    Err(IdeaControlError::StaleVersion { expected, actual })
}

fn build_controller_control_event(
    payload: &IdeaControllerEventPayloadV1,
    stage_key: &str,
) -> IdeaResult<IdeaEvent> {
    let operation = &payload.request.operation;
    let event = IdeaEvent {
        id: payload.outcome.event_id,
        project_id: payload.request.project_id,
        idea_id: payload.request.idea_id,
        sequence: payload.outcome.sequence,
        event_type: operation.event_type(),
        actor_kind: payload.request.actor_kind,
        actor_id: payload.request.actor_id.clone(),
        controller_session_id: operation.controller_session_id(),
        controller_epoch: Some(operation.controller_epoch()),
        expected_row_version: operation
            .expected_row_version()
            .map_err(IdeaControlError::InvalidRequest)?,
        resulting_row_version: payload.outcome.resulting_row_version,
        idempotency_key: stage_key.to_string(),
        occurred_at: payload.outcome.occurred_at,
        payload: serde_json::to_value(payload)
            .map_err(|error| IdeaControlError::InvalidRequest(error.to_string()))?,
        artifact_digests: Vec::new(),
        evidence_digests: Vec::new(),
    };
    event
        .controller_control_payload_v1()
        .map_err(IdeaControlError::InvalidRequest)?;
    Ok(event)
}

fn build_controller_mutation_event(
    payload: &IdeaControllerMutationPayloadV1,
    idempotency_key: &str,
    canonical_payload: &str,
) -> IdeaResult<IdeaEvent> {
    let request = &payload.request;
    let event = IdeaEvent {
        id: payload.outcome.event_id,
        project_id: request.project_id,
        idea_id: request.idea_id,
        sequence: payload.outcome.sequence,
        event_type: request.action.event_type(),
        actor_kind: IdeaActorKind::Session,
        actor_id: request.controller_session_id.to_string(),
        controller_session_id: Some(request.controller_session_id),
        controller_epoch: Some(request.controller_epoch),
        expected_row_version: request.expected_row_version,
        resulting_row_version: payload.outcome.resulting_row_version,
        idempotency_key: idempotency_key.to_string(),
        occurred_at: payload.outcome.occurred_at,
        payload: serde_json::from_str(canonical_payload)
            .map_err(|error| IdeaControlError::InvalidRequest(error.to_string()))?,
        artifact_digests: request.artifact_digests.clone(),
        evidence_digests: request.evidence_digests.clone(),
    };
    event
        .controller_mutation_payload_v1()
        .map_err(IdeaControlError::InvalidRequest)?;
    Ok(event)
}

fn insert_controller_event_tx(
    tx: &Transaction<'_>,
    event: &IdeaEvent,
    canonical_json: &str,
) -> IdeaResult<()> {
    let result = tx.execute(
        "INSERT INTO idea_events (
            id, project_id, idea_id, sequence, event_type, actor_kind, actor_id,
            controller_session_id, controller_epoch, expected_row_version,
            resulting_row_version, idempotency_key, occurred_at, payload_json,
            artifact_digests_json, evidence_digests_json
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16
         )",
        params![
            event.id.to_string(),
            event.project_id.to_string(),
            event.idea_id.to_string(),
            event.sequence,
            event.event_type.as_str(),
            event.actor_kind.as_str(),
            event.actor_id,
            event.controller_session_id.map(|id| id.to_string()),
            event.controller_epoch,
            event.expected_row_version,
            event.resulting_row_version,
            event.idempotency_key,
            canonical_timestamp(event.occurred_at),
            canonical_json,
            serde_json::to_string(&event.artifact_digests)
                .map_err(|error| IdeaControlError::StorageFailure(error.to_string()))?,
            serde_json::to_string(&event.evidence_digests)
                .map_err(|error| IdeaControlError::StorageFailure(error.to_string()))?,
        ],
    );
    match result {
        Ok(_) => Ok(()),
        Err(error) if is_unique_or_primary_constraint(&error) => {
            let by_key = load_event_by_key_tx(tx, event.idea_id, &event.idempotency_key)?;
            let by_id = tx
                .query_row(
                    &format!("SELECT {IDEA_EVENT_COLUMNS} FROM idea_events WHERE id = ?1"),
                    [event.id.to_string()],
                    map_idea_event_row,
                )
                .optional()
                .map_err(map_sql_error)?
                .map(|row| row.into_idea_event().map_err(corrupt_event))
                .transpose()?;
            if by_key.is_some() || by_id.is_some() {
                Err(IdeaControlError::IdempotencyConflict)
            } else {
                Err(map_sql_error(error))
            }
        }
        Err(error) => Err(map_sql_error(error)),
    }
}

fn controller_control_result(
    event: IdeaEvent,
    payload: IdeaControllerEventPayloadV1,
    deduplicated: bool,
) -> IdeaControllerControlResultV1 {
    IdeaControllerControlResultV1 {
        idea: payload.outcome.idea,
        reservation: payload.outcome.reservation,
        event,
        deduplicated,
    }
}

fn load_controller_control_replay_tx(
    tx: &Transaction<'_>,
    requested: &IdeaControllerControlRequestV1,
    stage_key: &str,
) -> IdeaResult<Option<IdeaControllerControlResultV1>> {
    let Some(event) = load_event_by_key_tx(tx, requested.idea_id, stage_key)? else {
        return Ok(None);
    };
    let payload = event
        .controller_control_payload_v1()
        .map_err(corrupt_event)?;
    if payload.request != *requested {
        return Err(IdeaControlError::IdempotencyConflict);
    }
    Ok(Some(controller_control_result(event, payload, true)))
}

fn load_reserve_replay_tx(
    tx: &Transaction<'_>,
    authority: &BoundIdeaControllerTransferAuthority,
    requested: &ReserveIdeaControllerRequestV1,
    stage_key: &str,
) -> IdeaResult<Option<IdeaControllerControlResultV1>> {
    let Some(event) = load_event_by_key_tx(tx, authority.idea_id(), stage_key)? else {
        return Ok(None);
    };
    let payload = event
        .controller_control_payload_v1()
        .map_err(corrupt_event)?;
    let exact = payload.request.project_id == authority.project_id()
        && payload.request.idea_id == authority.idea_id()
        && payload.request.actor_kind == authority.actor_kind()
        && payload.request.actor_id == authority.actor_id()
        && matches!(
            &payload.request.operation,
            IdeaControllerControlOperationV1::Reserve {
                transfer_intent_key,
                expected_row_version,
                observed_controller_epoch,
                lease_seconds,
                ..
            } if transfer_intent_key == &requested.transfer_intent_key
                && *expected_row_version == requested.expected_row_version
                && *observed_controller_epoch == authority.observed_controller_epoch()
                && *lease_seconds == IDEA_CONTROLLER_RESERVATION_LEASE_SECONDS
        );
    if !exact {
        return Err(IdeaControlError::IdempotencyConflict);
    }
    Ok(Some(controller_control_result(event, payload, true)))
}

fn load_reservation_release_replay_tx(
    tx: &Transaction<'_>,
    authority: &BoundIdeaControllerTransferAuthority,
    requested: &ReleaseIdeaControllerReservationRequestV1,
    stage_key: &str,
) -> IdeaResult<Option<IdeaControllerControlResultV1>> {
    let Some(event) = load_event_by_key_tx(tx, authority.idea_id(), stage_key)? else {
        return Ok(None);
    };
    let payload = event
        .controller_control_payload_v1()
        .map_err(corrupt_event)?;
    let exact = payload.request.project_id == authority.project_id()
        && payload.request.idea_id == authority.idea_id()
        && payload.request.actor_kind == authority.actor_kind()
        && payload.request.actor_id == authority.actor_id()
        && matches!(
            &payload.request.operation,
            IdeaControllerControlOperationV1::ReleaseReservation {
                reservation,
                reason,
                ..
            } if reservation.transfer_intent_key == requested.transfer_intent_key
                && *reason == requested.reason
        );
    if !exact {
        return Err(IdeaControlError::IdempotencyConflict);
    }
    Ok(Some(controller_control_result(event, payload, true)))
}

fn load_assigned_release_replay_tx(
    tx: &Transaction<'_>,
    authority: &AssignedReleaseAuthority<'_>,
    requested: &ReleaseAssignedIdeaControllerRequestV1,
    stage_key: &str,
) -> IdeaResult<Option<IdeaControllerControlResultV1>> {
    let Some(event) = load_event_by_key_tx(tx, authority.idea_id, stage_key)? else {
        return Ok(None);
    };
    let payload = event
        .controller_control_payload_v1()
        .map_err(corrupt_event)?;
    let exact = payload.request.project_id == authority.project_id
        && payload.request.idea_id == authority.idea_id
        && payload.request.actor_kind == authority.actor_kind
        && payload.request.actor_id == authority.actor_id
        && matches!(
            &payload.request.operation,
            IdeaControllerControlOperationV1::ReleaseAssigned {
                release_intent_key,
                expected_row_version,
                observed_controller_epoch,
                reason,
                ..
            } if release_intent_key == &requested.release_intent_key
                && *expected_row_version == requested.expected_row_version
                && *observed_controller_epoch == authority.observed_epoch
                && *reason == requested.reason
        );
    if !exact {
        return Err(IdeaControlError::IdempotencyConflict);
    }
    Ok(Some(controller_control_result(event, payload, true)))
}

fn load_controller_mutation_replay_tx(
    tx: &Transaction<'_>,
    requested: &IdeaControllerMutationRequestV1,
    idempotency_key: &str,
) -> IdeaResult<Option<IdeaControllerMutationResultV1>> {
    let Some(event) = load_event_by_key_tx(tx, requested.idea_id, idempotency_key)? else {
        return Ok(None);
    };
    let payload = event
        .controller_mutation_payload_v1()
        .map_err(corrupt_event)?;
    if payload.request != *requested {
        return Err(IdeaControlError::IdempotencyConflict);
    }
    Ok(Some(IdeaControllerMutationResultV1 {
        idea: payload.outcome.idea,
        event,
        deduplicated: true,
    }))
}

fn validate_authority(authority: &BoundIdeaWriteAuthority) -> IdeaResult<()> {
    if authority.actor_kind() != IdeaActorKind::Operator {
        return Err(IdeaControlError::ForbiddenActor);
    }
    rsi_common::types::validate_idea_actor_id(authority.actor_id())
        .map_err(IdeaControlError::InvalidRequest)
}

fn deterministic_idea_id(project_id: Uuid, key: &str) -> Uuid {
    Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!("rsi.idea/v1/{project_id}/{key}").as_bytes(),
    )
}

fn deterministic_event_id(idea_id: Uuid, key: &str) -> Uuid {
    Uuid::new_v5(&idea_id, format!("rsi.idea.event/v1/{key}").as_bytes())
}

fn deterministic_relationship_id(
    event_id: Uuid,
    kind: IdeaRelationshipKind,
    target_id: Uuid,
) -> Uuid {
    Uuid::new_v5(
        &event_id,
        format!("rsi.idea.relationship/v1/{}/{target_id}", kind.as_str()).as_bytes(),
    )
}

fn immediate_transaction(connection: &Connection) -> IdeaResult<Transaction<'_>> {
    Transaction::new_unchecked(connection, TransactionBehavior::Immediate).map_err(map_sql_error)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SqliteFailureClass {
    Contention,
    UniqueOrPrimary,
    ForeignKey,
    Check,
    NotNull,
    Storage,
}

pub(super) fn classify_sqlite_error(error: &rusqlite::Error) -> SqliteFailureClass {
    let Some(sqlite_error) = error.sqlite_error() else {
        return SqliteFailureClass::Storage;
    };
    if matches!(
        sqlite_error.code,
        rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
    ) {
        return SqliteFailureClass::Contention;
    }
    if matches!(
        sqlite_error.extended_code,
        rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE | rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY
    ) {
        return SqliteFailureClass::UniqueOrPrimary;
    }
    if sqlite_error.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY {
        return SqliteFailureClass::ForeignKey;
    }
    if sqlite_error.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_CHECK {
        return SqliteFailureClass::Check;
    }
    if sqlite_error.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_NOTNULL {
        return SqliteFailureClass::NotNull;
    }
    SqliteFailureClass::Storage
}

pub(super) fn map_sql_error(error: rusqlite::Error) -> IdeaControlError {
    match (classify_sqlite_error(&error), error) {
        (SqliteFailureClass::Contention, _) => IdeaControlError::Contention,
        (SqliteFailureClass::UniqueOrPrimary, _) => IdeaControlError::ConstraintViolation {
            class: IssueLinkConstraintClass::UniquePrimary,
        },
        (SqliteFailureClass::ForeignKey, _) => IdeaControlError::ConstraintViolation {
            class: IssueLinkConstraintClass::ForeignKey,
        },
        (SqliteFailureClass::Check, _) => IdeaControlError::ConstraintViolation {
            class: IssueLinkConstraintClass::Check,
        },
        (SqliteFailureClass::NotNull, _) => IdeaControlError::ConstraintViolation {
            class: IssueLinkConstraintClass::NotNull,
        },
        (SqliteFailureClass::Storage, error) => IdeaControlError::StorageFailure(error.to_string()),
    }
}

fn is_unique_or_primary_constraint(error: &rusqlite::Error) -> bool {
    classify_sqlite_error(error) == SqliteFailureClass::UniqueOrPrimary
}

fn corrupt_event(error: impl std::fmt::Display) -> IdeaControlError {
    IdeaControlError::CorruptStoredEvent(error.to_string())
}

fn validate_strict_idea_event(event: &IdeaEvent) -> IdeaResult<()> {
    if matches!(
        event.event_type,
        IdeaEventType::ControllerReserved
            | IdeaEventType::ControllerAssigned
            | IdeaEventType::ControllerReleased
    ) {
        event
            .controller_control_payload_v1()
            .map_err(corrupt_event)?;
    } else if event
        .payload
        .get("request")
        .and_then(|request| request.get("domain"))
        .and_then(serde_json::Value::as_str)
        == Some(rsi_common::types::IDEA_CONTROLLER_MUTATION_V1)
    {
        event
            .controller_mutation_payload_v1()
            .map_err(corrupt_event)?;
    } else {
        event.semantic_request_v1().map_err(corrupt_event)?;
    }
    Ok(())
}

fn canonical_timestamp(timestamp: DateTime<Utc>) -> String {
    timestamp.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn require_capture_tx(tx: &Transaction<'_>, project_id: Uuid, capture_id: Uuid) -> IdeaResult<()> {
    let capture = tx
        .query_row(
            &format!(
                "SELECT {CAPTURE_COLUMNS}
                 FROM captures WHERE id = ?1 AND project_id = ?2"
            ),
            params![capture_id.to_string(), project_id.to_string()],
            map_capture_row,
        )
        .optional()
        .map_err(map_sql_error)?;
    if let Some(capture) = capture {
        capture.into_capture().map_err(|error| {
            IdeaControlError::StorageFailure(format!("invalid stored Capture: {error}"))
        })?;
        return Ok(());
    }
    let exists = tx
        .query_row(
            "SELECT 1 FROM captures WHERE id = ?1",
            [capture_id.to_string()],
            |_| Ok(()),
        )
        .optional()
        .map_err(map_sql_error)?
        .is_some();
    Err(if exists {
        IdeaControlError::ProjectScopeMismatch
    } else {
        IdeaControlError::CaptureNotFound
    })
}

fn load_idea_by_id_tx(tx: &Transaction<'_>, idea_id: Uuid) -> IdeaResult<Option<Idea>> {
    tx.query_row(
        &format!("SELECT {IDEA_COLUMNS} FROM ideas WHERE id = ?1"),
        [idea_id.to_string()],
        map_idea_row,
    )
    .optional()
    .map_err(map_sql_error)?
    .map(|row| {
        row.into_idea().map_err(|error| {
            IdeaControlError::StorageFailure(format!("invalid stored Idea: {error}"))
        })
    })
    .transpose()
}

fn load_idea_for_project_tx(
    tx: &Transaction<'_>,
    project_id: Uuid,
    idea_id: Uuid,
) -> IdeaResult<Option<Idea>> {
    tx.query_row(
        &format!(
            "SELECT {IDEA_COLUMNS}
             FROM ideas WHERE id = ?1 AND project_id = ?2"
        ),
        params![idea_id.to_string(), project_id.to_string()],
        map_idea_row,
    )
    .optional()
    .map_err(map_sql_error)?
    .map(|row| {
        row.into_idea().map_err(|error| {
            IdeaControlError::StorageFailure(format!("invalid stored Idea: {error}"))
        })
    })
    .transpose()
}

fn require_same_project_idea_tx(
    tx: &Transaction<'_>,
    project_id: Uuid,
    idea_id: Uuid,
) -> IdeaResult<Idea> {
    if let Some(idea) = load_idea_for_project_tx(tx, project_id, idea_id)? {
        return Ok(idea);
    }
    Err(if load_idea_by_id_tx(tx, idea_id)?.is_some() {
        IdeaControlError::ProjectScopeMismatch
    } else {
        IdeaControlError::IdeaNotFound
    })
}

fn require_same_project_idea_connection(
    connection: &Connection,
    project_id: Uuid,
    idea_id: Uuid,
) -> IdeaResult<Idea> {
    let idea = connection
        .query_row(
            &format!(
                "SELECT {IDEA_COLUMNS}
                 FROM ideas WHERE id = ?1 AND project_id = ?2"
            ),
            params![idea_id.to_string(), project_id.to_string()],
            map_idea_row,
        )
        .optional()
        .map_err(map_sql_error)?;
    if let Some(idea) = idea {
        return idea.into_idea().map_err(|error| {
            IdeaControlError::StorageFailure(format!("invalid stored Idea: {error}"))
        });
    }
    let exists = connection
        .query_row(
            "SELECT 1 FROM ideas WHERE id = ?1",
            [idea_id.to_string()],
            |_| Ok(()),
        )
        .optional()
        .map_err(map_sql_error)?
        .is_some();
    Err(if exists {
        IdeaControlError::ProjectScopeMismatch
    } else {
        IdeaControlError::IdeaNotFound
    })
}

fn insert_idea_tx(tx: &Transaction<'_>, idea: &Idea) -> IdeaResult<()> {
    let result = tx.execute(
        "INSERT INTO ideas (
            id, project_id, slug, sigil, genesis_capture_id, genesis_span_start,
            genesis_span_end, genesis_span_digest, title, description, portfolio_summary,
            lifecycle, stage, priority, autonomy_policy, integration_target_ref,
            program_template_policy_id, current_controller_session_id, controller_epoch,
            row_version, next_event_sequence, created_at, updated_at, terminal_at,
            superseded_at
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
            ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25
         )",
        params![
            idea.id.to_string(),
            idea.project_id.to_string(),
            idea.slug,
            idea.sigil,
            idea.genesis_capture_id.to_string(),
            idea.genesis_span_start,
            idea.genesis_span_end,
            idea.genesis_span_digest.as_ref().map(ToString::to_string),
            idea.title,
            idea.description,
            idea.portfolio_summary,
            idea.lifecycle.as_str(),
            idea.stage.as_str(),
            idea.priority,
            idea.autonomy_policy.as_str(),
            idea.integration_target_ref,
            idea.program_template_policy_id,
            idea.current_controller_session_id.map(|id| id.to_string()),
            idea.controller_epoch,
            idea.row_version,
            idea.next_event_sequence,
            canonical_timestamp(idea.created_at),
            canonical_timestamp(idea.updated_at),
            idea.terminal_at.map(canonical_timestamp),
            idea.superseded_at.map(canonical_timestamp),
        ],
    );
    match result {
        Ok(_) => Ok(()),
        Err(error) if is_unique_or_primary_constraint(&error) => {
            if load_idea_by_id_tx(tx, idea.id)?.is_some() {
                return Err(IdeaControlError::IdempotencyConflict);
            }
            let slug_exists = tx
                .query_row(
                    "SELECT 1 FROM ideas WHERE project_id = ?1 AND slug = ?2",
                    params![idea.project_id.to_string(), idea.slug],
                    |_| Ok(()),
                )
                .optional()
                .map_err(map_sql_error)?
                .is_some();
            if slug_exists {
                Err(IdeaControlError::InvalidRequest(
                    "Idea slug already exists in the bound project".to_string(),
                ))
            } else {
                Err(map_sql_error(error))
            }
        }
        Err(error) => Err(map_sql_error(error)),
    }
}

fn build_event(
    event_id: Uuid,
    envelope: &IdeaSemanticRequestV1,
    idempotency_key: &str,
    sequence: i64,
    resulting_version: i64,
    timestamp: DateTime<Utc>,
    canonical_json: &str,
) -> IdeaResult<IdeaEvent> {
    let payload = serde_json::from_str(canonical_json)
        .map_err(|error| IdeaControlError::InvalidRequest(error.to_string()))?;
    let event = IdeaEvent {
        id: event_id,
        project_id: envelope.project_id,
        idea_id: envelope.idea_id,
        sequence,
        event_type: envelope.event_type(),
        actor_kind: envelope.actor_kind,
        actor_id: envelope.actor_id.clone(),
        controller_session_id: None,
        controller_epoch: None,
        expected_row_version: envelope.expected_row_version,
        resulting_row_version: resulting_version,
        idempotency_key: idempotency_key.to_string(),
        occurred_at: timestamp,
        payload,
        artifact_digests: envelope.artifact_digests.clone(),
        evidence_digests: envelope.evidence_digests.clone(),
    };
    event
        .semantic_request_v1()
        .map_err(IdeaControlError::InvalidRequest)?;
    Ok(event)
}

fn insert_event_tx(
    tx: &Transaction<'_>,
    event: &IdeaEvent,
    canonical_json: &str,
) -> IdeaResult<()> {
    let result = tx.execute(
        "INSERT INTO idea_events (
            id, project_id, idea_id, sequence, event_type, actor_kind, actor_id,
            controller_session_id, controller_epoch, expected_row_version,
            resulting_row_version, idempotency_key, occurred_at, payload_json,
            artifact_digests_json, evidence_digests_json
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, NULL, ?8, ?9, ?10, ?11, ?12, ?13, ?14
         )",
        params![
            event.id.to_string(),
            event.project_id.to_string(),
            event.idea_id.to_string(),
            event.sequence,
            event.event_type.as_str(),
            event.actor_kind.as_str(),
            event.actor_id,
            event.expected_row_version,
            event.resulting_row_version,
            event.idempotency_key,
            canonical_timestamp(event.occurred_at),
            canonical_json,
            serde_json::to_string(&event.artifact_digests)
                .map_err(|error| IdeaControlError::StorageFailure(error.to_string()))?,
            serde_json::to_string(&event.evidence_digests)
                .map_err(|error| IdeaControlError::StorageFailure(error.to_string()))?,
        ],
    );
    match result {
        Ok(_) => Ok(()),
        Err(error) if is_unique_or_primary_constraint(&error) => {
            let by_key = load_event_by_key_tx(tx, event.idea_id, &event.idempotency_key)?.is_some();
            let by_id = tx
                .query_row(
                    "SELECT 1 FROM idea_events WHERE id = ?1",
                    [event.id.to_string()],
                    |_| Ok(()),
                )
                .optional()
                .map_err(map_sql_error)?
                .is_some();
            if by_key || by_id {
                Err(IdeaControlError::IdempotencyConflict)
            } else {
                Err(map_sql_error(error))
            }
        }
        Err(error) => Err(map_sql_error(error)),
    }
}

fn build_relationship(
    event_id: Uuid,
    project_id: Uuid,
    source_id: Uuid,
    target_id: Uuid,
    kind: IdeaRelationshipKind,
    timestamp: DateTime<Utc>,
) -> IdeaRelationship {
    IdeaRelationship {
        id: deterministic_relationship_id(event_id, kind, target_id),
        project_id,
        source_idea_id: source_id,
        target_idea_id: target_id,
        kind,
        created_event_id: event_id,
        created_at: timestamp,
        removed_event_id: None,
        removed_at: None,
    }
}

fn insert_relationship_tx(tx: &Transaction<'_>, relationship: &IdeaRelationship) -> IdeaResult<()> {
    let result = tx.execute(
        "INSERT INTO idea_relationships (
            id, project_id, source_idea_id, target_idea_id, kind,
            created_event_id, created_at, removed_event_id, removed_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, NULL)",
        params![
            relationship.id.to_string(),
            relationship.project_id.to_string(),
            relationship.source_idea_id.to_string(),
            relationship.target_idea_id.to_string(),
            relationship.kind.as_str(),
            relationship.created_event_id.to_string(),
            canonical_timestamp(relationship.created_at),
        ],
    );
    match result {
        Ok(_) => Ok(()),
        Err(error) if is_unique_or_primary_constraint(&error) => {
            let exact_id = tx
                .query_row(
                    "SELECT 1 FROM idea_relationships WHERE id = ?1",
                    [relationship.id.to_string()],
                    |_| Ok(()),
                )
                .optional()
                .map_err(map_sql_error)?
                .is_some();
            let active_edge = tx
                .query_row(
                    "SELECT 1 FROM idea_relationships
                     WHERE source_idea_id = ?1 AND target_idea_id = ?2
                       AND kind = ?3 AND removed_at IS NULL",
                    params![
                        relationship.source_idea_id.to_string(),
                        relationship.target_idea_id.to_string(),
                        relationship.kind.as_str(),
                    ],
                    |_| Ok(()),
                )
                .optional()
                .map_err(map_sql_error)?
                .is_some();
            if exact_id || active_edge {
                Err(IdeaControlError::RelationshipConflict(
                    "relationship identity or active edge already exists".to_string(),
                ))
            } else {
                Err(map_sql_error(error))
            }
        }
        Err(error) => Err(map_sql_error(error)),
    }
}

fn load_event_by_key_tx(
    tx: &Transaction<'_>,
    idea_id: Uuid,
    idempotency_key: &str,
) -> IdeaResult<Option<IdeaEvent>> {
    tx.query_row(
        &format!(
            "SELECT {IDEA_EVENT_COLUMNS}
             FROM idea_events WHERE idea_id = ?1 AND idempotency_key = ?2"
        ),
        params![idea_id.to_string(), idempotency_key],
        map_idea_event_row,
    )
    .optional()
    .map_err(map_sql_error)?
    .map(|row| row.into_idea_event().map_err(corrupt_event))
    .transpose()
}

fn load_exact_replay_tx(
    tx: &Transaction<'_>,
    requested: &IdeaSemanticRequestV1,
    idempotency_key: &str,
) -> IdeaResult<Option<IdeaMutationResultV1>> {
    let Some(event) = load_event_by_key_tx(tx, requested.idea_id, idempotency_key)? else {
        return Ok(None);
    };
    let stored = event.semantic_request_v1().map_err(corrupt_event)?;
    if stored != *requested {
        return Err(IdeaControlError::IdempotencyConflict);
    }
    let idea = reconstruct_projection_at_tx(tx, &event)?;
    let relationships = load_relationships_for_event_tx(tx, &event)?;
    Ok(Some(IdeaMutationResultV1 {
        idea,
        event,
        relationships,
        deduplicated: true,
    }))
}

fn load_relationships_for_event_tx(
    tx: &Transaction<'_>,
    event: &IdeaEvent,
) -> IdeaResult<Vec<IdeaRelationship>> {
    let mut statement = tx
        .prepare(&format!(
            "SELECT {IDEA_RELATIONSHIP_COLUMNS}
             FROM idea_relationships
             WHERE created_event_id = ?1 OR removed_event_id = ?1
             ORDER BY id"
        ))
        .map_err(map_sql_error)?;
    let rows = statement
        .query_map([event.id.to_string()], map_idea_relationship_row)
        .map_err(map_sql_error)?;
    let mut relationships = Vec::new();
    for row in rows {
        let mut relationship = row
            .map_err(map_sql_error)?
            .into_idea_relationship()
            .map_err(corrupt_event)?;
        if relationship.created_event_id == event.id {
            relationship.removed_event_id = None;
            relationship.removed_at = None;
        }
        relationships.push(relationship);
    }
    Ok(relationships)
}

fn load_replay_event_batch_tx(
    tx: &Transaction<'_>,
    target_event: &IdeaEvent,
    cursor: i64,
) -> IdeaResult<Vec<IdeaEvent>> {
    let limit = i64::try_from(IDEA_EVENT_PAGE_MAX_LIMIT)
        .map_err(|_| corrupt_event("history page bound is out of range"))?;
    let mut statement = tx
        .prepare(&format!(
            "SELECT {IDEA_EVENT_COLUMNS}
             FROM idea_events
             WHERE project_id = ?1 AND idea_id = ?2
               AND sequence > ?3 AND sequence <= ?4
             ORDER BY sequence ASC
             LIMIT ?5"
        ))
        .map_err(map_sql_error)?;
    let rows = statement
        .query_map(
            params![
                target_event.project_id.to_string(),
                target_event.idea_id.to_string(),
                cursor,
                target_event.sequence,
                limit,
            ],
            map_idea_event_row,
        )
        .map_err(map_sql_error)?;
    let mut batch = Vec::with_capacity(IDEA_EVENT_PAGE_MAX_LIMIT);
    for row in rows {
        batch.push(
            row.map_err(map_sql_error)?
                .into_idea_event()
                .map_err(corrupt_event)?,
        );
    }
    Ok(batch)
}

fn reconstruct_projection_at_tx(
    tx: &Transaction<'_>,
    target_event: &IdeaEvent,
) -> IdeaResult<Idea> {
    let mut idea = None;
    let mut cursor = 0_i64;
    let mut previous_resulting_version = 0_i64;
    loop {
        let batch = load_replay_event_batch_tx(tx, target_event, cursor)?;
        if batch.is_empty() {
            break;
        }
        for event in batch {
            if event.sequence != cursor + 1
                || event.expected_row_version != previous_resulting_version
                || event.resulting_row_version != event.expected_row_version + 1
            {
                return Err(corrupt_event("Idea event sequence/version chain has a gap"));
            }
            if matches!(
                event.event_type,
                IdeaEventType::ControllerReserved
                    | IdeaEventType::ControllerAssigned
                    | IdeaEventType::ControllerReleased
            ) {
                if idea.is_none() {
                    return Err(corrupt_event(
                        "controller event appeared before the Idea creation event",
                    ));
                }
                let payload = event
                    .controller_control_payload_v1()
                    .map_err(corrupt_event)?;
                idea = Some(payload.outcome.idea);
            } else if event
                .payload
                .get("request")
                .and_then(|request| request.get("domain"))
                .and_then(serde_json::Value::as_str)
                == Some(rsi_common::types::IDEA_CONTROLLER_MUTATION_V1)
            {
                if idea.is_none() {
                    return Err(corrupt_event(
                        "controller mutation appeared before the Idea creation event",
                    ));
                }
                let payload = event
                    .controller_mutation_payload_v1()
                    .map_err(corrupt_event)?;
                idea = Some(payload.outcome.idea);
            } else {
                let semantic = event.semantic_request_v1().map_err(corrupt_event)?;
                match (idea.as_mut(), semantic.operation) {
                    (None, IdeaSemanticOperationV1::Create(create)) if event.sequence == 1 => {
                        idea = Some(Idea {
                            id: event.idea_id,
                            project_id: event.project_id,
                            slug: create.slug,
                            sigil: create.sigil,
                            genesis_capture_id: create.genesis_capture_id,
                            genesis_span_start: create.genesis_span.as_ref().map(|span| span.start),
                            genesis_span_end: create.genesis_span.as_ref().map(|span| span.end),
                            genesis_span_digest: create.genesis_span.map(|span| span.digest),
                            title: create.title,
                            description: create.description,
                            portfolio_summary: create.portfolio_summary,
                            lifecycle: IdeaLifecycle::Open,
                            stage: IdeaStage::Captured,
                            priority: create.priority,
                            autonomy_policy: create.autonomy_policy,
                            integration_target_ref: create.integration_target_ref,
                            program_template_policy_id: create.program_template_policy_id,
                            current_controller_session_id: None,
                            controller_epoch: 0,
                            row_version: 1,
                            next_event_sequence: 2,
                            created_at: event.occurred_at,
                            updated_at: event.occurred_at,
                            terminal_at: None,
                            superseded_at: None,
                        });
                    }
                    (Some(idea), IdeaSemanticOperationV1::Mutate { action }) => {
                        apply_action_to_projection(idea, &action, event.occurred_at);
                        idea.row_version = event.resulting_row_version;
                        idea.next_event_sequence = event.sequence + 1;
                        idea.updated_at = event.occurred_at;
                    }
                    (Some(idea), IdeaSemanticOperationV1::LinkIssue { .. }) => {
                        idea.row_version = event.resulting_row_version;
                        idea.next_event_sequence = event.sequence + 1;
                        idea.updated_at = event.occurred_at;
                    }
                    _ => {
                        return Err(corrupt_event(
                            "Idea history does not contain exactly one leading create event",
                        ));
                    }
                }
            }
            cursor = event.sequence;
            previous_resulting_version = event.resulting_row_version;
        }
        if cursor >= target_event.sequence {
            break;
        }
    }
    if cursor != target_event.sequence {
        return Err(corrupt_event(
            "Idea history ended before the replay target event",
        ));
    }
    let idea = idea.ok_or_else(|| corrupt_event("Idea has no creation event"))?;
    idea.validate().map_err(corrupt_event)?;
    Ok(idea)
}

const fn is_terminal(lifecycle: IdeaLifecycle) -> bool {
    matches!(
        lifecycle,
        IdeaLifecycle::Completed | IdeaLifecycle::Abandoned | IdeaLifecycle::Superseded
    )
}

fn prepare_supersession_tx(
    tx: &Transaction<'_>,
    idea: &Idea,
    replacement_idea_ids: &[Uuid],
) -> IdeaResult<RelationshipWrite> {
    map_transition(evaluate_idea_lifecycle_transition(
        idea.lifecycle,
        IdeaLifecycle::Superseded,
        false,
        true,
    ))?;
    for target_id in replacement_idea_ids {
        if *target_id == idea.id {
            return Err(IdeaControlError::RelationshipConflict(
                "supersession cannot target the source Idea".to_string(),
            ));
        }
        let target = require_same_project_idea_tx(tx, idea.project_id, *target_id)?;
        if is_terminal(target.lifecycle) {
            return Err(IdeaControlError::RelationshipConflict(
                "supersession target must be nonterminal".to_string(),
            ));
        }
        ensure_no_active_edge_tx(tx, idea.id, *target_id, IdeaRelationshipKind::Supersedes)?;
        ensure_no_relationship_cycle_tx(tx, idea.id, *target_id, IdeaRelationshipKind::Supersedes)?;
    }
    Ok(RelationshipWrite::Supersede {
        target_ids: replacement_idea_ids.to_vec(),
    })
}

fn prepare_mutation_tx(
    tx: &Transaction<'_>,
    idea: &Idea,
    action: &IdeaMutationActionV1,
) -> IdeaResult<RelationshipWrite> {
    match action {
        IdeaMutationActionV1::Park { reason } => {
            map_transition(evaluate_idea_lifecycle_transition(
                idea.lifecycle,
                IdeaLifecycle::Parked,
                !reason.trim().is_empty(),
                false,
            ))?;
        }
        IdeaMutationActionV1::Reopen { reason } => {
            map_transition(evaluate_idea_lifecycle_transition(
                idea.lifecycle,
                IdeaLifecycle::Open,
                !reason.trim().is_empty(),
                false,
            ))?;
        }
        IdeaMutationActionV1::Abandon { reason } => {
            map_transition(evaluate_idea_lifecycle_transition(
                idea.lifecycle,
                IdeaLifecycle::Abandoned,
                !reason.trim().is_empty(),
                false,
            ))?;
        }
        IdeaMutationActionV1::TransitionStage { stage, reason } => {
            map_transition(evaluate_idea_stage_transition(
                idea.lifecycle,
                idea.stage,
                *stage,
                reason
                    .as_deref()
                    .is_some_and(|value| !value.trim().is_empty()),
            ))?;
        }
        IdeaMutationActionV1::AcceptSupersession {
            replacement_idea_ids,
        } => return prepare_supersession_tx(tx, idea, replacement_idea_ids),
        IdeaMutationActionV1::AddRelationship {
            relationship_kind,
            target_idea_id,
        } => {
            reject_terminal_projection_mutation(idea)?;
            validate_relationship_target_tx(tx, idea, *target_idea_id)?;
            ensure_no_active_edge_tx(tx, idea.id, *target_idea_id, *relationship_kind)?;
            ensure_no_relationship_cycle_tx(tx, idea.id, *target_idea_id, *relationship_kind)?;
            return Ok(RelationshipWrite::Add {
                kind: *relationship_kind,
                target_id: *target_idea_id,
            });
        }
        IdeaMutationActionV1::RemoveRelationship {
            relationship_kind,
            target_idea_id,
        } => {
            reject_terminal_projection_mutation(idea)?;
            validate_relationship_target_tx(tx, idea, *target_idea_id)?;
            let relationship =
                load_active_relationship_tx(tx, idea.id, *target_idea_id, *relationship_kind)?
                    .ok_or(IdeaControlError::RelationshipNotFound)?;
            return Ok(RelationshipWrite::Remove { relationship });
        }
        IdeaMutationActionV1::ChangeProjection { .. }
        | IdeaMutationActionV1::ChangeScope { .. }
        | IdeaMutationActionV1::ChangeAutonomy { .. } => {
            reject_terminal_projection_mutation(idea)?;
            ensure_action_changes_projection(idea, action)?;
        }
    }
    Ok(RelationshipWrite::None)
}

fn map_transition(result: std::result::Result<(), IdeaTransitionViolation>) -> IdeaResult<()> {
    result.map_err(|violation| match violation {
        IdeaTransitionViolation::NoSemanticChange => IdeaControlError::NoSemanticChange,
        IdeaTransitionViolation::InvalidLifecycleTransition => {
            IdeaControlError::InvalidLifecycleTransition
        }
        IdeaTransitionViolation::InvalidStageTransition => IdeaControlError::InvalidStageTransition,
        IdeaTransitionViolation::PrerequisiteUnavailable => {
            IdeaControlError::PrerequisiteUnavailable
        }
        IdeaTransitionViolation::TerminalLifecycle => IdeaControlError::TerminalLifecycle,
        IdeaTransitionViolation::LifecyclePaused => IdeaControlError::LifecyclePaused,
    })?;
    Ok(())
}

const fn reject_terminal_projection_mutation(idea: &Idea) -> IdeaResult<()> {
    if is_terminal(idea.lifecycle) {
        Err(IdeaControlError::TerminalLifecycle)
    } else {
        Ok(())
    }
}

fn ensure_action_changes_projection(idea: &Idea, action: &IdeaMutationActionV1) -> IdeaResult<()> {
    let changes = match action {
        IdeaMutationActionV1::ChangeProjection {
            sigil,
            title,
            description,
            portfolio_summary,
            priority,
        } => {
            sigil.as_ref().is_some_and(|patch| match patch {
                IdeaOptionalStringV1::Set { value } => idea.sigil.as_ref() != Some(value),
                IdeaOptionalStringV1::Clear => idea.sigil.is_some(),
            }) || title.as_ref().is_some_and(|value| idea.title != *value)
                || description
                    .as_ref()
                    .is_some_and(|value| idea.description != *value)
                || portfolio_summary
                    .as_ref()
                    .is_some_and(|value| idea.portfolio_summary != *value)
                || priority.is_some_and(|value| idea.priority != value)
        }
        IdeaMutationActionV1::ChangeScope {
            integration_target_ref,
            program_template_policy_id,
        } => {
            integration_target_ref
                .as_ref()
                .is_some_and(|value| idea.integration_target_ref != *value)
                || program_template_policy_id
                    .as_ref()
                    .is_some_and(|patch| match patch {
                        IdeaOptionalStringV1::Set { value } => {
                            idea.program_template_policy_id.as_ref() != Some(value)
                        }
                        IdeaOptionalStringV1::Clear => idea.program_template_policy_id.is_some(),
                    })
        }
        IdeaMutationActionV1::ChangeAutonomy { autonomy_policy } => {
            idea.autonomy_policy != *autonomy_policy
        }
        _ => true,
    };
    if changes {
        Ok(())
    } else {
        Err(IdeaControlError::NoSemanticChange)
    }
}

fn apply_action_to_projection(
    idea: &mut Idea,
    action: &IdeaMutationActionV1,
    timestamp: DateTime<Utc>,
) {
    match action {
        IdeaMutationActionV1::ChangeProjection {
            sigil,
            title,
            description,
            portfolio_summary,
            priority,
        } => {
            if let Some(patch) = sigil {
                idea.sigil = match patch {
                    IdeaOptionalStringV1::Set { value } => Some(value.clone()),
                    IdeaOptionalStringV1::Clear => None,
                };
            }
            if let Some(value) = title {
                idea.title.clone_from(value);
            }
            if let Some(value) = description {
                idea.description.clone_from(value);
            }
            if let Some(value) = portfolio_summary {
                idea.portfolio_summary.clone_from(value);
            }
            if let Some(value) = priority {
                idea.priority = *value;
            }
        }
        IdeaMutationActionV1::ChangeScope {
            integration_target_ref,
            program_template_policy_id,
        } => {
            if let Some(value) = integration_target_ref {
                idea.integration_target_ref.clone_from(value);
            }
            if let Some(patch) = program_template_policy_id {
                idea.program_template_policy_id = match patch {
                    IdeaOptionalStringV1::Set { value } => Some(value.clone()),
                    IdeaOptionalStringV1::Clear => None,
                };
            }
        }
        IdeaMutationActionV1::ChangeAutonomy { autonomy_policy } => {
            idea.autonomy_policy = *autonomy_policy;
        }
        IdeaMutationActionV1::Park { .. } => idea.lifecycle = IdeaLifecycle::Parked,
        IdeaMutationActionV1::Reopen { .. } => idea.lifecycle = IdeaLifecycle::Open,
        IdeaMutationActionV1::Abandon { .. } => {
            idea.lifecycle = IdeaLifecycle::Abandoned;
            idea.terminal_at = Some(timestamp);
        }
        IdeaMutationActionV1::TransitionStage { stage, .. } => idea.stage = *stage,
        IdeaMutationActionV1::AcceptSupersession { .. } => {
            idea.lifecycle = IdeaLifecycle::Superseded;
            idea.terminal_at = Some(timestamp);
            idea.superseded_at = Some(timestamp);
        }
        IdeaMutationActionV1::AddRelationship { .. }
        | IdeaMutationActionV1::RemoveRelationship { .. } => {}
    }
}

fn update_idea_projection_tx(
    tx: &Transaction<'_>,
    idea: &Idea,
    expected_version: i64,
    expected_sequence: i64,
    timestamp: DateTime<Utc>,
) -> IdeaResult<usize> {
    tx.execute(
        "UPDATE ideas
         SET sigil = ?1, title = ?2, description = ?3, portfolio_summary = ?4,
             lifecycle = ?5, stage = ?6, priority = ?7, autonomy_policy = ?8,
             integration_target_ref = ?9, program_template_policy_id = ?10,
             row_version = row_version + 1,
             next_event_sequence = next_event_sequence + 1,
             updated_at = ?11, terminal_at = ?12, superseded_at = ?13
         WHERE id = ?14 AND project_id = ?15
           AND row_version = ?16 AND next_event_sequence = ?17",
        params![
            idea.sigil,
            idea.title,
            idea.description,
            idea.portfolio_summary,
            idea.lifecycle.as_str(),
            idea.stage.as_str(),
            idea.priority,
            idea.autonomy_policy.as_str(),
            idea.integration_target_ref,
            idea.program_template_policy_id,
            canonical_timestamp(timestamp),
            idea.terminal_at.map(canonical_timestamp),
            idea.superseded_at.map(canonical_timestamp),
            idea.id.to_string(),
            idea.project_id.to_string(),
            expected_version,
            expected_sequence,
        ],
    )
    .map_err(map_sql_error)
}

fn validate_relationship_target_tx(
    tx: &Transaction<'_>,
    source: &Idea,
    target_id: Uuid,
) -> IdeaResult<()> {
    if target_id == source.id {
        return Err(IdeaControlError::RelationshipConflict(
            "relationship endpoints must be distinct".to_string(),
        ));
    }
    require_same_project_idea_tx(tx, source.project_id, target_id)?;
    Ok(())
}

fn ensure_no_active_edge_tx(
    tx: &Transaction<'_>,
    source_id: Uuid,
    target_id: Uuid,
    kind: IdeaRelationshipKind,
) -> IdeaResult<()> {
    let exists = tx
        .query_row(
            "SELECT 1 FROM idea_relationships
             WHERE source_idea_id = ?1 AND target_idea_id = ?2
               AND kind = ?3 AND removed_at IS NULL",
            params![source_id.to_string(), target_id.to_string(), kind.as_str()],
            |_| Ok(()),
        )
        .optional()
        .map_err(map_sql_error)?
        .is_some();
    if exists {
        Err(IdeaControlError::RelationshipConflict(
            "active relationship already exists".to_string(),
        ))
    } else {
        Ok(())
    }
}

fn ensure_no_relationship_cycle_tx(
    tx: &Transaction<'_>,
    source_id: Uuid,
    target_id: Uuid,
    kind: IdeaRelationshipKind,
) -> IdeaResult<()> {
    let closes_cycle = tx
        .query_row(
            "WITH RECURSIVE reachable(idea_id) AS (
                 SELECT target_idea_id
                 FROM idea_relationships
                 WHERE source_idea_id = ?1 AND kind = ?2 AND removed_at IS NULL
                 UNION
                 SELECT relationships.target_idea_id
                 FROM idea_relationships AS relationships
                 JOIN reachable ON relationships.source_idea_id = reachable.idea_id
                 WHERE relationships.kind = ?2 AND relationships.removed_at IS NULL
             )
             SELECT 1 FROM reachable WHERE idea_id = ?3 LIMIT 1",
            params![target_id.to_string(), kind.as_str(), source_id.to_string()],
            |_| Ok(()),
        )
        .optional()
        .map_err(map_sql_error)?
        .is_some();
    if closes_cycle {
        Err(IdeaControlError::RelationshipConflict(
            "active relationship would create a cycle".to_string(),
        ))
    } else {
        Ok(())
    }
}

fn load_active_relationship_tx(
    tx: &Transaction<'_>,
    source_id: Uuid,
    target_id: Uuid,
    kind: IdeaRelationshipKind,
) -> IdeaResult<Option<IdeaRelationship>> {
    tx.query_row(
        &format!(
            "SELECT {IDEA_RELATIONSHIP_COLUMNS}
             FROM idea_relationships
             WHERE source_idea_id = ?1 AND target_idea_id = ?2
               AND kind = ?3 AND removed_at IS NULL"
        ),
        params![source_id.to_string(), target_id.to_string(), kind.as_str()],
        map_idea_relationship_row,
    )
    .optional()
    .map_err(map_sql_error)?
    .map(|row| row.into_idea_relationship().map_err(corrupt_event))
    .transpose()
}

fn apply_relationship_write_tx(
    tx: &Transaction<'_>,
    write: RelationshipWrite,
    event: &IdeaEvent,
    timestamp: DateTime<Utc>,
) -> IdeaResult<Vec<IdeaRelationship>> {
    match write {
        RelationshipWrite::None => Ok(Vec::new()),
        RelationshipWrite::Add { kind, target_id } => {
            let relationship = build_relationship(
                event.id,
                event.project_id,
                event.idea_id,
                target_id,
                kind,
                timestamp,
            );
            insert_relationship_tx(tx, &relationship)?;
            d02_fault!(IdeaWriteFault::RelationshipAddAfterEdge)?;
            Ok(vec![relationship])
        }
        RelationshipWrite::Remove { mut relationship } => {
            let changed = tx
                .execute(
                    "UPDATE idea_relationships
                     SET removed_event_id = ?1, removed_at = ?2
                     WHERE id = ?3 AND removed_event_id IS NULL AND removed_at IS NULL",
                    params![
                        event.id.to_string(),
                        canonical_timestamp(timestamp),
                        relationship.id.to_string(),
                    ],
                )
                .map_err(map_sql_error)?;
            if changed != 1 {
                return Err(IdeaControlError::RelationshipConflict(
                    "relationship tombstone changed concurrently".to_string(),
                ));
            }
            relationship.removed_event_id = Some(event.id);
            relationship.removed_at = Some(timestamp);
            d02_fault!(IdeaWriteFault::RelationshipRemoveAfterTombstone)?;
            Ok(vec![relationship])
        }
        RelationshipWrite::Supersede { target_ids } => {
            let mut relationships = Vec::with_capacity(target_ids.len());
            for target_id in target_ids {
                let relationship = build_relationship(
                    event.id,
                    event.project_id,
                    event.idea_id,
                    target_id,
                    IdeaRelationshipKind::Supersedes,
                    timestamp,
                );
                insert_relationship_tx(tx, &relationship)?;
                relationships.push(relationship);
                d02_fault!(IdeaWriteFault::SupersessionAfterEachEdge)?;
            }
            Ok(relationships)
        }
    }
}

#[cfg(test)]
impl Store {
    pub(crate) fn insert_d02_capture_fixture(
        &self,
        capture: &rsi_common::types::Capture,
    ) -> Result<()> {
        capture
            .validate()
            .map_err(|error| DaemonError::Store(format!("invalid fixture Capture: {error}")))?;
        self.conn.execute(
            "INSERT INTO captures (
                id, project_id, creator_kind, creator_id, captured_at, source_kind,
                raw_content_digest, storage_policy_id, content_ref
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                capture.id.to_string(),
                capture.project_id.to_string(),
                capture.creator_kind.as_str(),
                capture.creator_id,
                d01_timestamp(capture.captured_at),
                capture.source_kind.as_str(),
                capture.raw_content_digest.as_str(),
                capture.storage_policy_id,
                capture.content_ref.as_str(),
            ],
        )?;
        Ok(())
    }

    pub(crate) fn insert_d01_idea_fixture(
        &self,
        capture: &rsi_common::types::Capture,
        idea: &rsi_common::types::Idea,
    ) -> Result<()> {
        capture
            .validate()
            .map_err(|error| DaemonError::Store(format!("invalid fixture Capture: {error}")))?;
        idea.validate()
            .map_err(|error| DaemonError::Store(format!("invalid fixture Idea: {error}")))?;

        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO captures (
                id, project_id, creator_kind, creator_id, captured_at, source_kind,
                raw_content_digest, storage_policy_id, content_ref
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                capture.id.to_string(),
                capture.project_id.to_string(),
                capture.creator_kind.as_str(),
                capture.creator_id,
                d01_timestamp(capture.captured_at),
                capture.source_kind.as_str(),
                capture.raw_content_digest.as_str(),
                capture.storage_policy_id,
                capture.content_ref.as_str(),
            ],
        )?;
        tx.execute(
            "INSERT INTO ideas (
                id, project_id, slug, sigil, genesis_capture_id, genesis_span_start,
                genesis_span_end, genesis_span_digest, title, description, portfolio_summary,
                lifecycle, stage, priority, autonomy_policy, integration_target_ref,
                program_template_policy_id, current_controller_session_id, controller_epoch,
                row_version, next_event_sequence, created_at, updated_at, terminal_at,
                superseded_at
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25
             )",
            params![
                idea.id.to_string(),
                idea.project_id.to_string(),
                idea.slug,
                idea.sigil,
                idea.genesis_capture_id.to_string(),
                idea.genesis_span_start,
                idea.genesis_span_end,
                idea.genesis_span_digest.as_ref().map(ToString::to_string),
                idea.title,
                idea.description,
                idea.portfolio_summary,
                idea.lifecycle.as_str(),
                idea.stage.as_str(),
                idea.priority,
                idea.autonomy_policy.as_str(),
                idea.integration_target_ref,
                idea.program_template_policy_id,
                idea.current_controller_session_id.map(|id| id.to_string()),
                idea.controller_epoch,
                idea.row_version,
                idea.next_event_sequence,
                d01_timestamp(idea.created_at),
                d01_timestamp(idea.updated_at),
                idea.terminal_at.map(d01_timestamp),
                idea.superseded_at.map(d01_timestamp),
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn insert_d01_event_fixture(
        &self,
        event: &rsi_common::types::IdeaEvent,
    ) -> Result<()> {
        event
            .validate()
            .map_err(|error| DaemonError::Store(format!("invalid fixture IdeaEvent: {error}")))?;
        self.conn.execute(
            "INSERT INTO idea_events (
                id, project_id, idea_id, sequence, event_type, actor_kind, actor_id,
                controller_session_id, controller_epoch, expected_row_version,
                resulting_row_version, idempotency_key, occurred_at, payload_json,
                artifact_digests_json, evidence_digests_json
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16
             )",
            params![
                event.id.to_string(),
                event.project_id.to_string(),
                event.idea_id.to_string(),
                event.sequence,
                event.event_type.as_str(),
                event.actor_kind.as_str(),
                event.actor_id,
                event.controller_session_id.map(|id| id.to_string()),
                event.controller_epoch,
                event.expected_row_version,
                event.resulting_row_version,
                event.idempotency_key,
                d01_timestamp(event.occurred_at),
                serde_json::to_string(&event.payload)?,
                serde_json::to_string(&event.artifact_digests)?,
                serde_json::to_string(&event.evidence_digests)?,
            ],
        )?;
        Ok(())
    }

    pub(crate) fn insert_d01_collection_fixture(
        &self,
        collection: &rsi_common::types::IdeaCollection,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO idea_collections (
                id, project_id, slug, name, description, created_at, updated_at, retired_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                collection.id.to_string(),
                collection.project_id.to_string(),
                collection.slug,
                collection.name,
                collection.description,
                d01_timestamp(collection.created_at),
                d01_timestamp(collection.updated_at),
                collection.retired_at.map(d01_timestamp),
            ],
        )?;
        Ok(())
    }

    pub(crate) fn insert_d01_relationship_fixture(
        &self,
        relationship: &rsi_common::types::IdeaRelationship,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO idea_relationships (
                id, project_id, source_idea_id, target_idea_id, kind,
                created_event_id, created_at, removed_event_id, removed_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                relationship.id.to_string(),
                relationship.project_id.to_string(),
                relationship.source_idea_id.to_string(),
                relationship.target_idea_id.to_string(),
                relationship.kind.as_str(),
                relationship.created_event_id.to_string(),
                d01_timestamp(relationship.created_at),
                relationship.removed_event_id.map(|id| id.to_string()),
                relationship.removed_at.map(d01_timestamp),
            ],
        )?;
        Ok(())
    }

    pub(crate) fn insert_d01_membership_fixture(
        &self,
        membership: &rsi_common::types::IdeaCollectionMembership,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO idea_collection_memberships (
                id, project_id, collection_id, idea_id, added_at, removed_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                membership.id.to_string(),
                membership.project_id.to_string(),
                membership.collection_id.to_string(),
                membership.idea_id.to_string(),
                d01_timestamp(membership.added_at),
                membership.removed_at.map(d01_timestamp),
            ],
        )?;
        Ok(())
    }

    pub(crate) fn insert_d01_compatibility_fixture(
        &self,
        mapping: &rsi_common::types::IdeaCompatibilityMapping,
    ) -> Result<()> {
        mapping.validate().map_err(|error| {
            DaemonError::Store(format!("invalid fixture compatibility mapping: {error}"))
        })?;
        self.conn.execute(
            "INSERT INTO idea_compatibility_mappings (
                id, project_id, legacy_source_kind, legacy_source_id, idea_id, collection_id,
                status, provenance_json, disposition, created_at, updated_at, mapped_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                mapping.id.to_string(),
                mapping.project_id.to_string(),
                mapping.legacy_source_kind.as_str(),
                mapping.legacy_source_id.to_string(),
                mapping.idea_id.map(|id| id.to_string()),
                mapping.collection_id.map(|id| id.to_string()),
                mapping.status.as_str(),
                serde_json::to_string(&mapping.provenance)?,
                mapping.disposition,
                d01_timestamp(mapping.created_at),
                d01_timestamp(mapping.updated_at),
                mapping.mapped_at.map(d01_timestamp),
            ],
        )?;
        Ok(())
    }
}

#[cfg(test)]
fn d01_timestamp(timestamp: chrono::DateTime<chrono::Utc>) -> String {
    timestamp.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}
