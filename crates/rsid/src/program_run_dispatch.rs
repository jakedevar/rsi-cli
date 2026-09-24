//! Bounded transactional-outbox dispatcher for `ProgramRun` wake actions.

// The accepted symbol inventory requires explicit crate-private publisher and
// dispatcher names even though this module is itself crate-private.
#![allow(clippy::redundant_pub_crate)]

use crate::program_run_control::{BoundProgramRunSchedulerAuthority, ProgramRunControlError};
use crate::session::SessionManager;
use crate::store::Store;
use crate::store::program_runs::{
    PROGRAM_RUN_DISPATCH_EFFECT_LIMIT, ProgramRunExternalReferenceV1,
};
#[cfg(test)]
use crate::store::program_runs::{
    PROGRAM_RUN_DISPATCH_EXACT_CANDIDATE_SQL, program_run_dispatch_scan_sql,
};
use rsi_common::program_runs::{
    ProgramRunActionClaimResultV1, ProgramRunActionKindV1, ProgramRunActionStateV1,
    ProgramRunActionV1,
};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProgramRunPublicationOutcome {
    Published(ProgramRunExternalReferenceV1),
    ExactDuplicate(ProgramRunExternalReferenceV1),
    DefinitiveFailure { class: String, message: String },
    Uncertain { class: String, message: String },
}

#[async_trait::async_trait]
pub(crate) trait ProgramRunActionPublisher: Send + Sync {
    fn supports(&self, kind: ProgramRunActionKindV1) -> bool;
    async fn publish(&self, action: &ProgramRunActionV1) -> ProgramRunPublicationOutcome;
}

pub(crate) struct ScheduledJobWakeAdapter {
    store: Arc<Mutex<Store>>,
}

impl ScheduledJobWakeAdapter {
    pub(crate) const fn new(store: Arc<Mutex<Store>>) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl ProgramRunActionPublisher for ScheduledJobWakeAdapter {
    fn supports(&self, kind: ProgramRunActionKindV1) -> bool {
        kind == ProgramRunActionKindV1::Wake
    }

    async fn publish(&self, action: &ProgramRunActionV1) -> ProgramRunPublicationOutcome {
        if action.action_kind != ProgramRunActionKindV1::Wake {
            return ProgramRunPublicationOutcome::DefinitiveFailure {
                class: "unsupported_action_kind".into(),
                message: "work publisher is not installed".into(),
            };
        }
        let store = self.store.lock().await;
        let run = match store.get_program_run_v1(action.program_run_id) {
            Ok(Some(run)) => run,
            Ok(None) => {
                return ProgramRunPublicationOutcome::DefinitiveFailure {
                    class: "run_not_found".into(),
                    message: "ProgramRun is unavailable".into(),
                };
            }
            Err(_) => {
                return ProgramRunPublicationOutcome::Uncertain {
                    class: "store_unavailable".into(),
                    message: "wake publication state is uncertain".into(),
                };
            }
        };
        match store.insert_or_replay_program_run_wake_job(action, run.project_id) {
            Ok((job, false)) => ProgramRunPublicationOutcome::Published(
                ProgramRunExternalReferenceV1::ScheduledJob(job.id),
            ),
            Ok((job, true)) => ProgramRunPublicationOutcome::ExactDuplicate(
                ProgramRunExternalReferenceV1::ScheduledJob(job.id),
            ),
            Err(error)
                if error
                    .to_string()
                    .contains("program_run_downstream_replay_conflict") =>
            {
                ProgramRunPublicationOutcome::DefinitiveFailure {
                    class: "downstream_replay_conflict".into(),
                    message: "existing wake differs from the durable action envelope".into(),
                }
            }
            Err(_) => ProgramRunPublicationOutcome::Uncertain {
                class: "wake_publish_uncertain".into(),
                message: "wake publication state is uncertain".into(),
            },
        }
    }
}

pub(crate) struct ProgramRunDispatcher {
    manager: Arc<SessionManager>,
    publisher: Arc<dyn ProgramRunActionPublisher>,
    boot_id: Uuid,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProgramRunDispatchTickStats {
    pub(crate) selection_queries: u32,
    pub(crate) loaded_visits: u32,
    pub(crate) loaded_visit_capacity: u32,
    pub(crate) rust_sort_operations: u32,
    pub(crate) processed_visits: u32,
    pub(crate) authority_bind_attempts: u32,
    pub(crate) claim_transactions: u32,
    pub(crate) cursor_transactions: u32,
    pub(crate) external_effect_attempts: u32,
}

impl ProgramRunDispatcher {
    pub(crate) fn new(
        manager: Arc<SessionManager>,
        publisher: Arc<dyn ProgramRunActionPublisher>,
        boot_id: Uuid,
    ) -> Result<Self, ProgramRunControlError> {
        if boot_id.is_nil() {
            return Err(ProgramRunControlError::InvalidRequest);
        }
        Ok(Self {
            manager,
            publisher,
            boot_id,
        })
    }

    pub(crate) async fn tick(&self) {
        let _ = self.tick_with_stats().await;
    }

    pub(crate) async fn tick_with_stats(&self) -> ProgramRunDispatchTickStats {
        let mut stats = ProgramRunDispatchTickStats::default();
        let kinds: Vec<_> = ProgramRunActionKindV1::ALL
            .iter()
            .copied()
            .filter(|kind| self.publisher.supports(*kind))
            .collect();
        if kinds.is_empty() {
            return stats;
        }
        let Ok(batch) = self
            .manager
            .program_run_dispatch_visits(self.boot_id, &kinds)
            .await
        else {
            return stats;
        };
        stats.selection_queries = batch.selection_queries.saturating_add(1);
        stats.loaded_visits = u32::try_from(batch.visits.len()).unwrap_or(u32::MAX);
        stats.loaded_visit_capacity = u32::try_from(batch.visits.capacity()).unwrap_or(u32::MAX);
        let mut remaining_effects = PROGRAM_RUN_DISPATCH_EFFECT_LIMIT;
        let mut processed_count = 0_usize;

        for visit in &batch.visits {
            if visit.may_attempt_external_effect && remaining_effects == 0 {
                break;
            }
            stats.processed_visits = stats.processed_visits.saturating_add(1);
            processed_count = processed_count.saturating_add(1);
            if !visit.dispatchable {
                continue;
            }
            stats.authority_bind_attempts = stats.authority_bind_attempts.saturating_add(1);
            let Ok(authority) = self
                .manager
                .bind_program_run_scheduler_authority(visit.controller_session_id, self.boot_id)
                .await
            else {
                continue;
            };
            stats.claim_transactions = stats.claim_transactions.saturating_add(1);
            let Ok(Some(claim)) = authority
                .claim_dispatch_candidate(&kinds, visit.action_id)
                .await
            else {
                continue;
            };
            if self
                .publish_claim(&authority, claim, remaining_effects > 0)
                .await
            {
                remaining_effects = remaining_effects.saturating_sub(1);
                stats.external_effect_attempts = stats.external_effect_attempts.saturating_add(1);
            }
        }

        if processed_count != 0 {
            stats.cursor_transactions = 1;
            if !matches!(
                self.manager
                    .advance_program_run_dispatch_cursor(&batch, &batch.visits[..processed_count],)
                    .await,
                Ok(true)
            ) {
                tracing::warn!("ProgramRun dispatch cursor advance lost its durable CAS");
            }
        }
        stats
    }

    async fn publish_claim(
        &self,
        authority: &BoundProgramRunSchedulerAuthority,
        claim: ProgramRunActionClaimResultV1,
        allow_external_effect: bool,
    ) -> bool {
        let action = claim.action;
        if action.state == ProgramRunActionStateV1::Acknowledged {
            if let Err(error) = authority
                .acknowledge(action.id, action.claim_generation)
                .await
            {
                tracing::warn!(action_id = %action.id, error = %error,
                    "ProgramRun acknowledged action awaits semantic convergence");
            }
            return false;
        }
        if let Some(semantic_claim) = claim.semantic_claim
            && let Err(error) = authority.claim_action(&semantic_claim).await
        {
            tracing::warn!(action_id = %action.id, error = %error,
                "ProgramRun work claim lost semantic mutation authority");
            return false;
        }
        if action.state == ProgramRunActionStateV1::Published
            && let Some(reference) = action_external_reference(&action)
        {
            match authority
                .bind_reference(action.id, action.claim_generation, reference)
                .await
            {
                Ok(_) => {
                    let _ = authority
                        .acknowledge(action.id, action.claim_generation)
                        .await;
                }
                Err(error) => tracing::warn!(action_id = %action.id, error = %error,
                    "ProgramRun bound publication awaits acknowledgement"),
            }
            return false;
        }
        if !allow_external_effect {
            return false;
        }
        match self.publisher.publish(&action).await {
            ProgramRunPublicationOutcome::Published(reference)
            | ProgramRunPublicationOutcome::ExactDuplicate(reference) => {
                if authority
                    .published(action.id, action.claim_generation)
                    .await
                    .is_ok()
                {
                    match authority
                        .bind_reference(action.id, action.claim_generation, reference)
                        .await
                    {
                        Ok(_) => {
                            let _ = authority
                                .acknowledge(action.id, action.claim_generation)
                                .await;
                        }
                        Err(error) => {
                            tracing::warn!(
                                action_id = %action.id,
                                error = %error,
                                "ProgramRun publication reference remains fenced for reconciliation"
                            );
                        }
                    }
                }
            }
            ProgramRunPublicationOutcome::DefinitiveFailure { class, message } => {
                if let Err(error) = authority
                    .fail(action.id, action.claim_generation, &class, &message)
                    .await
                {
                    tracing::warn!(action_id = %action.id, error_class = %class,
                        error = %error, "ProgramRun definitive publication failure remains claim-fenced");
                }
            }
            ProgramRunPublicationOutcome::Uncertain { class, message } => {
                tracing::warn!(action_id = %action.id, error_class = %class,
                    error = %message, "ProgramRun action remains claim-fenced for reconciliation");
            }
        }
        true
    }
}

fn action_external_reference(action: &ProgramRunActionV1) -> Option<ProgramRunExternalReferenceV1> {
    action
        .external_model_invocation_id
        .map(ProgramRunExternalReferenceV1::ModelInvocation)
        .or_else(|| {
            action
                .external_session_id
                .map(ProgramRunExternalReferenceV1::Session)
        })
        .or_else(|| {
            action
                .scheduled_job_id
                .map(ProgramRunExternalReferenceV1::ScheduledJob)
        })
}

pub(crate) fn spawn_program_run_dispatcher(
    dispatcher: ProgramRunDispatcher,
    cancellation: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                () = cancellation.cancelled() => break,
                _ = interval.tick() => dispatcher.tick().await,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::too_many_lines, clippy::unwrap_used)]

