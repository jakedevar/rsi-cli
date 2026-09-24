//! Crate-internal authority boundary for D02 operator writes and D03
//! controller transfer/mutation.
//!
//! No RPC, agent verb, native tool, or `StoreWorker` command constructs a
//! controller grant. Transport adapters must first resolve A6 attribution,
//! then use the construction-only bound types in this module.

use crate::store::Store;
use chrono::{DateTime, Utc};
use rsi_common::rpc::{LinkIssueToIdeaParams, LinkIssueToIdeaResult};
use rsi_common::types::{
    ControllerReleaseReasonV1, CreateIdeaRequestV1, IdeaActorKind, IdeaControllerControlResultV1,
    IdeaControllerLaunchConfirmationV1, IdeaControllerMutationResultV1,
    IdeaControllerReservationV1, IdeaEventPageRequestV1, IdeaEventPageV1, IdeaMutationResultV1,
    MutateIdeaAsControllerRequestV1, MutateIdeaRequestV1, ReleaseAssignedIdeaControllerRequestV1,
    ReleaseIdeaControllerReservationRequestV1, ReserveIdeaControllerRequestV1,
    validate_idea_actor_id,
};
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

#[allow(clippy::redundant_pub_crate)]
pub(crate) trait AgentTokenLookup: Send + Sync {
    fn session_for_token(&self, token: &str) -> Option<Uuid>;
}

impl AgentTokenLookup for HashMap<String, Uuid> {
    fn session_for_token(&self, token: &str) -> Option<Uuid> {
        self.get(token).copied()
    }
}

impl AgentTokenLookup for crate::session::AgentTokenRegistry {
    fn session_for_token(&self, token: &str) -> Option<Uuid> {
        self.get(token).copied()
    }
}

/// Redacted, stable `SQLite` integrity classes exposed only through the
/// operator Issue-link error envelope.  The database message is deliberately
/// not transport data: it can contain SQL or implementation details.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueLinkConstraintClass {
    ForeignKey,
    Check,
    NotNull,
    UniquePrimary,
}

impl IssueLinkConstraintClass {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::ForeignKey => "foreign_key",
            Self::Check => "check",
            Self::NotNull => "not_null",
            Self::UniquePrimary => "unique_primary",
        }
    }
}

/// Stable internal error categories for the D02 mutation boundary.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[allow(
    clippy::redundant_pub_crate,
    reason = "explicit crate barrier prevents future module widening from exposing authority errors"
)]
pub(crate) enum IdeaControlError {
    #[error("invalid Idea request: {0}")]
    InvalidRequest(String),
    #[error("Idea actor is forbidden")]
    ForbiddenActor,
    #[error("Idea not found")]
    IdeaNotFound,
    #[error("Issue not found")]
    IssueNotFound,
    #[error("Issue is already linked")]
    IssueAlreadyLinked,
    #[error("Issue linkage is corrupt")]
    CorruptStoredIssueLink,
    #[error("Issue source event is outside the linked Idea/project")]
    SourceEventScopeMismatch,
    #[error("Capture not found")]
    CaptureNotFound,
    #[error("Idea relationship not found")]
    RelationshipNotFound,
    #[error("Idea object is outside the bound project")]
    ProjectScopeMismatch,
    #[error("stale Idea row version: expected {expected}, actual {actual}")]
    StaleVersion { expected: i64, actual: i64 },
    #[error("stale controller epoch: expected {expected}, actual {actual}")]
    StaleControllerEpoch { expected: i64, actual: i64 },
    #[error("Idea controller does not match the bound controller")]
    ControllerMismatch,
    #[error("an unresolved controller reservation already exists")]
    ControllerReservationConflict,
    #[error("controller reservation not found")]
    ControllerReservationNotFound,
    #[error("controller reservation was already resolved")]
    ControllerReservationResolved,
    #[error("controller reservation expired")]
    ControllerReservationExpired,
    #[error("controller candidate launch was not confirmed")]
    LaunchNotConfirmed,
    #[error("controller epoch is exhausted")]
    ControllerEpochExhausted,
    #[error("Idea idempotency key was already used for different semantics")]
    IdempotencyConflict,
    #[error("Idea action makes no semantic change")]
    NoSemanticChange,
    #[error("invalid Idea lifecycle transition")]
    InvalidLifecycleTransition,
    #[error("invalid Idea stage transition")]
    InvalidStageTransition,
    #[error("Idea transition prerequisite is unavailable")]
    PrerequisiteUnavailable,
    #[error("terminal Idea lifecycle rejects this action")]
    TerminalLifecycle,
    #[error("parked Idea must be reopened before changing stage")]
    LifecyclePaused,
    #[error("Idea relationship conflict: {0}")]
    RelationshipConflict(String),
    #[error("Idea write encountered SQLite contention")]
    Contention,
    #[error("Issue link violated a SQLite integrity constraint")]
    ConstraintViolation { class: IssueLinkConstraintClass },
    #[error("corrupt stored Idea event: {0}")]
    CorruptStoredEvent(String),
    #[error("Idea storage failure: {0}")]
    StorageFailure(String),
}