    use super::*;
    use crate::bus::EventBus;
    use crate::config::{Config, RuntimeConfig};
    use crate::idea_control::BoundControllerWriteAuthority;
    use crate::session::types::TrackedSession;
    use crate::store::program_runs::{
        ProgramRunStoreAuthority, ProgramRunTransitionInputV1,
        program_run_scheduler_authority_from_live_grant, test_program_run_controller_authority,
        test_program_run_scheduler_authority,
    };
    use chrono::{SecondsFormat, Utc};
    use rsi_common::program_runs::{
        CommitProgramRunOutputRequestV1, CreateProgramRunRequestV1, ProgramRunActionKindV1,
        ProgramRunBudgetLimitsV1, ProgramRunCursorV1, ProgramRunGateRequirementV1,
        ProgramRunGateResultV1, ProgramRunOperationV1, ProgramRunStatusV1, ProgramRunTemplateV1,
        ProgramRunTransitionRequestV1, RecordProgramRunGateRequestV1,
    };
    use rsi_common::types::{
        AutonomyPolicy, Capture, CaptureSourceKind, ContentAddressedRef, Idea, IdeaActorKind,
        IdeaLifecycle, IdeaStage, Project, Session, SessionStatus, Sha256Digest,
    };
    use rusqlite::StatementStatus;

    #[derive(Clone, Copy)]
    enum CrashBoundary {
        DownstreamCommitted,
        Published,
        Bound,
        Acknowledged,
    }

    type DispatchFixture = (Vec<(Session, Project, Idea)>, Vec<Uuid>, Vec<Uuid>);

    #[derive(Default)]
    struct UncertainPublisher {
        calls: std::sync::Mutex<Vec<Uuid>>,
    }

    #[async_trait::async_trait]
    impl ProgramRunActionPublisher for UncertainPublisher {
        fn supports(&self, kind: ProgramRunActionKindV1) -> bool {
            kind == ProgramRunActionKindV1::Work
        }

        async fn publish(&self, action: &ProgramRunActionV1) -> ProgramRunPublicationOutcome {
            self.calls.lock().unwrap().push(action.id);
            ProgramRunPublicationOutcome::Uncertain {
                class: "transport_uncertain".into(),
                message: "external effect cannot be excluded".into(),
            }
        }
    }

    #[derive(Default)]
    struct DefinitiveFailurePublisher {
        calls: std::sync::Mutex<Vec<Uuid>>,
    }

    #[derive(Default)]
    struct NeverPublishCurrentPublisher {
        calls: std::sync::Mutex<Vec<Uuid>>,
    }

    #[async_trait::async_trait]
    impl ProgramRunActionPublisher for NeverPublishCurrentPublisher {
        fn supports(&self, _kind: ProgramRunActionKindV1) -> bool {
            true
        }

        async fn publish(&self, action: &ProgramRunActionV1) -> ProgramRunPublicationOutcome {
            self.calls.lock().unwrap().push(action.id);
            ProgramRunPublicationOutcome::Uncertain {
                class: "unexpected_v7_publication".into(),
                message: "current reconciliation work must not republish".into(),
            }
        }
    }

    #[async_trait::async_trait]
    impl ProgramRunActionPublisher for DefinitiveFailurePublisher {
        fn supports(&self, kind: ProgramRunActionKindV1) -> bool {
            kind == ProgramRunActionKindV1::Work
        }

        async fn publish(&self, action: &ProgramRunActionV1) -> ProgramRunPublicationOutcome {
            self.calls.lock().unwrap().push(action.id);
            ProgramRunPublicationOutcome::DefinitiveFailure {
                class: "fixture_pre_effect_failure".into(),
                message: "fixture confirms that no external effect occurred".into(),
            }
        }
    }

    fn timestamp() -> chrono::DateTime<Utc> {
        Utc::now()
    }