/// Unforgeable project and actor binding required by every Store write.
#[derive(Debug, Clone)]
#[allow(
    clippy::redundant_pub_crate,
    reason = "explicit crate barrier keeps Store authority unnameable outside rsid"
)]
pub(crate) struct BoundIdeaWriteAuthority {
    project_id: Uuid,
    actor_kind: IdeaActorKind,
    actor_id: String,
}

impl BoundIdeaWriteAuthority {
    fn operator(project_id: Uuid, actor_id: String) -> Result<Self, IdeaControlError> {
        if project_id.is_nil() {
            return Err(IdeaControlError::InvalidRequest(
                "project_id must not be nil".to_string(),
            ));
        }
        validate_idea_actor_id(&actor_id).map_err(IdeaControlError::InvalidRequest)?;
        Ok(Self {
            project_id,
            actor_kind: IdeaActorKind::Operator,
            actor_id,
        })
    }

    pub(crate) const fn project_id(&self) -> Uuid {
        self.project_id
    }

    pub(crate) const fn actor_kind(&self) -> IdeaActorKind {
        self.actor_kind
    }

    pub(crate) fn actor_id(&self) -> &str {
        &self.actor_id
    }
}

/// Sole internal production owner of D02 Idea mutation authority.
#[derive(Clone)]
#[allow(
    clippy::redundant_pub_crate,
    reason = "explicit crate barrier preserves the construction-only D02 authority boundary"
)]
pub(crate) struct IdeaControlHandle {
    store: Arc<Mutex<Store>>,
    authority: BoundIdeaWriteAuthority,
}

/// Injected UTC clock used by every D03 persisted timestamp and expiry.
#[allow(
    clippy::redundant_pub_crate,
    reason = "explicit crate barrier keeps the controller clock injection internal"
)]
pub(crate) trait IdeaControllerClock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

#[derive(Debug, Default)]
#[allow(
    clippy::redundant_pub_crate,
    reason = "explicit crate barrier keeps the production controller clock internal"
)]
pub(crate) struct SystemIdeaControllerClock;

impl IdeaControllerClock for SystemIdeaControllerClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// Bound operator/system authority for controller-control operations.
#[derive(Debug, Clone)]
#[allow(
    clippy::redundant_pub_crate,
    reason = "explicit crate barrier keeps transfer authority construction-only"
)]
pub(crate) struct BoundIdeaControllerTransferAuthority {
    project_id: Uuid,
    idea_id: Uuid,
    actor_kind: IdeaActorKind,
    actor_id: String,
    observed_controller_epoch: i64,
}

impl BoundIdeaControllerTransferAuthority {
    fn new(
        project_id: Uuid,
        idea_id: Uuid,
        actor_kind: IdeaActorKind,
        actor_id: String,
        observed_controller_epoch: i64,
    ) -> Result<Self, IdeaControlError> {
        if project_id.is_nil() || idea_id.is_nil() || observed_controller_epoch < 0 {
            return Err(IdeaControlError::InvalidRequest(
                "bound controller-transfer scope or epoch is invalid".to_string(),
            ));
        }
        validate_idea_actor_id(&actor_id).map_err(IdeaControlError::InvalidRequest)?;
        if !matches!(actor_kind, IdeaActorKind::Operator | IdeaActorKind::System) {
            return Err(IdeaControlError::ForbiddenActor);
        }
        Ok(Self {
            project_id,
            idea_id,
            actor_kind,
            actor_id,
            observed_controller_epoch,
        })
    }

    pub(crate) const fn project_id(&self) -> Uuid {
        self.project_id
    }

    pub(crate) const fn idea_id(&self) -> Uuid {
        self.idea_id
    }

    pub(crate) const fn actor_kind(&self) -> IdeaActorKind {
        self.actor_kind
    }

    pub(crate) fn actor_id(&self) -> &str {
        &self.actor_id
    }

    pub(crate) const fn observed_controller_epoch(&self) -> i64 {
        self.observed_controller_epoch
    }
}

/// Durable semantic authority reconstructed only after A6 and the assigned
/// Idea projection both match.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(
    clippy::redundant_pub_crate,
    reason = "explicit crate barrier keeps semantic grants construction-only"
)]
pub(crate) struct BoundControllerWriteAuthority {
    project_id: Uuid,
    idea_id: Uuid,
    controller_session_id: Uuid,
    controller_epoch: i64,
}

impl BoundControllerWriteAuthority {
    pub(crate) fn new(
        project_id: Uuid,
        idea_id: Uuid,
        controller_session_id: Uuid,
        controller_epoch: i64,
    ) -> Result<Self, IdeaControlError> {
        if project_id.is_nil()
            || idea_id.is_nil()
            || controller_session_id.is_nil()
            || controller_epoch < 0
        {
            return Err(IdeaControlError::InvalidRequest(
                "bound controller authority is invalid".to_string(),
            ));
        }
        Ok(Self {
            project_id,
            idea_id,
            controller_session_id,
            controller_epoch,
        })
    }

    pub(crate) const fn project_id(&self) -> Uuid {
        self.project_id
    }

    pub(crate) const fn idea_id(&self) -> Uuid {
        self.idea_id
    }

    pub(crate) const fn controller_session_id(&self) -> Uuid {
        self.controller_session_id
    }

    pub(crate) const fn controller_epoch(&self) -> i64 {
        self.controller_epoch
    }
}

/// Construction-only controller transfer coordinator.
#[derive(Clone)]
#[allow(
    clippy::redundant_pub_crate,
    reason = "explicit crate barrier prevents transport construction of transfer handles"
)]
pub(crate) struct IdeaControllerTransferHandle {
    store: Arc<Mutex<Store>>,
    authority: BoundIdeaControllerTransferAuthority,
    clock: Arc<dyn IdeaControllerClock>,
}

impl IdeaControllerTransferHandle {
    /// Bind an operator to the current server-read Idea epoch.
    pub(crate) async fn for_operator(
        store: Arc<Mutex<Store>>,
        project_id: Uuid,
        idea_id: Uuid,
        actor_id: impl Into<String>,
        clock: Arc<dyn IdeaControllerClock>,
    ) -> Result<Self, IdeaControlError> {
        Self::new(
            store,
            project_id,
            idea_id,
            IdeaActorKind::Operator,
            actor_id.into(),
            clock,
        )
        .await
    }

    /// Bind the fixed internal transfer actor to the current server-read epoch.
    pub(crate) async fn for_system(
        store: Arc<Mutex<Store>>,
        project_id: Uuid,
        idea_id: Uuid,
        clock: Arc<dyn IdeaControllerClock>,
    ) -> Result<Self, IdeaControlError> {
        let _ = d03_internal_surface_compile_anchor;
        Self::new(
            store,
            project_id,
            idea_id,
            IdeaActorKind::System,
            "rsid:controller-transfer".to_string(),
            clock,
        )
        .await
    }