    fn query_plan(
        connection: &rusqlite::Connection,
        sql: &str,
        parameters: impl rusqlite::Params,
    ) -> Vec<String> {
        connection
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map(parameters, |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    fn assert_exact_dispatch_sql_plans(connection: &rusqlite::Connection) {
        let selection = query_plan(
            connection,
            &program_run_dispatch_scan_sql(
                "subject.not_before>?2 OR (subject.not_before=?2 AND subject.id>?3)",
            ),
            rusqlite::params!["acknowledged", "", "", 32_i64],
        );
        assert!(
            selection
                .iter()
                .any(|line| line.contains("idx_idea_program_run_actions_due")),
            "production selection must use the state/not-before/action index: {selection:?}"
        );
        assert!(
            selection.iter().any(|line| {
                line.contains("idea_program_run_transitions") && line.contains("id=?)")
            }),
            "the current-transition witness must be a transition primary-key lookup: {selection:?}"
        );
        assert!(
            selection.iter().any(|line| {
                line.contains("uidx_idea_program_run_actions_one_active")
                    && line.contains("program_run_id=?)")
            }),
            "the no-active-action witness must use the partial unique index: {selection:?}"
        );
        assert!(
            selection
                .iter()
                .all(|line| !line.contains("SCAN ") && !line.contains("USE TEMP B-TREE")),
            "production selection may not scan or sort durable history: {selection:?}"
        );

        let exact = query_plan(
            connection,
            PROGRAM_RUN_DISPATCH_EXACT_CANDIDATE_SQL,
            rusqlite::params![
                Uuid::new_v4().to_string(),
                Uuid::new_v4().to_string(),
                1_i64,
                Option::<String>::None,
                Option::<String>::None,
                Uuid::new_v4().to_string(),
                1_i64,
                1_i64,
                timestamp().to_rfc3339_opts(SecondsFormat::Nanos, true),
            ],
        );
        assert!(
            exact.iter().any(|line| {
                line.contains("idea_program_run_actions") && line.contains("id=?)")
            }),
            "exact candidate access must begin at the action primary key: {exact:?}"
        );
        assert!(
            exact.iter().any(|line| {
                line.contains("idea_program_run_transitions") && line.contains("id=?)")
            }),
            "exact current acknowledgement must use the transition primary key: {exact:?}"
        );
        assert!(
            exact
                .iter()
                .any(|line| line.contains("uidx_idea_program_run_actions_one_active")),
            "exact current acknowledgement must use the bounded active-action witness: {exact:?}"
        );
        assert!(
            exact
                .iter()
                .all(|line| !line.contains("SCAN ") && !line.contains("USE TEMP B-TREE")),
            "exact candidate classification may not scan or sort durable history: {exact:?}"
        );
    }

    fn insert_dispatch_history(connection: &mut rusqlite::Connection, first: usize, count: usize) {
        let tx = connection.transaction().unwrap();
        for index in first..first + count {
            let idea_id = format!("idea-{index:032}");
            let run_id = format!("run-{index:033}");
            let transition_id = format!("transition-{index:026}");
            let action_id = format!("action-{index:029}");
            let session_id = format!("session-{index:028}");
            tx.execute(
                "INSERT INTO ideas(id,project_id,current_controller_session_id,controller_epoch)
                 VALUES (?1,'project',?2,1)",
                rusqlite::params![idea_id, session_id],
            )
            .unwrap();
            tx.execute(
                "INSERT INTO idea_program_runs(
                    id,project_id,idea_id,controller_session_id,controller_epoch,row_version,status)
                 VALUES (?1,'project',?2,?3,1,2,'retry_pending')",
                rusqlite::params![run_id, idea_id, session_id],
            )
            .unwrap();
            tx.execute(
                "INSERT INTO idea_program_run_transitions(
                    id,program_run_id,resulting_run_version) VALUES (?1,?2,2)",
                rusqlite::params![transition_id, run_id],
            )
            .unwrap();
            tx.execute(
                "INSERT INTO idea_program_run_actions(
                    id,program_run_id,creating_transition_id,action_kind,purpose,
                    controller_session_id,controller_epoch,not_before,state,claim_boot_id,
                    claim_expires_at,claim_run_version,external_model_invocation_id,
                    external_session_id,scheduled_job_id)
                 VALUES (?1,?2,?3,'wake','retry_wake',?4,1,?5,'acknowledged','boot',
                    NULL,2,NULL,NULL,NULL)",
                rusqlite::params![
                    action_id,
                    run_id,
                    transition_id,
                    session_id,
                    format!("{index:020}")
                ],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }

    fn dispatch_selection_statement_work(connection: &rusqlite::Connection) -> (i32, i32, i32) {
        let mut statement = connection
            .prepare(&program_run_dispatch_scan_sql(
                "subject.not_before>?2 OR (subject.not_before=?2 AND subject.id>?3)",
            ))
            .unwrap();
        let rows = statement
            .query_map(rusqlite::params!["acknowledged", "", "", 32_i64], |row| {
                row.get::<_, String>(0)
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(rows.len(), 32);
        (
            statement.get_status(StatementStatus::FullscanStep),
            statement.get_status(StatementStatus::Sort),
            statement.get_status(StatementStatus::VmStep),
        )
    }

    fn exact_dispatch_statement_work(
        connection: &rusqlite::Connection,
        index: usize,
    ) -> (i32, i32, i32) {
        let mut statement = connection
            .prepare(PROGRAM_RUN_DISPATCH_EXACT_CANDIDATE_SQL)
            .unwrap();
        let result = statement
            .query_row(
                rusqlite::params![
                    format!("action-{index:029}"),
                    format!("session-{index:028}"),
                    1_i64,
                    Option::<String>::None,
                    Option::<String>::None,
                    "boot",
                    1_i64,
                    1_i64,
                    timestamp().to_rfc3339_opts(SecondsFormat::Nanos, true),
                ],
                |row| Ok((row.get::<_, bool>(0)?, row.get::<_, bool>(1)?)),
            )
            .unwrap();
        assert_eq!(result, (true, false));
        (
            statement.get_status(StatementStatus::FullscanStep),
            statement.get_status(StatementStatus::Sort),
            statement.get_status(StatementStatus::VmStep),
        )
    }

    #[test]
    fn d05_v8_exact_production_sql_plans_and_history_work_are_bounded() {
        let mut connection = rusqlite::Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE ideas(
                    id TEXT PRIMARY KEY, project_id TEXT,
                    current_controller_session_id TEXT, controller_epoch INTEGER);
                 CREATE TABLE idea_program_runs(
                    id TEXT PRIMARY KEY, project_id TEXT, idea_id TEXT,
                    controller_session_id TEXT, controller_epoch INTEGER,
                    row_version INTEGER, status TEXT);
                 CREATE TABLE idea_program_run_transitions(
                    id TEXT PRIMARY KEY, program_run_id TEXT, resulting_run_version INTEGER);
                 CREATE TABLE idea_program_run_actions(
                    id TEXT PRIMARY KEY, program_run_id TEXT, creating_transition_id TEXT UNIQUE,
                    action_kind TEXT, purpose TEXT, controller_session_id TEXT,
                    controller_epoch INTEGER, not_before TEXT, state TEXT, claim_boot_id TEXT,
                    claim_expires_at TEXT, claim_run_version INTEGER,
                    external_model_invocation_id TEXT, external_session_id TEXT,
                    scheduled_job_id TEXT);
                 CREATE INDEX idx_idea_program_run_actions_due
                    ON idea_program_run_actions(state,not_before,id);
                 CREATE INDEX idx_idea_program_run_actions_expired_claim
                    ON idea_program_run_actions(state,claim_expires_at,id);
                 CREATE UNIQUE INDEX uidx_idea_program_run_actions_one_active
                    ON idea_program_run_actions(program_run_id)
                    WHERE state IN ('reserved','claimed','published');",
            )
            .unwrap();
        insert_dispatch_history(&mut connection, 0, 32);
        assert_exact_dispatch_sql_plans(&connection);
        let small = dispatch_selection_statement_work(&connection);
        let exact_small = exact_dispatch_statement_work(&connection, 0);
        insert_dispatch_history(&mut connection, 32, 10_000);
        let large = dispatch_selection_statement_work(&connection);
        let exact_large = exact_dispatch_statement_work(&connection, 0);
        assert_eq!((small.0, small.1), (0, 0));
        assert_eq!((large.0, large.1), (0, 0));
        assert!(
            large.2 <= small.2 + 256,
            "10,000 durable history rows may add only bounded B-tree depth, not row work: small={small:?} large={large:?}"
        );
        assert_eq!((exact_small.0, exact_small.1), (0, 0));
        assert_eq!((exact_large.0, exact_large.1), (0, 0));
        assert!(
            exact_large.2 <= exact_small.2 + 256,
            "exact PK reconciliation may add only bounded B-tree depth, not history work: small={exact_small:?} large={exact_large:?}"
        );
    }

    fn seed_retry_pending_wake(
        path: &std::path::Path,
        boundary: CrashBoundary,
    ) -> (Session, Project, Idea, Uuid, Uuid) {
        let store = Store::open(path).expect("open outbox seed store");
        let now = timestamp();
        let project = Project {
            id: Uuid::new_v4(),
            name: format!("D05 dispatcher restart {}", Uuid::new_v4()),
            path: None,
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: now,
            updated_at: now,
        };
        store.insert_project(&project).expect("insert project");
        let mut session = crate::store::tests::make_test_session();
        session.project_id = Some(project.id);
        session.status = SessionStatus::Running;
        store
            .insert_session(&session)
            .expect("insert controller session");
        let digest = Sha256Digest::parse(format!("sha256:{}", "d".repeat(64))).unwrap();
        let capture = Capture {
            id: Uuid::new_v4(),
            project_id: project.id,
            creator_kind: IdeaActorKind::Operator,
            creator_id: "d05-dispatcher-test".into(),
            captured_at: now,
            source_kind: CaptureSourceKind::OperatorInput,
            raw_content_digest: digest.clone(),
            storage_policy_id: "cas-v1".into(),
            content_ref: ContentAddressedRef::for_digest(&digest),
        };
        let idea = Idea {
            id: Uuid::new_v4(),
            project_id: project.id,
            slug: format!("d05-dispatcher-{}", Uuid::new_v4()),
            sigil: None,
            genesis_capture_id: capture.id,
            genesis_span_start: None,
            genesis_span_end: None,
            genesis_span_digest: None,
            title: "Dispatcher restart".into(),
            description: "outbox crash convergence".into(),
            portfolio_summary: "D05".into(),
            lifecycle: IdeaLifecycle::Open,
            stage: IdeaStage::Captured,
            priority: 1,
            autonomy_policy: AutonomyPolicy::CaptureOnly,
            integration_target_ref: "refs/heads/main".into(),
            program_template_policy_id: Some("d05-v1".into()),
            current_controller_session_id: Some(session.id),
            controller_epoch: 1,
            row_version: 0,
            next_event_sequence: 1,
            created_at: now,
            updated_at: now,
            terminal_at: None,
            superseded_at: None,
        };
        store
            .insert_d01_idea_fixture(&capture, &idea)
            .expect("insert idea");
        let created = store
            .create_program_run_v1(
                &ProgramRunStoreAuthority::operator(),
                &CreateProgramRunRequestV1 {
                    idea_id: idea.id,
                    expected_idea_row_version: 0,
                    idempotency_key: format!("d05-dispatcher-create:{}", idea.id),
                    template: ProgramRunTemplateV1 {
                        template_key: "d05-dispatcher".into(),
                        template_version: 1,
                        cursors: vec![ProgramRunCursorV1 {
                            key: "implement".into(),
                            phase: "implementation".into(),
                            required_gates: vec![ProgramRunGateRequirementV1 {
                                gate_key: "review".into(),
                                policy_key: "review-v1".into(),
                                policy_version: 1,
                            }],
                            revision_target_ordinal: Some(0),
                        }],
                        budgets: ProgramRunBudgetLimitsV1 {
                            productive_transitions: 256,
                            work_attempts: 64,
                            launch_retries: 64,
                            revisions: 64,
                            wake_reservations: 64,
                            action_publication_retries: 64,
                        },
                        locks: Vec::new(),
                        max_publication_attempts: 4,
                    },
                },
            )
            .expect("create run");
        let controller = test_program_run_controller_authority(session.id, 1);
        let scheduler = test_program_run_scheduler_authority(session.id, 1);
        let _ready = store
            .apply_program_run_transition_v1(
                &controller,
                &ProgramRunTransitionInputV1::Simple(ProgramRunTransitionRequestV1 {
                    program_run_id: created.run.id,
                    expected_run_version: created.run.row_version,
                    expected_idea_version: created.run.idea_row_version,
                    operation: ProgramRunOperationV1::LocksGranted,
                    idempotency_key: format!("d05-dispatcher-ready:{}", created.run.id),
                    reason: None,
                }),
            )
            .expect("ready run");
        let boot_id = store.program_run_boot_id();
        let now = timestamp();
        let work = store
            .claim_due_program_run_actions_v1(
                &scheduler,
                boot_id,
                &[ProgramRunActionKindV1::Work],
                now,
                1,
            )
            .expect("claim work")
            .pop()
            .expect("work action");
        let running = store
            .apply_program_run_transition_v1(
                &controller,
                &ProgramRunTransitionInputV1::ClaimAction(
                    work.semantic_claim.clone().expect("semantic claim"),
                ),
            )
            .expect("record work claim");
        store
            .record_program_run_publication_v1(
                &scheduler,
                work.action.id,
                boot_id,
                work.action.claim_generation,
                now,
            )
            .expect("publish work");
        store
            .bind_program_run_external_reference_v1(
                &scheduler,
                work.action.id,
                boot_id,
                work.action.claim_generation,
                ProgramRunExternalReferenceV1::Session(session.id),
                now,
            )
            .expect("bind work session");
        let attempt = store
            .get_program_run_operational_status_v1(running.run.id, true, now)
            .expect("running status")
            .current_attempt
            .expect("work attempt");
        let awaiting = store
            .apply_program_run_transition_v1(
                &controller,
                &ProgramRunTransitionInputV1::CommitOutput(CommitProgramRunOutputRequestV1 {
                    program_run_id: running.run.id,
                    attempt_id: attempt.id,
                    expected_run_version: running.run.row_version,
                    expected_idea_version: running.run.idea_row_version,
                    idempotency_key: format!("d05-dispatcher-output:{}", running.run.id),
                    output_ref: "cas://d05-dispatcher-output".into(),
                    output_digest: format!("sha256:{}", "e".repeat(64)),
                }),
            )
            .expect("commit output");
        let retrying = store
            .apply_program_run_transition_v1(
                &controller,
                &ProgramRunTransitionInputV1::Gate(RecordProgramRunGateRequestV1 {
                    program_run_id: awaiting.run.id,
                    expected_run_version: awaiting.run.row_version,
                    expected_idea_version: awaiting.run.idea_row_version,
                    idempotency_key: format!("d05-dispatcher-failed-gate:{}", awaiting.run.id),
                    gate_key: "review".into(),
                    result: ProgramRunGateResultV1::Failed,
                    policy_key: "review-v1".into(),
                    policy_version: 1,
                    evidence_ref: "cas://d05-dispatcher-review".into(),
                    evidence_digest: format!("sha256:{}", "f".repeat(64)),
                }),
            )
            .expect("fail gate");
        assert_eq!(retrying.run.status, ProgramRunStatusV1::RetryPending);
        let now = timestamp();
        let wake = store
            .claim_due_program_run_actions_v1(
                &scheduler,
                boot_id,
                &[ProgramRunActionKindV1::Wake],
                now,
                1,
            )
            .expect("claim wake")
            .pop()
            .expect("wake action")
            .action;
        let (job, replayed) = store
            .insert_or_replay_program_run_wake_job(&wake, project.id)
            .expect("commit downstream wake");
        assert!(!replayed);
        if matches!(
            boundary,
            CrashBoundary::Published | CrashBoundary::Bound | CrashBoundary::Acknowledged
        ) {
            store
                .record_program_run_publication_v1(
                    &scheduler,
                    wake.id,
                    boot_id,
                    wake.claim_generation,
                    now,
                )
                .expect("record publication");
        }
        if matches!(boundary, CrashBoundary::Bound | CrashBoundary::Acknowledged) {
            store
                .bind_program_run_external_reference_v1(
                    &scheduler,
                    wake.id,
                    boot_id,
                    wake.claim_generation,
                    ProgramRunExternalReferenceV1::ScheduledJob(job.id),
                    now,
                )
                .expect("bind wake job");
        }
        if matches!(boundary, CrashBoundary::Acknowledged) {
            store
                .acknowledge_program_run_action_v1(
                    &scheduler,
                    wake.id,
                    boot_id,
                    wake.claim_generation,
                    None,
                    now,
                )
                .expect("ack wake before semantic transition");
        }
        (session, project, idea, retrying.run.id, job.id)
    }

    fn seed_ready_work_runs(
        path: &std::path::Path,
        count: usize,
    ) -> (Vec<(Session, Project, Idea)>, Vec<Uuid>, Uuid) {
        let store = Store::open(path).expect("open bounded publication seed store");
        let now = timestamp();
        let digest = Sha256Digest::parse(format!("sha256:{}", "c".repeat(64))).unwrap();
        let mut controllers = Vec::with_capacity(count);
        let mut run_ids = Vec::with_capacity(count);
        let mut deferred_action_id = Uuid::nil();
        for index in 0..count {
            let project = Project {
                id: Uuid::new_v4(),
                name: format!("D05 bounded publication {index}"),
                path: None,
                description: None,
                color: Project::DEFAULT_COLOR.into(),
                context_files: None,
                created_at: now,
                updated_at: now,
            };
            store.insert_project(&project).unwrap();
            let mut session = crate::store::tests::make_test_session();
            session.project_id = Some(project.id);
            session.status = SessionStatus::Running;
            store.insert_session(&session).unwrap();
            let capture = Capture {
                id: Uuid::new_v4(),
                project_id: project.id,
                creator_kind: IdeaActorKind::Operator,
                creator_id: "d05-v4-dispatcher-test".into(),
                captured_at: now,
                source_kind: CaptureSourceKind::OperatorInput,
                raw_content_digest: digest.clone(),
                storage_policy_id: "cas-v1".into(),
                content_ref: ContentAddressedRef::for_digest(&digest),
            };
            let idea = Idea {
                id: Uuid::new_v4(),
                project_id: project.id,
                slug: format!("d05-v4-dispatcher-{index}-{}", Uuid::new_v4()),
                sigil: None,
                genesis_capture_id: capture.id,
                genesis_span_start: None,
                genesis_span_end: None,
                genesis_span_digest: None,
                title: format!("Bounded publication {index}"),
                description: "finite uncertain publication".into(),
                portfolio_summary: "D05 V4".into(),
                lifecycle: IdeaLifecycle::Open,
                stage: IdeaStage::Captured,
                priority: 1,
                autonomy_policy: AutonomyPolicy::CaptureOnly,
                integration_target_ref: "refs/heads/main".into(),
                program_template_policy_id: Some("d05-v4".into()),
                current_controller_session_id: Some(session.id),
                controller_epoch: 1,
                row_version: 0,
                next_event_sequence: 1,
                created_at: now,
                updated_at: now,
                terminal_at: None,
                superseded_at: None,
            };
            store.insert_d01_idea_fixture(&capture, &idea).unwrap();
            let created = store
                .create_program_run_v1(
                    &ProgramRunStoreAuthority::operator(),
                    &CreateProgramRunRequestV1 {
                        idea_id: idea.id,
                        expected_idea_row_version: 0,
                        idempotency_key: format!("d05-v4-dispatcher-create-{index}"),
                        template: ProgramRunTemplateV1 {
                            template_key: "d05-v4-dispatcher".into(),
                            template_version: 1,
                            cursors: vec![ProgramRunCursorV1 {
                                key: "implement".into(),
                                phase: "implementation".into(),
                                required_gates: Vec::new(),
                                revision_target_ordinal: None,
                            }],
                            budgets: ProgramRunBudgetLimitsV1 {
                                productive_transitions: 8,
                                work_attempts: 2,
                                launch_retries: 1,
                                revisions: 1,
                                wake_reservations: 1,
                                action_publication_retries: 1,
                            },
                            locks: Vec::new(),
                            max_publication_attempts: 2,
                        },
                    },
                )
                .unwrap();
            let ready = store
                .apply_program_run_transition_v1(
                    &test_program_run_controller_authority(session.id, 1),
                    &ProgramRunTransitionInputV1::Simple(ProgramRunTransitionRequestV1 {
                        program_run_id: created.run.id,
                        expected_run_version: created.run.row_version,
                        expected_idea_version: created.run.idea_row_version,
                        operation: ProgramRunOperationV1::LocksGranted,
                        idempotency_key: format!("d05-v4-dispatcher-ready-{index}"),
                        reason: None,
                    }),
                )
                .unwrap();
            let action = store
                .get_program_run_operational_status_v1(ready.run.id, true, now)
                .unwrap()
                .active_action
                .unwrap();
            if index + 1 == count {
                deferred_action_id = action.id;
                store
                    .conn
                    .execute(
                        "UPDATE idea_program_run_actions SET not_before=?1 WHERE id=?2",
                        rusqlite::params![
                            (now + chrono::Duration::hours(1))
                                .to_rfc3339_opts(SecondsFormat::Nanos, true),
                            action.id.to_string()
                        ],
                    )
                    .unwrap();
            }
            run_ids.push(ready.run.id);
            controllers.push((session, project, idea));
        }
        (controllers, run_ids, deferred_action_id)
    }

    fn seed_stale_published_reference_prefix(
        path: &std::path::Path,
        count: usize,
    ) -> DispatchFixture {
        let (controllers, run_ids, _) = seed_ready_work_runs(path, count);
        let store = Store::open(path).unwrap();
        let now = timestamp();
        store
            .conn
            .execute(
                "UPDATE idea_program_run_actions SET not_before=?1 WHERE state='reserved'",
                [now.to_rfc3339_opts(SecondsFormat::Nanos, true)],
            )
            .unwrap();
        let mut action_ids = Vec::with_capacity(count);
        for (session, _, _) in &controllers {
            let scheduler = test_program_run_scheduler_authority(session.id, 1);
            let claim = store
                .claim_due_program_run_actions_v1(
                    &scheduler,
                    store.program_run_boot_id(),
                    &[ProgramRunActionKindV1::Work],
                    now,
                    1,
                )
                .unwrap()
                .pop()
                .unwrap();
            let running = store
                .apply_program_run_transition_v1(
                    &scheduler,
                    &ProgramRunTransitionInputV1::ClaimAction(
                        claim.semantic_claim.clone().unwrap(),
                    ),
                )
                .unwrap();
            store
                .record_program_run_publication_v1(
                    &scheduler,
                    claim.action.id,
                    store.program_run_boot_id(),
                    claim.action.claim_generation,
                    now,
                )
                .unwrap();
            store
                .bind_program_run_external_reference_v1(
                    &scheduler,
                    claim.action.id,
                    store.program_run_boot_id(),
                    claim.action.claim_generation,
                    ProgramRunExternalReferenceV1::Session(session.id),
                    now,
                )
                .unwrap();
            store
                .conn
                .execute(
                    "UPDATE idea_program_runs SET row_version=row_version+1,updated_at=?1
                     WHERE id=?2",
                    rusqlite::params![
                        now.to_rfc3339_opts(SecondsFormat::Nanos, true),
                        running.run.id.to_string()
                    ],
                )
                .unwrap();
            action_ids.push(claim.action.id);
        }
        (controllers, run_ids, action_ids)
    }

    fn seed_acknowledged_semantic_prefix(path: &std::path::Path, count: usize) -> DispatchFixture {
        let mut controllers = Vec::with_capacity(count);
        let mut run_ids = Vec::with_capacity(count);
        let mut wake_ids = Vec::with_capacity(count);
        let base = timestamp() - chrono::Duration::hours(1);
        for index in 0..count {
            let (session, project, idea, run_id, _) =
                seed_retry_pending_wake(path, CrashBoundary::Acknowledged);
            let store = Store::open(path).unwrap();
            let wake_id: String = store
                .conn
                .query_row(
                    "SELECT id FROM idea_program_run_actions
                     WHERE program_run_id=?1 AND action_kind='wake' AND state='acknowledged'
                     ORDER BY acknowledged_at DESC,id DESC LIMIT 1",
                    [run_id.to_string()],
                    |row| row.get(0),
                )
                .unwrap();
            let ordered_at = (base
                + chrono::Duration::nanoseconds(i64::try_from(index).unwrap_or(i64::MAX)))
            .to_rfc3339_opts(SecondsFormat::Nanos, true);
            store
                .conn
                .execute(
                    "UPDATE idea_program_run_actions SET not_before=?1
                     WHERE program_run_id=?2 AND state='acknowledged'",
                    rusqlite::params![ordered_at, run_id.to_string()],
                )
                .unwrap();
            if index + 1 < count {
                store
                    .conn
                    .execute(
                        "UPDATE idea_program_run_budgets SET limit_value=used_value
                         WHERE program_run_id=?1 AND dimension='productive_transitions'",
                        [run_id.to_string()],
                    )
                    .unwrap();
            }
            controllers.push((session, project, idea));
            run_ids.push(run_id);
            wake_ids.push(Uuid::parse_str(&wake_id).unwrap());
        }
        (controllers, run_ids, wake_ids)
    }

    async fn expire_uncertain_claims(
        manager: &Arc<SessionManager>,
        deferred_action_id: Uuid,
        include_deferred: bool,
    ) {
        let store = manager.store.lock().await;
        let expiry = (timestamp() - chrono::Duration::seconds(1))
            .to_rfc3339_opts(SecondsFormat::Nanos, true);
        if include_deferred {
            store
                .conn
                .execute(
                    "UPDATE idea_program_run_actions SET claim_expires_at=?1
                     WHERE state IN ('claimed','published')",
                    [expiry],
                )
                .unwrap();
        } else {
            store
                .conn
                .execute(
                    "UPDATE idea_program_run_actions SET claim_expires_at=?1
                     WHERE state IN ('claimed','published') AND id!=?2",
                    rusqlite::params![expiry, deferred_action_id.to_string()],
                )
                .unwrap();
        }
    }

    fn reopen_dispatch_manager(
        db_path: &std::path::Path,
        root: &std::path::Path,
    ) -> Arc<SessionManager> {
        Arc::new(
            SessionManager::new(
                Arc::new(EventBus::new(16)),
                Store::open(db_path).unwrap(),
                false,
                root.join("daemon.sock"),
                None,
                Vec::new(),
                RuntimeConfig::from_config(&Config::from_env()),
                root.join("sandboxes"),
            )
            .unwrap(),
        )
    }

    async fn activate_dispatch_controllers(
        manager: &Arc<SessionManager>,
        controllers: &[(Session, Project, Idea)],
        token_generation: &str,
    ) {
        for (session, project, idea) in controllers {
            manager
                .active
                .write()
                .await
                .insert(session.id, TrackedSession::new_for_test(session.clone()));
            manager
                .register_agent_token(
                    format!("d05-v6-{token_generation}-token:{}", session.id),
                    session.id,
                )
                .await;
            manager.store.lock().await.install_controller_grant_v1(
                BoundControllerWriteAuthority::new(project.id, idea.id, session.id, 1).unwrap(),
            );
        }
    }

    type ForeignDurableFact = (String, String, i64, i64, String, i64, i64, i64);

    fn foreign_durable_facts(store: &Store, eligible_action_id: Uuid) -> Vec<ForeignDurableFact> {
        let mut statement = store
            .conn
            .prepare(
                "SELECT action.id,action.state,action.claim_generation,
                        action.publication_attempts,budget.dimension,
                        budget.reserved_value,budget.used_value,budget.row_version
                 FROM idea_program_run_actions action
                 JOIN idea_program_run_budgets budget
                   ON budget.program_run_id=action.program_run_id
                 WHERE action.id!=?1
                 ORDER BY action.id,budget.dimension",
            )
            .unwrap();
        statement
            .query_map([eligible_action_id.to_string()], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    #[test]
    fn d05_v5_due_claim_sql_scope_precedes_limit_and_preserves_foreign_facts() {
        for scenario in [
            "stale_controller",
            "foreign_project_idea",
            "same_session_epoch_foreign_grant_scope",
            "stale_epoch",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let db_path = dir.path().join("rsi.db");
            let (mut controllers, run_ids, eligible_action_id) = seed_ready_work_runs(&db_path, 65);
            let (session, project, idea) = controllers.pop().unwrap();
            let eligible_run_id = *run_ids.last().unwrap();
            let store = Store::open(&db_path).unwrap();
            let now = timestamp();
            let earlier =
                (now - chrono::Duration::minutes(2)).to_rfc3339_opts(SecondsFormat::Nanos, true);
            let later =
                (now - chrono::Duration::minutes(1)).to_rfc3339_opts(SecondsFormat::Nanos, true);
            store
                .conn
                .execute(
                    "UPDATE idea_program_run_actions SET not_before=?1",
                    [&earlier],
                )
                .unwrap();
            store
                .conn
                .execute(
                    "UPDATE idea_program_run_actions SET not_before=?1 WHERE id=?2",
                    rusqlite::params![later, eligible_action_id.to_string()],
                )
                .unwrap();

            let epoch = match scenario {
                "same_session_epoch_foreign_grant_scope" => {
                    store
                        .conn
                        .execute(
                            "UPDATE idea_program_run_actions
                             SET controller_session_id=?1,controller_epoch=1 WHERE id!=?2",
                            rusqlite::params![
                                session.id.to_string(),
                                eligible_action_id.to_string()
                            ],
                        )
                        .unwrap();
                    1
                }
                "stale_epoch" => {
                    store
                        .conn
                        .execute(
                            "UPDATE idea_program_run_actions
                             SET controller_session_id=?1,controller_epoch=1 WHERE id!=?2",
                            rusqlite::params![
                                session.id.to_string(),
                                eligible_action_id.to_string()
                            ],
                        )
                        .unwrap();
                    store
                        .conn
                        .execute(
                            "UPDATE idea_program_run_actions SET controller_epoch=2 WHERE id=?1",
                            [eligible_action_id.to_string()],
                        )
                        .unwrap();
                    store
                        .conn
                        .execute(
                            "UPDATE idea_program_runs SET controller_epoch=2 WHERE id=?1",
                            [eligible_run_id.to_string()],
                        )
                        .unwrap();
                    store
                        .conn
                        .execute(
                            "UPDATE ideas SET controller_epoch=2 WHERE id=?1",
                            [idea.id.to_string()],
                        )
                        .unwrap();
                    2
                }
                "stale_controller" | "foreign_project_idea" => 1,
                _ => unreachable!(),
            };
            let grant =
                BoundControllerWriteAuthority::new(project.id, idea.id, session.id, epoch).unwrap();
            let authority = program_run_scheduler_authority_from_live_grant(&grant);
            let before = foreign_durable_facts(&store, eligible_action_id);
            assert_eq!(before.len(), 64 * 6, "scenario {scenario} fixture floor");

            let claims = store
                .claim_due_program_run_actions_v1(
                    &authority,
                    store.program_run_boot_id(),
                    &[ProgramRunActionKindV1::Work],
                    now,
                    1,
                )
                .unwrap();
            assert_eq!(claims.len(), 1, "scenario {scenario} must make progress");
            assert_eq!(claims[0].action.id, eligible_action_id);
            assert_eq!(
                foreign_durable_facts(&store, eligible_action_id),
                before,
                "scenario {scenario} must not mutate foreign actions, generations, or budgets"
            );
        }
    }

    #[tokio::test]
    async fn d05_v6_dispatch_progress_survives_dispatcher_manager_and_boot_recreation() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("rsi.db");
        let (controllers, _run_ids, deferred_action_id) = seed_ready_work_runs(&db_path, 65);
        Store::open(&db_path)
            .unwrap()
            .conn
            .execute(
                "UPDATE idea_program_run_actions SET not_before=?1 WHERE id=?2",
                rusqlite::params![
                    timestamp().to_rfc3339_opts(SecondsFormat::Nanos, true),
                    deferred_action_id.to_string()
                ],
            )
            .unwrap();
        let publisher = Arc::new(DefinitiveFailurePublisher::default());

        let manager = reopen_dispatch_manager(&db_path, dir.path());
        activate_dispatch_controllers(&manager, &controllers, "first-boot").await;
        ProgramRunDispatcher::new(
            Arc::clone(&manager),
            publisher.clone(),
            manager.program_run_boot_id,
        )
        .unwrap()
        .tick()
        .await;
        assert_eq!(publisher.calls.lock().unwrap().len(), 32);

        // Recreate the process-local dispatcher before the second tick. V5
        // reset its cursor here and revisited the already-failed prefix.
        ProgramRunDispatcher::new(
            Arc::clone(&manager),
            publisher.clone(),
            manager.program_run_boot_id,
        )
        .unwrap()
        .tick()
        .await;
        assert_eq!(publisher.calls.lock().unwrap().len(), 64);

        // Cross the real manager/boot boundary before the third tick, including
        // production startup reconciliation and fresh A6/grant incarnations.
        drop(manager);
        let manager = reopen_dispatch_manager(&db_path, dir.path());
        activate_dispatch_controllers(&manager, &controllers, "replacement-boot").await;
        manager.reconcile_program_runs_at_startup().await.unwrap();
        ProgramRunDispatcher::new(
            Arc::clone(&manager),
            publisher.clone(),
            manager.program_run_boot_id,
        )
        .unwrap()
        .tick()
        .await;

        let calls = publisher.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 65, "ceil(65 / 32) ticks is the finite bound");
        let unique: std::collections::HashSet<_> = calls.iter().copied().collect();
        assert_eq!(
            unique.len(),
            65,
            "each live authority advances exactly once"
        );
        let store = manager.store.lock().await;
        let (attempts, retried): (i64, i64) = store
            .conn
            .query_row(
                "SELECT SUM(publication_attempts),
                        SUM(CASE WHEN publication_attempts>1 THEN 1 ELSE 0 END)
                 FROM idea_program_run_actions",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let retry_budget_used: i64 = store
            .conn
            .query_row(
                "SELECT SUM(used_value) FROM idea_program_run_budgets
                 WHERE dimension='action_publication_retries'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(store);
        assert_eq!((attempts, retried, retry_budget_used), (65, 0, 0));
    }

    #[tokio::test]
    async fn d05_v6_repeated_boot_reclaim_prioritizes_never_claimed_authorities() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("rsi.db");
        let (controllers, _run_ids, deferred_action_id) = seed_ready_work_runs(&db_path, 65);
        Store::open(&db_path)
            .unwrap()
            .conn
            .execute(
                "UPDATE idea_program_run_actions SET not_before=?1 WHERE id=?2",
                rusqlite::params![
                    timestamp().to_rfc3339_opts(SecondsFormat::Nanos, true),
                    deferred_action_id.to_string()
                ],
            )
            .unwrap();
        let publisher = Arc::new(UncertainPublisher::default());

        for generation in 0..3 {
            let manager = reopen_dispatch_manager(&db_path, dir.path());
            activate_dispatch_controllers(
                &manager,
                &controllers,
                &format!("uncertain-boot-{generation}"),
            )
            .await;
            if generation > 0 {
                manager.reconcile_program_runs_at_startup().await.unwrap();
            }
            ProgramRunDispatcher::new(
                Arc::clone(&manager),
                publisher.clone(),
                manager.program_run_boot_id,
            )
            .unwrap()
            .tick()
            .await;
            drop(manager);
        }

        let calls = publisher.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 96, "each boot retains the 32-claim cap");
        assert_eq!(
            calls
                .iter()
                .take(65)
                .copied()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            65,
            "every never-claimed authority is offered before any reclaimed retry"
        );
        let store = Store::open(&db_path).unwrap();
        let (attempts, retried): (i64, i64) = store
            .conn
            .query_row(
                "SELECT SUM(publication_attempts),
                        SUM(CASE WHEN publication_attempts=2 THEN 1 ELSE 0 END)
                 FROM idea_program_run_actions",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let retry_budget_used: i64 = store
            .conn
            .query_row(
                "SELECT SUM(used_value) FROM idea_program_run_budgets
                 WHERE dimension='action_publication_retries'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!((attempts, retried, retry_budget_used), (96, 31, 31));
    }

    #[tokio::test]
    async fn d05_v6_sparse_scan_skips_empty_failed_removed_and_replaced_prefixes() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("rsi.db");
        let (controllers, _run_ids, _) = seed_ready_work_runs(&db_path, 65);
        let mut sorted = controllers.clone();
        sorted.sort_by_key(|(session, project, idea)| (project.id, idea.id, session.id, 1_u64));

        let manager = reopen_dispatch_manager(&db_path, dir.path());
        let publisher = Arc::new(DefinitiveFailurePublisher::default());
        let dispatcher = ProgramRunDispatcher::new(
            Arc::clone(&manager),
            publisher.clone(),
            manager.program_run_boot_id,
        )
        .unwrap();
        dispatcher.tick().await;
        assert!(publisher.calls.lock().unwrap().is_empty());

        activate_dispatch_controllers(&manager, &controllers, "initial-provider").await;
        let future =
            (timestamp() + chrono::Duration::hours(1)).to_rfc3339_opts(SecondsFormat::Nanos, true);
        let now = timestamp().to_rfc3339_opts(SecondsFormat::Nanos, true);
        {
            let store = manager.store.lock().await;
            store
                .conn
                .execute(
                    "UPDATE idea_program_run_actions SET not_before=?1",
                    [&future],
                )
                .unwrap();
            for (session, _, _) in sorted.iter().skip(33) {
                store
                    .conn
                    .execute(
                        "UPDATE idea_program_run_actions SET not_before=?1
                         WHERE controller_session_id=?2",
                        rusqlite::params![now, session.id.to_string()],
                    )
                    .unwrap();
            }
            for (session, _, _) in sorted.iter().skip(4).take(5) {
                store
                    .conn
                    .execute(
                        "UPDATE idea_program_run_actions SET state='failed',
                         last_error_class='fixture_prior_failure'
                         WHERE controller_session_id=?1",
                        [session.id.to_string()],
                    )
                    .unwrap();
            }
        }

        for (session, _, _) in sorted.iter().take(4) {
            manager.active.write().await.remove(&session.id);
            manager.revoke_agent_token_for_session(session.id).await;
            manager
                .store
                .lock()
                .await
                .remove_controller_grant_v1(session.id);
        }

        // Re-establish one due controller under the same Session id with a new
        // provider token and grant incarnation, exactly as continuation/remint
        // reconstruction does. The dispatcher must bind only the fresh facts.
        let (session, project, idea) = sorted.last().unwrap();
        manager.active.write().await.remove(&session.id);
        manager
            .active
            .write()
            .await
            .insert(session.id, TrackedSession::new_for_test(session.clone()));
        manager.revoke_agent_token_for_session(session.id).await;
        manager
            .register_agent_token(
                format!("d05-v6-replacement-provider-token:{}", session.id),
                session.id,
            )
            .await;
        manager.store.lock().await.install_controller_grant_v1(
            BoundControllerWriteAuthority::new(project.id, idea.id, session.id, 1).unwrap(),
        );

        dispatcher.tick().await;
        assert_eq!(
            publisher.calls.lock().unwrap().len(),
            32,
            "only actual claims consume the global capacity"
        );
        dispatcher.tick().await;
        assert_eq!(
            publisher.calls.lock().unwrap().len(),
            32,
            "failed actions must not publish or charge twice"
        );
        let store = manager.store.lock().await;
        let (attempts, retried): (i64, i64) = store
            .conn
            .query_row(
                "SELECT SUM(publication_attempts),
                        SUM(CASE WHEN publication_attempts>1 THEN 1 ELSE 0 END)
                 FROM idea_program_run_actions",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let retry_budget_used: i64 = store
            .conn
            .query_row(
                "SELECT SUM(used_value) FROM idea_program_run_budgets
                 WHERE dimension='action_publication_retries'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(store);
        assert_eq!((attempts, retried, retry_budget_used), (32, 0, 0));
    }

    #[allow(clippy::significant_drop_tightening)]
    #[tokio::test]
    async fn d05_v7_unchanged_current_prefix_rotates_durably_without_republication() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("rsi.db");
        let (published_controllers, published_runs, published_actions) =
            seed_stale_published_reference_prefix(&db_path, 33);
        let (acknowledged_controllers, acknowledged_runs, acknowledged_wakes) =
            seed_acknowledged_semantic_prefix(&db_path, 33);
        let mut controllers = published_controllers;
        controllers.extend(acknowledged_controllers);
        let publisher = Arc::new(NeverPublishCurrentPublisher::default());

        let manager = reopen_dispatch_manager(&db_path, dir.path());
        activate_dispatch_controllers(&manager, &controllers, "v7-current-prefix").await;
        manager.reconcile_program_runs_at_startup().await.unwrap();
        {
            let store = manager.store.lock().await;
            for run_id in &published_runs {
                store
                    .conn
                    .execute(
                        "UPDATE idea_program_runs SET row_version=row_version+1,updated_at=?1
                         WHERE id=?2",
                        rusqlite::params![
                            timestamp().to_rfc3339_opts(SecondsFormat::Nanos, true),
                            run_id.to_string()
                        ],
                    )
                    .unwrap();
            }
        }
        let stale_pre_tick_batch = manager
            .store
            .lock()
            .await
            .select_program_run_dispatch_visits_v1(
                manager.program_run_boot_id,
                &[ProgramRunActionKindV1::Work, ProgramRunActionKindV1::Wake],
                timestamp(),
                128,
            )
            .unwrap();
        let mut tick_stats = vec![
            ProgramRunDispatcher::new(
                Arc::clone(&manager),
                publisher.clone(),
                manager.program_run_boot_id,
            )
            .unwrap()
            .tick_with_stats()
            .await,
        ];
        {
            let store = manager.store.lock().await;
            assert!(
                !store
                    .advance_program_run_dispatch_cursor_v1(
                        &stale_pre_tick_batch,
                        &[],
                        timestamp(),
                    )
                    .unwrap(),
                "a stale cursor batch must lose CAS after the production tick"
            );
            let suffix = store
                .get_program_run_v1(*acknowledged_runs.last().unwrap())
                .unwrap()
                .unwrap();
            assert_eq!(
                suffix.status,
                ProgramRunStatusV1::RetryPending,
                "the valid suffix must remain pending before manager/boot/A6/grant reconstruction"
            );
            let cursor: String = store
                .conn
                .query_row(
                    "SELECT value FROM daemon_settings WHERE key='d05.program-run-dispatch.cursor.v1'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            let cursor_json: serde_json::Value = serde_json::from_str(&cursor).unwrap();
            let acknowledged_position = cursor_json["acknowledged"]["action_id"]
                .as_str()
                .map(Uuid::parse_str)
                .transpose()
                .unwrap()
                .expect("the first tick must persist an acknowledged position");
            assert!(
                acknowledged_position != *acknowledged_wakes.last().unwrap(),
                "the persisted cursor must not cross the valid suffix before restart: {cursor}"
            );
        }
        drop(manager);

        let manager = reopen_dispatch_manager(&db_path, dir.path());
        activate_dispatch_controllers(&manager, &controllers, "v7-recreated-boot").await;
        manager.reconcile_program_runs_at_startup().await.unwrap();
        {
            let store = manager.store.lock().await;
            for run_id in &published_runs {
                store
                    .conn
                    .execute(
                        "UPDATE idea_program_runs SET row_version=row_version+1,updated_at=?1
                         WHERE id=?2",
                        rusqlite::params![
                            timestamp().to_rfc3339_opts(SecondsFormat::Nanos, true),
                            run_id.to_string()
                        ],
                    )
                    .unwrap();
            }
        }
        let mut opportunity_tick = None;
        for tick in 2..=20 {
            tick_stats.push(
                ProgramRunDispatcher::new(
                    Arc::clone(&manager),
                    publisher.clone(),
                    manager.program_run_boot_id,
                )
                .unwrap()
                .tick_with_stats()
                .await,
            );
            let suffix = manager
                .store
                .lock()
                .await
                .get_program_run_v1(*acknowledged_runs.last().unwrap())
                .unwrap()
                .unwrap();
            if suffix.status == ProgramRunStatusV1::Ready {
                opportunity_tick = Some(tick);
                break;
            }
        }

        for stats in tick_stats {
            assert!(stats.loaded_visits <= 128);
            assert!(stats.processed_visits <= 128);
            assert!(stats.selection_queries <= 9);
            assert!(stats.claim_transactions <= 128);
            assert_eq!(stats.cursor_transactions, 1);
            assert_eq!(stats.external_effect_attempts, 0);
        }
        assert!(
            opportunity_tick.is_some(),
            "the valid suffix must advance within the declared stable-set bound"
        );
        assert!(
            opportunity_tick.is_some_and(|tick| (2..=4).contains(&tick)),
            "post-restart cursor continuation and wrap must reach the suffix within three ticks: {opportunity_tick:?}"
        );
        assert!(publisher.calls.lock().unwrap().is_empty());
        let store = manager.store.lock().await;
        let suffix = store
            .get_program_run_v1(*acknowledged_runs.last().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            suffix.status,
            ProgramRunStatusV1::Ready,
            "the valid suffix must advance after the 65 unchanged current prefix facts; \
             opportunity={opportunity_tick:?}"
        );
        let unchanged_published: i64 = store
            .conn
            .query_row(
                &format!(
                    "SELECT count(*) FROM idea_program_run_actions
                     WHERE state='published' AND id IN ({})",
                    std::iter::repeat_n("?", published_actions.len())
                        .collect::<Vec<_>>()
                        .join(",")
                ),
                rusqlite::params_from_iter(published_actions.iter().map(ToString::to_string)),
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(unchanged_published, 33);
        let unchanged_acknowledged: i64 = store
            .conn
            .query_row(
                &format!(
                    "SELECT count(*) FROM idea_program_run_actions
                     WHERE state='acknowledged' AND id IN ({})",
                    std::iter::repeat_n("?", acknowledged_wakes.len() - 1)
                        .collect::<Vec<_>>()
                        .join(",")
                ),
                rusqlite::params_from_iter(
                    acknowledged_wakes[..acknowledged_wakes.len() - 1]
                        .iter()
                        .map(ToString::to_string),
                ),
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(unchanged_acknowledged, 32);
    }

    #[allow(clippy::significant_drop_tightening)]
    #[tokio::test]
    async fn d05_v7_sparse_portfolio_has_exact_tick_work_and_index_bounds() {
        const SPARSE_LIVE_AUTHORITIES: usize = 10_001;
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("rsi.db");
        let (mut controllers, _, _) = seed_ready_work_runs(&db_path, 1);
        let (session, project, idea) = controllers.pop().unwrap();
        let manager = reopen_dispatch_manager(&db_path, dir.path());
        activate_dispatch_controllers(
            &manager,
            &[(session.clone(), project.clone(), idea.clone())],
            "v7-sparse-real",
        )
        .await;
        manager
            .store
            .lock()
            .await
            .conn
            .execute(
                "UPDATE idea_program_run_actions SET not_before=?1",
                [(timestamp() - chrono::Duration::seconds(1))
                    .to_rfc3339_opts(SecondsFormat::Nanos, true)],
            )
            .unwrap();
        {
            let mut active = manager.active.write().await;
            let mut tokens = manager.agent_tokens.write().await;
            let store = manager.store.lock().await;
            for index in 1..SPARSE_LIVE_AUTHORITIES {
                let mut sparse = crate::store::tests::make_test_session();
                sparse.project_id = Some(project.id);
                sparse.status = SessionStatus::Running;
                active.insert(sparse.id, TrackedSession::new_for_test(sparse.clone()));
                tokens.insert(format!("d05-v7-sparse-token-{index}"), sparse.id);
                store.install_controller_grant_v1(
                    BoundControllerWriteAuthority::new(project.id, idea.id, sparse.id, 1).unwrap(),
                );
            }
        }
        assert_eq!(manager.active.read().await.len(), SPARSE_LIVE_AUTHORITIES);
        let publisher = Arc::new(NeverPublishCurrentPublisher::default());
        let stats = ProgramRunDispatcher::new(
            Arc::clone(&manager),
            publisher.clone(),
            manager.program_run_boot_id,
        )
        .unwrap()
        .tick_with_stats()
        .await;
        assert_eq!(stats.selection_queries, 5);
        assert_eq!(stats.loaded_visits, 1);
        assert_eq!(stats.loaded_visit_capacity, 128);
        assert_eq!(stats.processed_visits, 1);
        assert_eq!(stats.authority_bind_attempts, 1);
        assert_eq!(stats.claim_transactions, 1);
        assert_eq!(stats.cursor_transactions, 1);
        assert_eq!(stats.external_effect_attempts, 1);
        assert_eq!(stats.rust_sort_operations, 0);
        assert_eq!(publisher.calls.lock().unwrap().len(), 1);

        let store = manager.store.lock().await;
        assert_exact_dispatch_sql_plans(&store.conn);
    }

    #[tokio::test]
    async fn d05_real_dispatcher_reopen_converges_all_wake_outbox_crash_boundaries() {
        for boundary in [
            CrashBoundary::DownstreamCommitted,
            CrashBoundary::Published,
            CrashBoundary::Bound,
            CrashBoundary::Acknowledged,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let db_path = dir.path().join("rsi.db");
            let (session, project, idea, run_id, job_id) =
                seed_retry_pending_wake(&db_path, boundary);

            let store = Store::open(&db_path).expect("reopen after crash boundary");
            let manager = Arc::new(
                SessionManager::new(
                    Arc::new(EventBus::new(16)),
                    store,
                    false,
                    dir.path().join("daemon.sock"),
                    None,
                    Vec::new(),
                    RuntimeConfig::from_config(&Config::from_env()),
                    dir.path().join("sandboxes"),
                )
                .expect("restart manager"),
            );
            manager
                .active
                .write()
                .await
                .insert(session.id, TrackedSession::new_for_test(session.clone()));
            manager
                .agent_tokens
                .write()
                .await
                .insert(format!("d05-restart-token:{}", session.id), session.id);
            manager.store.lock().await.install_controller_grant_v1(
                BoundControllerWriteAuthority::new(project.id, idea.id, session.id, 1)
                    .expect("restart grant"),
            );
            manager
                .reconcile_program_runs_at_startup()
                .await
                .expect("restart reconciliation");
            if matches!(boundary, CrashBoundary::Published) {
                let store = manager.store.lock().await;
                let expired = (timestamp() - chrono::Duration::seconds(1))
                    .to_rfc3339_opts(SecondsFormat::Nanos, true);
                store
                    .conn
                    .execute(
                        "UPDATE idea_program_run_actions SET claim_expires_at=?1
                         WHERE program_run_id=?2 AND state='published'
                           AND scheduled_job_id IS NULL",
                        rusqlite::params![expired, run_id.to_string()],
                    )
                    .unwrap();
            }
            let dispatcher = ProgramRunDispatcher::new(
                Arc::clone(&manager),
                Arc::new(ScheduledJobWakeAdapter::new(Arc::clone(&manager.store))),
                manager.program_run_boot_id,
            )
            .expect("restart dispatcher");
            dispatcher.tick().await;

            let store = manager.store.lock().await;
            let status = store
                .get_program_run_operational_status_v1(run_id, true, timestamp())
                .expect("converged status");
            let acknowledged_wakes: i64 = store
                .conn
                .query_row(
                    "SELECT count(*) FROM idea_program_run_actions
                     WHERE program_run_id=?1 AND action_kind='wake' AND state='acknowledged'
                     AND acknowledged_at IS NOT NULL",
                    [run_id.to_string()],
                    |row| row.get(0),
                )
                .unwrap();
            let jobs: i64 = store
                .conn
                .query_row(
                    "SELECT count(*) FROM scheduled_jobs WHERE id=?1",
                    [job_id.to_string()],
                    |row| row.get(0),
                )
                .unwrap();
            drop(store);
            assert_eq!(status.run.status, ProgramRunStatusV1::Ready);
            assert_eq!(acknowledged_wakes, 1);
            assert_eq!(jobs, 1, "restart must never duplicate the wake job");
        }
    }

    #[tokio::test]
    async fn d05_v4_uncertain_publication_is_finite_expiry_gated_and_fair() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("rsi.db");
        let (controllers, run_ids, deferred_action_id) = seed_ready_work_runs(&db_path, 33);
        let store = Store::open(&db_path).unwrap();
        let manager = Arc::new(
            SessionManager::new(
                Arc::new(EventBus::new(16)),
                store,
                false,
                dir.path().join("daemon.sock"),
                None,
                Vec::new(),
                RuntimeConfig::from_config(&Config::from_env()),
                dir.path().join("sandboxes"),
            )
            .unwrap(),
        );
        for (session, project, idea) in &controllers {
            manager
                .active
                .write()
                .await
                .insert(session.id, TrackedSession::new_for_test(session.clone()));
            manager
                .agent_tokens
                .write()
                .await
                .insert(format!("d05-v4-token:{}", session.id), session.id);
            manager.store.lock().await.install_controller_grant_v1(
                BoundControllerWriteAuthority::new(project.id, idea.id, session.id, 1).unwrap(),
            );
        }
        let publisher = Arc::new(UncertainPublisher::default());
        let dispatcher = ProgramRunDispatcher::new(
            Arc::clone(&manager),
            publisher.clone(),
            manager.program_run_boot_id,
        )
        .unwrap();

        dispatcher.tick().await;
        dispatcher.tick().await;
        assert_eq!(publisher.calls.lock().unwrap().len(), 32);
        {
            let store = manager.store.lock().await;
            store
                .conn
                .execute(
                    "UPDATE idea_program_run_actions SET not_before=?1 WHERE id=?2",
                    rusqlite::params![
                        timestamp().to_rfc3339_opts(SecondsFormat::Nanos, true),
                        deferred_action_id.to_string()
                    ],
                )
                .unwrap();
        }
        dispatcher.tick().await;
        dispatcher.tick().await;
        assert_eq!(
            publisher.calls.lock().unwrap().len(),
            33,
            "32 current unexpired claims cannot starve later due work"
        );
        dispatcher.tick().await;
        assert_eq!(
            publisher.calls.lock().unwrap().len(),
            33,
            "pre-expiry ticks must not republish uncertainty"
        );

        expire_uncertain_claims(&manager, deferred_action_id, false).await;
        dispatcher.tick().await;
        dispatcher.tick().await;
        assert_eq!(publisher.calls.lock().unwrap().len(), 65);
        {
            let store = manager.store.lock().await;
            let retried: i64 = store
                .conn
                .query_row(
                    "SELECT count(*) FROM idea_program_run_actions
                     WHERE publication_attempts=2 AND claim_generation=3",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            let charged: i64 = store
                .conn
                .query_row(
                    "SELECT count(*) FROM idea_program_run_budgets
                     WHERE dimension='action_publication_retries' AND used_value=1",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            drop(store);
            assert_eq!(retried, 32);
            assert_eq!(charged, 32);
        }
        dispatcher.tick().await;
        assert_eq!(
            publisher.calls.lock().unwrap().len(),
            65,
            "the retry claim also supplies the backoff/expiry window"
        );

        expire_uncertain_claims(&manager, deferred_action_id, true).await;
        dispatcher.tick().await;
        dispatcher.tick().await;
        assert_eq!(
            publisher.calls.lock().unwrap().len(),
            66,
            "32 exhausted rows settle locally and expose the later due retry"
        );
        dispatcher.tick().await;
        assert_eq!(publisher.calls.lock().unwrap().len(), 66);
        expire_uncertain_claims(&manager, deferred_action_id, true).await;
        dispatcher.tick().await;
        dispatcher.tick().await;
        assert_eq!(publisher.calls.lock().unwrap().len(), 66);

        let store = manager.store.lock().await;
        let exhausted: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM idea_program_run_actions
                 WHERE state='failed' AND publication_attempts=2
                   AND last_error_class='publication_budget_exhausted'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(exhausted, 33);
        let charged: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM idea_program_run_budgets
                 WHERE dimension='action_publication_retries' AND used_value=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(charged, 33);
        for run_id in &run_ids {
            let status = store
                .get_program_run_operational_status_v1(*run_id, true, timestamp())
                .unwrap();
            assert_eq!(
                status.next_action,
                rsi_common::program_runs::ProgramRunNextActionV1::OperatorCancelOnly
            );
            assert_eq!(
                status.current_attempt.as_ref().unwrap().state,
                rsi_common::program_runs::ProgramRunAttemptStateV1::Failed
            );
        }
        let first = store.get_program_run_v1(run_ids[0]).unwrap().unwrap();
        let cancelled = store
            .apply_program_run_transition_v1(
                &ProgramRunStoreAuthority::operator(),
                &ProgramRunTransitionInputV1::Simple(ProgramRunTransitionRequestV1 {
                    program_run_id: first.id,
                    expected_run_version: first.row_version,
                    expected_idea_version: first.idea_row_version,
                    operation: ProgramRunOperationV1::OperatorCancelled,
                    idempotency_key: "d05-v4-exhausted-cancel".into(),
                    reason: Some("publication retry budget exhausted".into()),
                }),
            )
            .unwrap();
        drop(store);
        assert_eq!(cancelled.run.status, ProgramRunStatusV1::Cancelled);
    }

    #[tokio::test]
    async fn d05_v4_published_reference_reconciles_without_republication() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("rsi.db");
        let (mut controllers, run_ids, action_id) = seed_ready_work_runs(&db_path, 1);
        let (session, project, idea) = controllers.pop().unwrap();
        let store = Store::open(&db_path).unwrap();
        let now = timestamp();
        store
            .conn
            .execute(
                "UPDATE idea_program_run_actions SET not_before=?1 WHERE id=?2",
                rusqlite::params![
                    now.to_rfc3339_opts(SecondsFormat::Nanos, true),
                    action_id.to_string()
                ],
            )
            .unwrap();
        let scheduler = test_program_run_scheduler_authority(session.id, 1);
        let boot = store.program_run_boot_id();
        let claim = store
            .claim_due_program_run_actions_v1(
                &scheduler,
                boot,
                &[ProgramRunActionKindV1::Work],
                now,
                1,
            )
            .unwrap()
            .pop()
            .unwrap();
        store
            .apply_program_run_transition_v1(
                &scheduler,
                &ProgramRunTransitionInputV1::ClaimAction(claim.semantic_claim.clone().unwrap()),
            )
            .unwrap();
        store
            .record_program_run_publication_v1(
                &scheduler,
                action_id,
                boot,
                claim.action.claim_generation,
                now,
            )
            .unwrap();
        store
            .bind_program_run_external_reference_v1(
                &scheduler,
                action_id,
                boot,
                claim.action.claim_generation,
                ProgramRunExternalReferenceV1::Session(session.id),
                now,
            )
            .unwrap();
        let manager = Arc::new(
            SessionManager::new(
                Arc::new(EventBus::new(16)),
                store,
                false,
                dir.path().join("daemon.sock"),
                None,
                Vec::new(),
                RuntimeConfig::from_config(&Config::from_env()),
                dir.path().join("sandboxes"),
            )
            .unwrap(),
        );
        manager
            .active
            .write()
            .await
            .insert(session.id, TrackedSession::new_for_test(session.clone()));
        manager
            .agent_tokens
            .write()
            .await
            .insert(format!("d05-v4-reference-token:{}", session.id), session.id);
        manager.store.lock().await.install_controller_grant_v1(
            BoundControllerWriteAuthority::new(project.id, idea.id, session.id, 1).unwrap(),
        );
        manager.reconcile_program_runs_at_startup().await.unwrap();
        let publisher = Arc::new(UncertainPublisher::default());
        let dispatcher = ProgramRunDispatcher::new(
            Arc::clone(&manager),
            publisher.clone(),
            manager.program_run_boot_id,
        )
        .unwrap();
        dispatcher.tick().await;
        assert!(publisher.calls.lock().unwrap().is_empty());
        let store = manager.store.lock().await;
        let state: String = store
            .conn
            .query_row(
                "SELECT state FROM idea_program_run_actions WHERE id=?1",
                [action_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "acknowledged");
        let status = store
            .get_program_run_operational_status_v1(run_ids[0], true, timestamp())
            .unwrap();
        drop(store);
        assert_eq!(
            status.next_action,
            rsi_common::program_runs::ProgramRunNextActionV1::CommitOutput
        );
    }

    #[tokio::test]
    async fn d05_rr2_consumed_wake_history_cannot_acknowledge_or_starve_new_wakes() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("rsi.db");
        let (session, project, idea, run_id, _) =
            seed_retry_pending_wake(&db_path, CrashBoundary::Acknowledged);
        let store = Store::open(&db_path).expect("reopen consumed wake fixture");
        let manager = Arc::new(
            SessionManager::new(
                Arc::new(EventBus::new(16)),
                store,
                false,
                dir.path().join("daemon.sock"),
                None,
                Vec::new(),
                RuntimeConfig::from_config(&Config::from_env()),
                dir.path().join("sandboxes"),
            )
            .unwrap(),
        );
        manager
            .active
            .write()
            .await
            .insert(session.id, TrackedSession::new_for_test(session.clone()));
        manager
            .agent_tokens
            .write()
            .await
            .insert(format!("d05-rr2-token:{}", session.id), session.id);
        manager.store.lock().await.install_controller_grant_v1(
            BoundControllerWriteAuthority::new(project.id, idea.id, session.id, 1).unwrap(),
        );
        manager.reconcile_program_runs_at_startup().await.unwrap();
        let dispatcher = ProgramRunDispatcher::new(
            Arc::clone(&manager),
            Arc::new(ScheduledJobWakeAdapter::new(Arc::clone(&manager.store))),
            manager.program_run_boot_id,
        )
        .unwrap();

        // Wake A was acknowledged before the boot change. The first real tick
        // must consume only A, then every later tick must consume its own wake.
        dispatcher.tick().await;
        for iteration in 0..32 {
            {
                let store = manager.store.lock().await;
                let controller = test_program_run_controller_authority(session.id, 1);
                let scheduler = test_program_run_scheduler_authority(session.id, 1);
                let boot = store.program_run_boot_id();
                let claim = store
                    .claim_due_program_run_actions_v1(
                        &scheduler,
                        boot,
                        &[ProgramRunActionKindV1::Work],
                        timestamp(),
                        1,
                    )
                    .unwrap()
                    .pop()
                    .unwrap();
                let running = store
                    .apply_program_run_transition_v1(
                        &controller,
                        &ProgramRunTransitionInputV1::ClaimAction(
                            claim.semantic_claim.clone().unwrap(),
                        ),
                    )
                    .unwrap();
                store
                    .record_program_run_publication_v1(
                        &scheduler,
                        claim.action.id,
                        boot,
                        claim.action.claim_generation,
                        timestamp(),
                    )
                    .unwrap();
                let mut attempt_session = crate::store::tests::make_test_session();
                attempt_session.project_id = Some(project.id);
                attempt_session.status = rsi_common::SessionStatus::Running;
                store.insert_session(&attempt_session).unwrap();
                store
                    .bind_program_run_external_reference_v1(
                        &scheduler,
                        claim.action.id,
                        boot,
                        claim.action.claim_generation,
                        ProgramRunExternalReferenceV1::Session(attempt_session.id),
                        timestamp(),
                    )
                    .unwrap();
                let attempt = store
                    .get_program_run_operational_status_v1(run_id, true, timestamp())
                    .unwrap()
                    .current_attempt
                    .unwrap();
                let awaiting = store
                    .apply_program_run_transition_v1(
                        &controller,
                        &ProgramRunTransitionInputV1::CommitOutput(
                            CommitProgramRunOutputRequestV1 {
                                program_run_id: run_id,
                                attempt_id: attempt.id,
                                expected_run_version: running.run.row_version,
                                expected_idea_version: running.run.idea_row_version,
                                idempotency_key: format!("d05-rr2-output-{iteration}"),
                                output_ref: format!("cas://d05-rr2-output-{iteration}"),
                                output_digest: format!("sha256:{:064x}", iteration + 1),
                            },
                        ),
                    )
                    .unwrap();
                store
                    .apply_program_run_transition_v1(
                        &controller,
                        &ProgramRunTransitionInputV1::Gate(RecordProgramRunGateRequestV1 {
                            program_run_id: run_id,
                            expected_run_version: awaiting.run.row_version,
                            expected_idea_version: awaiting.run.idea_row_version,
                            idempotency_key: format!("d05-rr2-gate-{iteration}"),
                            gate_key: "review".into(),
                            result: ProgramRunGateResultV1::Failed,
                            policy_key: "review-v1".into(),
                            policy_version: 1,
                            evidence_ref: format!("cas://d05-rr2-gate-{iteration}"),
                            evidence_digest: format!("sha256:{:064x}", iteration + 65),
                        }),
                    )
                    .unwrap();
                drop(store);
            }
            let tick_stats = dispatcher.tick_with_stats().await;
            let status = manager
                .store
                .lock()
                .await
                .get_program_run_operational_status_v1(run_id, true, timestamp())
                .unwrap();
            assert_eq!(
                status.run.status,
                ProgramRunStatusV1::Ready,
                "iteration {iteration}, stats {tick_stats:?}"
            );
        }
        let store = manager.store.lock().await;
        let consumed: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM idea_program_run_actions
             WHERE program_run_id=?1 AND action_kind='wake' AND state='acknowledged'
               AND claim_boot_id IS NULL",
                [run_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(consumed, 33);
        let current = store
            .claim_due_program_run_actions_v1(
                &test_program_run_scheduler_authority(session.id, 1),
                store.program_run_boot_id(),
                &[ProgramRunActionKindV1::Work],
                timestamp(),
                1,
            )
            .unwrap();
        drop(store);
        assert_eq!(
            current.len(),
            1,
            "consumed history must not consume the bounded limit"
        );
    }
}