    async fn new(
        store: Arc<Mutex<Store>>,
        project_id: Uuid,
        idea_id: Uuid,
        actor_kind: IdeaActorKind,
        actor_id: String,
        clock: Arc<dyn IdeaControllerClock>,
    ) -> Result<Self, IdeaControlError> {
        let observed_controller_epoch = store
            .lock()
            .await
            .load_idea_controller_projection_v1(project_id, idea_id)?
            .controller_epoch;
        Ok(Self {
            store,
            authority: BoundIdeaControllerTransferAuthority::new(
                project_id,
                idea_id,
                actor_kind,
                actor_id,
                observed_controller_epoch,
            )?,
            clock,
        })
    }

    pub(crate) const fn project_id(&self) -> Uuid {
        self.authority.project_id()
    }

    pub(crate) const fn idea_id(&self) -> Uuid {
        self.authority.idea_id()
    }

    /// Reserve the deterministic candidate and next checked epoch without
    /// changing assigned ownership.
    pub(crate) async fn reserve(
        &self,
        request: &ReserveIdeaControllerRequestV1,
    ) -> Result<IdeaControllerControlResultV1, IdeaControlError> {
        let request = request
            .normalized()
            .map_err(IdeaControlError::InvalidRequest)?;
        self.store.lock().await.reserve_idea_controller_v1(
            &self.authority,
            &request,
            self.clock.now(),
        )
    }

    /// Assign only the exact confirmed and unexpired reservation.
    pub(crate) async fn assign_confirmed_guarded<T: AgentTokenLookup>(
        &self,
        reservation: &IdeaControllerReservationV1,
        confirmation: &IdeaControllerLaunchConfirmationV1,
        prospective_token: &str,
        agent_tokens: &RwLock<T>,
    ) -> Result<IdeaControllerControlResultV1, IdeaControlError> {
        if self.authority.actor_kind() != IdeaActorKind::System {
            return Err(IdeaControlError::ForbiddenActor);
        }
        // Global lock order is Store -> A6 read. Remint/revoke requires the A6
        // write guard and therefore cannot overtake this assignment commit.
        let store = self.store.lock().await;
        let a6 = agent_tokens.read().await;
        if a6.session_for_token(prospective_token) != Some(reservation.candidate_session_id) {
            return Err(IdeaControlError::ForbiddenActor);
        }
        let result = store.assign_idea_controller_v1(
            &self.authority,
            reservation,
            confirmation,
            self.clock.now(),
        )?;
        let candidate_grant = BoundControllerWriteAuthority::new(
            result.idea.project_id,
            result.idea.id,
            reservation.candidate_session_id,
            reservation.proposed_epoch,
        )?;
        store.transfer_controller_grant_v1(reservation.base_controller_session_id, candidate_grant);
        drop(a6);
        drop(store);
        Ok(result)
    }

    /// Resolve the exact reservation derived from one intent.
    pub(crate) async fn release_reservation(
        &self,
        request: &ReleaseIdeaControllerReservationRequestV1,
    ) -> Result<IdeaControllerControlResultV1, IdeaControlError> {
        let request = request
            .normalized()
            .map_err(IdeaControlError::InvalidRequest)?;
        self.store
            .lock()
            .await
            .release_idea_controller_reservation_v1(&self.authority, &request, self.clock.now())
    }

    /// Release the exact currently assigned controller.
    pub(crate) async fn release_assigned(
        &self,
        request: &ReleaseAssignedIdeaControllerRequestV1,
    ) -> Result<IdeaControllerControlResultV1, IdeaControlError> {
        let request = request
            .normalized()
            .map_err(IdeaControlError::InvalidRequest)?;
        let store = self.store.lock().await;
        let result = store.release_assigned_idea_controller_v1(
            &self.authority,
            &request,
            self.clock.now(),
        )?;
        let payload = result
            .event
            .controller_control_payload_v1()
            .map_err(IdeaControlError::CorruptStoredEvent)?;
        let rsi_common::types::IdeaControllerControlOperationV1::ReleaseAssigned {
            controller_session_id,
            ..
        } = payload.request.operation
        else {
            return Err(IdeaControlError::CorruptStoredEvent(
                "assigned release returned a non-release event".to_string(),
            ));
        };
        store.remove_controller_grant_v1(controller_session_id);
        Ok(result)
    }
}

/// Minimal controller-authorized mutation surface.
#[derive(Clone)]
#[allow(
    clippy::redundant_pub_crate,
    reason = "explicit crate barrier prevents transport code from widening the controller action set"
)]
pub(crate) struct IdeaControllerHandle {
    store: Arc<Mutex<Store>>,
    authority: BoundControllerWriteAuthority,
    clock: Arc<dyn IdeaControllerClock>,
}

impl IdeaControllerHandle {
    pub(crate) fn from_bound(
        store: Arc<Mutex<Store>>,
        authority: BoundControllerWriteAuthority,
        clock: Arc<dyn IdeaControllerClock>,
    ) -> Self {
        Self {
            store,
            authority,
            clock,
        }
    }

    pub(crate) const fn authority(&self) -> &BoundControllerWriteAuthority {
        &self.authority
    }

    /// Apply only `ChangeProjection` or `TransitionStage` under the exact semantic
    /// controller fence.
    pub(crate) async fn mutate(
        &self,
        request: &MutateIdeaAsControllerRequestV1,
    ) -> Result<IdeaControllerMutationResultV1, IdeaControlError> {
        let request = request
            .normalized()
            .map_err(IdeaControlError::InvalidRequest)?;
        self.store
            .lock()
            .await
            .mutate_idea_as_controller_v1(&self.authority, &request, self.clock.now())
            .map_err(IdeaControlError::for_attributed_caller)
    }

    /// Release only this exact controller assignment.
    pub(crate) async fn self_release(
        &self,
        expected_row_version: i64,
        release_intent_key: impl Into<String>,
    ) -> Result<IdeaControllerControlResultV1, IdeaControlError> {
        let request = ReleaseAssignedIdeaControllerRequestV1 {
            expected_row_version,
            release_intent_key: release_intent_key.into(),
            reason: ControllerReleaseReasonV1::ControllerSelfRelease,
        }
        .normalized()
        .map_err(IdeaControlError::InvalidRequest)?;
        self.store
            .lock()
            .await
            .release_assigned_idea_controller_by_controller_v1(
                &self.authority,
                &request,
                self.clock.now(),
            )
            .map_err(IdeaControlError::for_attributed_caller)
    }
}

impl IdeaControlError {
    /// Collapse semantic/scope details at an attributed boundary.
    fn for_attributed_caller(self) -> Self {
        match self {
            Self::IdeaNotFound
            | Self::ProjectScopeMismatch
            | Self::StaleVersion { .. }
            | Self::StaleControllerEpoch { .. }
            | Self::ControllerMismatch
            | Self::ControllerReservationConflict
            | Self::ControllerReservationNotFound
            | Self::ControllerReservationResolved
            | Self::ControllerReservationExpired => Self::ForbiddenActor,
            other => other,
        }
    }
}

impl IdeaControlHandle {
    /// Bind one Store handle to one project and validated operator identity.
    pub(crate) fn for_operator(
        store: Arc<Mutex<Store>>,
        project_id: Uuid,
        actor_id: impl Into<String>,
    ) -> Result<Self, IdeaControlError> {
        Ok(Self {
            store,
            authority: BoundIdeaWriteAuthority::operator(project_id, actor_id.into())?,
        })
    }

    /// Atomically create one Idea, created event, and optional origin edge.
    pub(crate) async fn create_idea(
        &self,
        request: &CreateIdeaRequestV1,
    ) -> Result<IdeaMutationResultV1, IdeaControlError> {
        let normalized = request
            .normalized()
            .map_err(IdeaControlError::InvalidRequest)?;
        self.store
            .lock()
            .await
            .create_idea_v1(&self.authority, &normalized)
    }

    /// Atomically CAS-update one Idea and append exactly one semantic event.
    pub(crate) async fn mutate_idea(
        &self,
        idea_id: Uuid,
        request: &MutateIdeaRequestV1,
    ) -> Result<IdeaMutationResultV1, IdeaControlError> {
        let normalized = request
            .normalized()
            .map_err(IdeaControlError::InvalidRequest)?;
        self.store
            .lock()
            .await
            .mutate_idea_v1(&self.authority, idea_id, &normalized)
    }

    /// Link one project-owned Issue to an Idea under the operator binding.
    pub(crate) async fn link_issue_to_idea(
        &self,
        request: &LinkIssueToIdeaParams,
    ) -> Result<LinkIssueToIdeaResult, IdeaControlError> {
        self.store
            .lock()
            .await
            .link_issue_to_idea_v1(&self.authority, request)
    }

    /// Return one strictly decoded, bounded page of semantic history.
    pub(crate) async fn list_idea_events(
        &self,
        idea_id: Uuid,
        page: IdeaEventPageRequestV1,
    ) -> Result<IdeaEventPageV1, IdeaControlError> {
        page.validated_limit()
            .map_err(IdeaControlError::InvalidRequest)?;
        self.store
            .lock()
            .await
            .list_idea_events_v1(self.authority.project_id, idea_id, page)
    }
}

// D02 deliberately has no production caller before D03. Keep the complete
// internal surface type-checked and considered reachable without registering
// a transport or a second writer.
#[allow(dead_code)]
async fn d02_internal_surface_compile_anchor(
    store: Arc<Mutex<Store>>,
    project_id: Uuid,
    actor_id: String,
    idea_id: Uuid,
    create: &CreateIdeaRequestV1,
    mutate: &MutateIdeaRequestV1,
    page: IdeaEventPageRequestV1,
) -> Result<(), IdeaControlError> {
    let handle = IdeaControlHandle::for_operator(store, project_id, actor_id)?;
    let _ = handle.create_idea(create).await?;
    let _ = handle.mutate_idea(idea_id, mutate).await?;
    let _ = handle.list_idea_events(idea_id, page).await?;
    Ok(())
}

// D03 intentionally registers no new transport. Keep its complete internal
// authority surface type-checked and considered reachable from one scoped
// anchor until a later slice supplies an authorized consumer.
async fn d03_internal_surface_compile_anchor(
    store: Arc<Mutex<Store>>,
    project_id: Uuid,
    idea_id: Uuid,
    session_id: Uuid,
    controller_epoch: i64,
    expected_row_version: i64,
    mutation: &MutateIdeaAsControllerRequestV1,
) -> Result<(), IdeaControlError> {
    let transfer = IdeaControllerTransferHandle::for_operator(
        Arc::clone(&store),
        project_id,
        idea_id,
        "compile-anchor-operator",
        Arc::new(SystemIdeaControllerClock),
    )
    .await?;
    let _ = transfer
        .release_assigned(&ReleaseAssignedIdeaControllerRequestV1 {
            expected_row_version,
            release_intent_key: "compile-anchor-operator-release".to_string(),
            reason: ControllerReleaseReasonV1::OperatorRelease,
        })
        .await?;
    let controller = IdeaControllerHandle::from_bound(
        store,
        BoundControllerWriteAuthority::new(project_id, idea_id, session_id, controller_epoch)?,
        Arc::new(SystemIdeaControllerClock),
    );
    let _ = controller.authority();
    let _ = controller.mutate(mutation).await?;
    let _ = controller
        .self_release(expected_row_version, "compile-anchor-self-release")
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "internal handle tests use literal-valid fixtures and fail-fast setup"
    )]

    use super::*;
    use rsi_common::types::{
        AutonomyPolicy, Capture, CaptureSourceKind, ContentAddressedRef, Project, Sha256Digest,
    };

    fn project(name: &str) -> Project {
        let now = chrono::Utc::now();
        Project {
            id: Uuid::new_v4(),
            name: name.to_string(),
            path: None,
            description: None,
            color: Project::DEFAULT_COLOR.to_string(),
            context_files: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn capture(project_id: Uuid) -> Capture {
        let digest =
            Sha256Digest::parse(format!("sha256:{}", "a".repeat(64))).expect("valid digest");
        Capture {
            id: Uuid::new_v4(),
            project_id,
            creator_kind: IdeaActorKind::Operator,
            creator_id: "fixture".to_string(),
            captured_at: chrono::Utc::now(),
            source_kind: CaptureSourceKind::OperatorInput,
            raw_content_digest: digest.clone(),
            storage_policy_id: "cas-v1".to_string(),
            content_ref: ContentAddressedRef::for_digest(&digest),
        }
    }

    fn create_request(capture_id: Uuid, key: &str) -> CreateIdeaRequestV1 {
        CreateIdeaRequestV1 {
            idempotency_key: key.to_string(),
            genesis_capture_id: capture_id,
            genesis_span: None,
            slug: key.to_string(),
            sigil: None,
            title: "Bound actor".to_string(),
            description: String::new(),
            portfolio_summary: String::new(),
            priority: 1,
            autonomy_policy: AutonomyPolicy::CaptureOnly,
            integration_target_ref: "refs/heads/main".to_string(),
            program_template_policy_id: None,
            derived_from_idea_id: None,
            artifact_digests: Vec::new(),
            evidence_digests: Vec::new(),
        }
    }

    #[tokio::test]
    async fn idea_control_binds_operator_and_project_before_store_writes() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("open store")));
        let project_a = project("A");
        let project_b = project("B");
        let capture_a = capture(project_a.id);
        {
            let store = store.lock().await;
            store.insert_project(&project_a).expect("insert project A");
            store.insert_project(&project_b).expect("insert project B");
            store
                .insert_d02_capture_fixture(&capture_a)
                .expect("insert capture");
        }

        for actor in ["", " ", "bad\0actor"] {
            assert!(matches!(
                IdeaControlHandle::for_operator(
                    Arc::clone(&store),
                    project_a.id,
                    actor.to_string()
                ),
                Err(IdeaControlError::InvalidRequest(_))
            ));
        }
        assert!(
            IdeaControlHandle::for_operator(Arc::clone(&store), project_a.id, "x".repeat(257))
                .is_err()
        );
        assert!(
            IdeaControlHandle::for_operator(
                Arc::clone(&store),
                Uuid::nil(),
                "operator".to_string()
            )
            .is_err()
        );

        let wrong_project = IdeaControlHandle::for_operator(
            Arc::clone(&store),
            project_b.id,
            "operator-b".to_string(),
        )
        .expect("bind project B");
        assert_eq!(
            wrong_project
                .create_idea(&create_request(capture_a.id, "wrong-project"))
                .await,
            Err(IdeaControlError::ProjectScopeMismatch)
        );
        let idea_count = store
            .lock()
            .await
            .conn
            .query_row("SELECT COUNT(*) FROM ideas", [], |row| row.get::<_, i64>(0))
            .expect("count ideas");
        assert_eq!(idea_count, 0);

        let handle = IdeaControlHandle::for_operator(
            Arc::clone(&store),
            project_a.id,
            "operator-a".to_string(),
        )
        .expect("bind project A");
        let created = handle
            .create_idea(&create_request(capture_a.id, "bound-create"))
            .await
            .expect("create Idea");
        assert_eq!(created.idea.project_id, project_a.id);
        assert_eq!(created.event.project_id, project_a.id);
        assert_eq!(created.event.actor_kind, IdeaActorKind::Operator);
        assert_eq!(created.event.actor_id, "operator-a");
        assert!(created.event.controller_session_id.is_none());
        assert!(created.event.controller_epoch.is_none());
    }
}
