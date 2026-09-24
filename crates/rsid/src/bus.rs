use rsi_common::provider_capabilities::ResolvedContextBudget;
use rsi_common::types::{
    ContextUsageConfidence, ConversationEvent, GraphExecutionUpdate, PendingQuestion, Session,
    SessionKind, SessionStatus,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::broadcast;
use uuid::Uuid;

/// Event type constant for graph execution events.
pub const GRAPH_EXECUTION_EVENT: &str = "graph_execution";

/// Event type constant for cursor-bearing durable agent message-state events.
pub const AGENT_MESSAGE_STATE_EVENT: &str = "agent_message_state";

/// Daemon-internal typed event enum.
/// Converted to BusEvent for external transmission.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum DaemonEvent {
    SessionCreated {
        session: Session,
    },
    SessionQuestionRaised {
        session_id: Uuid,
        question: PendingQuestion,
    },
    /// An exact durable harness-manager notice was queued for its bound watch.
    ManagerNoticeQueued {
        job_id: Uuid,
    },
    SessionStatusChanged {
        session_id: Uuid,
        old_status: SessionStatus,
        new_status: SessionStatus,
    },
    ConversationEvent {
        session_id: Uuid,
        event: ConversationEvent,
    },
    GraphExecution {
        update: GraphExecutionUpdate,
    },
    /// Hint that a project-bound codegraph status changed; the database is authoritative.
    CodegraphIndexStatus {
        project_id: Uuid,
        workspace_id: Uuid,
        phase: rsi_codegraph::lifecycle::IndexLifecyclePhase,
        generation: Option<i64>,
        run_id: Option<Uuid>,
    },
    /// System-level event (errors, info) not tied to a session
    SystemMessage {
        level: String,
        message: String,
    },
    /// A session was deleted from the store
    SessionDeleted {
        session_id: Uuid,
    },
    /// A session was archived (soft-deleted)
    SessionArchived {
        session_id: Uuid,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        projection_id: Option<Uuid>,
    },
    /// A session was unarchived (restored to Completed)
    SessionUnarchived {
        session_id: Uuid,
    },
    /// Session metadata changed (pin, model, project assignment).
    SessionMetadataChanged {
        session_id: Uuid,
        model: Option<String>,
        pinned_at: Option<Option<String>>,
        project_id: Option<Option<Uuid>>,
        parent_id: Option<Option<Uuid>>,
        lead_session_id: Option<Option<Uuid>>,
        testing_needed_at: Option<Option<String>>,
        rotation_disabled_at: Option<Option<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resolved_context_budget: Option<ResolvedContextBudget>,
    },
    /// Memory index was updated after a sync operation.
    MemoryIndexUpdated {
        file_count: usize,
        chunk_count: usize,
    },
    /// A failed session is about to retry after a backoff delay.
    SessionRetrying {
        session_id: Uuid,
        attempt: u8,
        max_retries: u8,
        backoff_ms: u64,
        reason: String,
    },
    /// Real-time context usage update from the streaming hot path.
    ContextUsageUpdated {
        session_id: Uuid,
        /// Context fill percentage (0.0–100.0).
        pct: f64,
        /// Total input tokens accumulated so far (API-reported).
        input_tokens: u64,
        /// Total output tokens accumulated so far (API-reported).
        output_tokens: u64,
        /// Daemon-counted token total (input + output) from raw content.
        daemon_total: u64,
        /// Confidence level of this measurement.
        confidence: ContextUsageConfidence,
        /// Context window size used for percentage calculation.
        context_window: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resolved_context_budget: Option<ResolvedContextBudget>,
    },
    /// Account-level plan-window utilization changed (V99, P1-B).
    ///
    /// This is the LIVE path for rate-limit visibility, and it is the primary
    /// one: provider availability is refreshed at connect time only, so without
    /// a push event the TUI would never see utilization move during a session.
    /// The snapshot is account-scoped, not session-scoped — every concurrent
    /// session reports the same windows.
    ProviderRateLimitUpdated {
        snapshot: rsi_common::rpc::ProviderRateLimitSnapshot,
    },
    /// A session has been idle beyond the stall threshold.
    SessionStalled {
        session_id: Uuid,
        status: SessionStatus,
        /// Seconds since last event.
        idle_secs: u64,
    },
    /// The stall classifier emitted a verdict for an idle session. Always
    /// telemetry; nudge dispatch is a separate code path on the daemon
    /// side. The TUI renders this as an ambient notification.
    SessionClassified {
        session_id: Uuid,
        verdict: crate::stall_classifier::types::Verdict,
        confidence: f64,
        /// Idle duration at classification time (seconds).
        idle_secs: u64,
        /// `"notify_only"` or `"continue"` — kept as a string for forward
        /// compatibility with future `NudgeAction` variants.
        action_taken: String,
    },
    /// A background queue task was completed.
    QueueTaskCompleted {
        work_unit_key: String,
        task_type: String,
        item_count: usize,
    },
    /// A background queue task failed after retries.
    QueueTaskFailed {
        work_unit_key: String,
        task_type: String,
        error: String,
        attempts: i32,
    },
    /// A new session summary was generated.
    SessionSummaryUpdated {
        session_id: Uuid,
        kind: rsi_common::types::SummaryKind,
        /// The summary content (short summary used for session list display).
        content: String,
    },
    /// Observations were extracted from a completed session.
    ObservationsExtracted {
        session_id: Uuid,
        count: usize,
    },
    /// A session was transitioned by the reconciliation loop.
    SessionReconciled {
        session_id: Uuid,
        old_status: SessionStatus,
        new_status: SessionStatus,
        reason: crate::reconciliation::ReconciliationReason,
    },
    /// A dream consolidation cycle has started.
    DreamStarted,
    /// A dream consolidation cycle has completed.
    DreamCompleted {
        observations_extracted: usize,
        deductions_created: usize,
        patterns_identified: usize,
    },
    /// An issue was dispatched to a new session.
    IssueDispatched {
        issue_id: String,
        issue_identifier: String,
        session_id: Uuid,
        tracker: String,
    },
    /// Issue tracker completed a poll cycle.
    IssueTrackerPolled {
        issues_found: usize,
        dispatched: usize,
        skipped_claimed: usize,
        skipped_blocked: usize,
    },
    /// A running issue's tracker state changed.
    IssueReconciled {
        issue_id: String,
        issue_identifier: String,
        old_state: String,
        new_state: String,
        /// "stopped" | "continued" | "completed"
        action: String,
    },
    /// A scheduled job fired and launched a session.
    ScheduledJobFired {
        job_id: uuid::Uuid,
        job_name: String,
        session_id: uuid::Uuid,
    },
    /// Streaming token delta from an in-flight CompilePrompt request.
    CompilePromptChunk {
        request_id: Uuid,
        delta: String,
    },
    /// CompilePrompt succeeded — full result.
    CompilePromptCompleted {
        request_id: Uuid,
        result: rsi_common::prompt_compile::CompileResult,
    },
    /// CompilePrompt failed or was superseded.
    CompilePromptFailed {
        request_id: Uuid,
        error: String,
    },
    /// A sandbox orphan was detected and cleaned up. `session_id` is `None`
    /// when the on-disk sandbox dir has no corresponding DB row (pure disk
    /// orphan); `Some(uuid)` when a DB row still carried `Live` state but its
    /// owning session had reached a terminal status when the sweep ran.
    SandboxOrphanCleaned {
        session_id: Option<Uuid>,
        sandbox_root: std::path::PathBuf,
    },
    /// An Epic-lead session emitted a `<docregblock>/spawn_child …</docregblock>`
    /// directive that successfully passed validation and the daemon launched a
    /// new child session under that Epic.
    ChildSpawned {
        parent_epic_id: Uuid,
        child_id: Uuid,
        kind: SessionKind,
    },
    /// A lead session emitted a `/halt` directive (or other workflow directive)
    /// inside a `<docregblock>` block. The loop executor subscribes to this
    /// event to implement `UntilCondition::LeadHalt`.
    HaltDirective {
        session_id: Uuid,
        /// The directive name, e.g. `"/halt"`.
        directive: String,
    },
    /// A duplicate provider spawn for `session_id` was suppressed: a racing
    /// resume/continue/handoff-resume blocked on the per-session single-flight
    /// lock and adopted the already-live child instead of spawning a twin.
    /// One cursor-bearing durable agent message-state fact (P2-03).
    ///
    /// Published ONLY after the owning Store transaction commits, so a
    /// subscriber can never observe a state the database would not confirm.
    /// The envelope deliberately carries no message payload — progress and
    /// state events never expose message bodies.
    AgentMessageState {
        event: rsi_common::agent_coordination::AgentMessageStateEventV1,
    },
    SessionSpawnDeduped {
        session_id: Uuid,
        /// Which spawn entry point suppressed the duplicate, e.g.
        /// "continue_session" or "resume_for_handoff_write".
        source: String,
    },
    ModelInvocationAdmitted {
        invocation_id: Uuid,
        purpose: rsi_common::model_control::ModelInvocationPurpose,
        session_id: Option<Uuid>,
    },
    ModelInvocationDenied {
        invocation_id: Uuid,
        purpose: rsi_common::model_control::ModelInvocationPurpose,
        reason: String,
        session_id: Option<Uuid>,
    },
    ModelBudgetNearLimit {
        invocation_id: Uuid,
        metric: String,
        remaining: i64,
        limit: i64,
        scope_kind: rsi_common::model_control::BudgetScopeKind,
        scope_id: Option<String>,
    },
    ModelControlModeChanged {
        previous_mode: rsi_common::model_control::ModelControlMode,
        current_mode: rsi_common::model_control::ModelControlMode,
        updated_at: String,
    },
    ModelControlCircuitChanged {
        mode: rsi_common::model_control::ModelControlMode,
        scope_kind: rsi_common::model_control::BudgetScopeKind,
        scope_id: Option<String>,
        state: String,
        reason: String,
        error_class: Option<String>,
        source: String,
        updated_at: String,
    },
    ModelInvocationCancellationRequested {
        invocation_id: Uuid,
        session_id: Option<Uuid>,
        reason: String,
        mechanism: String,
    },
    ModelInvocationCancellationSkipped {
        invocation_id: Uuid,
        session_id: Option<Uuid>,
        reason: String,
    },
    ModelInvocationCancelled {
        invocation_id: Uuid,
        session_id: Option<Uuid>,
        reason: String,
    },
    ModelInvocationCompleted {
        invocation_id: Uuid,
        error_class: Option<String>,
    },
}

impl From<DaemonEvent> for rsi_common::rpc::BusEvent {
    fn from(event: DaemonEvent) -> Self {
        Self {
            event_type: match &event {
                DaemonEvent::SessionCreated { .. } => "session_created".to_string(),
                DaemonEvent::SessionQuestionRaised { .. } => "session_question_raised".to_string(),
                DaemonEvent::ManagerNoticeQueued { .. } => "manager_notice_queued".to_string(),
                DaemonEvent::SessionStatusChanged { .. } => "session_status_changed".to_string(),
                DaemonEvent::ConversationEvent { .. } => "conversation_event".to_string(),
                DaemonEvent::GraphExecution { .. } => GRAPH_EXECUTION_EVENT.to_string(),
                DaemonEvent::CodegraphIndexStatus { .. } => "codegraph.index.status".to_string(),
                DaemonEvent::AgentMessageState { .. } => AGENT_MESSAGE_STATE_EVENT.to_string(),
                DaemonEvent::SystemMessage { .. } => "system_message".to_string(),
                DaemonEvent::SessionDeleted { .. } => "session_deleted".to_string(),
                DaemonEvent::SessionArchived { .. } => "session_archived".to_string(),
                DaemonEvent::SessionUnarchived { .. } => "session_unarchived".to_string(),
                DaemonEvent::SessionMetadataChanged { .. } => {
                    "session_metadata_changed".to_string()
                }
                DaemonEvent::MemoryIndexUpdated { .. } => "memory_index_updated".to_string(),
                DaemonEvent::SessionRetrying { .. } => "session_retrying".to_string(),
                DaemonEvent::ContextUsageUpdated { .. } => "context_usage_updated".to_string(),
                DaemonEvent::ProviderRateLimitUpdated { .. } => {
                    "provider_rate_limit_updated".to_string()
                }
                DaemonEvent::SessionStalled { .. } => "session_stalled".to_string(),
                DaemonEvent::SessionClassified { .. } => "session_classified".to_string(),
                DaemonEvent::QueueTaskCompleted { .. } => "queue_task_completed".to_string(),
                DaemonEvent::QueueTaskFailed { .. } => "queue_task_failed".to_string(),
                DaemonEvent::SessionSummaryUpdated { .. } => "session_summary_updated".to_string(),
                DaemonEvent::ObservationsExtracted { .. } => "observations_extracted".to_string(),
                DaemonEvent::SessionReconciled { .. } => "session_reconciled".to_string(),
                DaemonEvent::DreamStarted => "dream_started".to_string(),
                DaemonEvent::DreamCompleted { .. } => "dream_completed".to_string(),
                DaemonEvent::IssueDispatched { .. } => "issue_dispatched".to_string(),
                DaemonEvent::IssueTrackerPolled { .. } => "issue_tracker_polled".to_string(),
                DaemonEvent::IssueReconciled { .. } => "issue_reconciled".to_string(),
                DaemonEvent::ScheduledJobFired { .. } => "scheduled_job_fired".to_string(),
                DaemonEvent::CompilePromptChunk { .. } => "compile_prompt_chunk".to_string(),
                DaemonEvent::CompilePromptCompleted { .. } => {
                    "compile_prompt_completed".to_string()
                }
                DaemonEvent::CompilePromptFailed { .. } => "compile_prompt_failed".to_string(),
                DaemonEvent::SandboxOrphanCleaned { .. } => "sandbox_orphan_cleaned".to_string(),
                DaemonEvent::ChildSpawned { .. } => "child_spawned".to_string(),
                DaemonEvent::HaltDirective { .. } => "halt_directive".to_string(),
                DaemonEvent::SessionSpawnDeduped { .. } => "session_spawn_deduped".to_string(),
                DaemonEvent::ModelInvocationAdmitted { .. } => {
                    "model_invocation_admitted".to_string()
                }
                DaemonEvent::ModelInvocationDenied { .. } => "model_invocation_denied".to_string(),
                DaemonEvent::ModelBudgetNearLimit { .. } => "model_budget_near_limit".to_string(),
                DaemonEvent::ModelControlModeChanged { .. } => {
                    "model_control_mode_changed".to_string()
                }
                DaemonEvent::ModelControlCircuitChanged { .. } => {
                    "model_control_circuit_changed".to_string()
                }
                DaemonEvent::ModelInvocationCancellationRequested { .. } => {
                    "model_invocation_cancellation_requested".to_string()
                }
                DaemonEvent::ModelInvocationCancellationSkipped { .. } => {
                    "model_invocation_cancellation_skipped".to_string()
                }
                DaemonEvent::ModelInvocationCancelled { .. } => {
                    "model_invocation_cancelled".to_string()
                }
                DaemonEvent::ModelInvocationCompleted { .. } => {
                    "model_invocation_completed".to_string()
                }
            },
            timestamp: chrono::Utc::now(),
            // Unwrap the `#[serde(tag = "type", content = "data")]` envelope so
            // consumers see the variant payload directly at `BusEvent.data.*`
            // (the `event_type` field already carries the discriminant).
            data: match serde_json::to_value(&event).unwrap_or_default() {
                serde_json::Value::Object(mut map) => {
                    map.remove("data").unwrap_or(serde_json::Value::Null)
                }
                other => other,
            },
        }
    }
}

pub struct EventBus {
    sender: broadcast::Sender<Arc<DaemonEvent>>,
    subscriber_count: AtomicUsize,
    archive_projection_in_flight: Arc<Mutex<HashSet<Uuid>>>,
}

/// A lifecycle-scoped guard for one accepted archive projection delivery.
///
/// Durable consumer acknowledgement is the replay fence. This guard only
/// prevents a concurrent retry from applying the same projection twice while
/// that acknowledgement is still being written, so its memory use is bounded
/// by concurrent archive deliveries rather than archive history.
pub(crate) struct ArchiveProjectionDeliveryGuard {
    projection_id: Uuid,
    in_flight: Arc<Mutex<HashSet<Uuid>>>,
}

impl Drop for ArchiveProjectionDeliveryGuard {
    fn drop(&mut self) {
        self.in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.projection_id);
    }
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel::<Arc<DaemonEvent>>(capacity);
        Self {
            sender,
            subscriber_count: AtomicUsize::new(0),
            archive_projection_in_flight: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Claims one projection ID until its durable consumer acknowledgement.
    pub(crate) fn begin_archive_projection_delivery(
        &self,
        projection_id: Uuid,
    ) -> Option<ArchiveProjectionDeliveryGuard> {
        let mut in_flight = self
            .archive_projection_in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !in_flight.insert(projection_id) {
            return None;
        }
        drop(in_flight);
        Some(ArchiveProjectionDeliveryGuard {
            projection_id,
            in_flight: Arc::clone(&self.archive_projection_in_flight),
        })
    }

    /// Publish an event to all subscribers.
    /// If there are no subscribers, the event is dropped silently.
    pub fn publish(&self, event: DaemonEvent) {
        if self.subscriber_count.load(Ordering::Relaxed) > 0 {
            // Ignore send errors (subscribers may have been dropped)
            let _ = self.sender.send(Arc::new(event));
        }
    }

    /// Subscribe to receive events.
    /// Remember to call `unsubscribe()` when done to maintain accurate count.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<DaemonEvent>> {
        self.subscriber_count.fetch_add(1, Ordering::Relaxed);
        self.sender.subscribe()
    }

    /// Decrement subscriber count when a subscriber is dropped.
    pub fn unsubscribe(&self) {
        self.subscriber_count.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn subscriber_count(&self) -> usize {
        self.subscriber_count.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::provider_capabilities::{
        CapabilityConfidence, CapabilityEvidence, CapabilitySource, ContextCapacity,
    };
    use rsi_common::types::SessionStatus;

    #[test]
    fn test_event_bus_no_subscribers() {
        let bus = EventBus::new(10);
        // Should not panic when publishing with no subscribers
        bus.publish(DaemonEvent::SessionStatusChanged {
            session_id: Uuid::new_v4(),
            old_status: SessionStatus::Starting,
            new_status: SessionStatus::Running,
        });
        assert_eq!(bus.subscriber_count(), 0);
    }

    #[test]
    fn test_event_bus_subscribe_receive() {
        let bus = EventBus::new(10);
        let mut rx = bus.subscribe();
        assert_eq!(bus.subscriber_count(), 1);

        let session_id = Uuid::new_v4();
        bus.publish(DaemonEvent::SessionStatusChanged {
            session_id,
            old_status: SessionStatus::Starting,
            new_status: SessionStatus::Running,
        });

        let event = rx.try_recv().unwrap();
        match *event {
            DaemonEvent::SessionStatusChanged { session_id: id, .. } => {
                assert_eq!(id, session_id);
            }
            _ => panic!("Wrong event type"),
        }

        bus.unsubscribe();
        assert_eq!(bus.subscriber_count(), 0);
    }

    #[test]
    fn archive_projection_delivery_guard_is_bounded_to_the_acknowledgement_lifecycle() {
        let bus = EventBus::new(10);
        let mut receiver = bus.subscribe();
        let session_id = Uuid::new_v4();
        let projection_id = Uuid::new_v4();
        let first = bus
            .begin_archive_projection_delivery(projection_id)
            .expect("first delivery claims the projection");
        assert!(
            bus.begin_archive_projection_delivery(projection_id)
                .is_none(),
            "a concurrent replay cannot claim the same logical application"
        );
        bus.publish(DaemonEvent::SessionArchived {
            session_id,
            projection_id: Some(projection_id),
        });
        let event = receiver.try_recv().expect("first stable projection");
        assert!(matches!(
            event.as_ref(),
            DaemonEvent::SessionArchived {
                session_id: actual_session,
                projection_id: Some(actual_projection),
            } if *actual_session == session_id && *actual_projection == projection_id
        ));
        drop(first);
        assert!(
            bus.archive_projection_in_flight
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "completed delivery retains no daemon-lifetime projection history"
        );
        bus.unsubscribe();
    }

    #[test]
    fn test_daemon_event_to_bus_event() {
        let event = DaemonEvent::SessionStatusChanged {
            session_id: Uuid::new_v4(),
            old_status: SessionStatus::Starting,
            new_status: SessionStatus::Running,
        };
        let bus_event: rsi_common::rpc::BusEvent = event.into();
        assert_eq!(bus_event.event_type, "session_status_changed");
    }

    #[test]
    fn test_graph_execution_event_type() {
        let event = DaemonEvent::GraphExecution {
            update: GraphExecutionUpdate {
                execution_id: Uuid::new_v4(),
                workflow_id: Uuid::new_v4(),
                node_id: Some("plan".to_string()),
                sequence: 1,
                status: rsi_common::types::WorkflowExecutionStatus::Running,
                node_state: Some(rsi_common::types::WorkflowNodeExecutionState::Running),
                finished: false,
                error: None,
                output_preview: None,
                updated_at: chrono::Utc::now(),
            },
        };
        let bus_event: rsi_common::rpc::BusEvent = event.into();
        assert_eq!(bus_event.event_type, GRAPH_EXECUTION_EVENT);
    }

    #[test]
    fn test_memory_index_updated_event_type() {
        let event = DaemonEvent::MemoryIndexUpdated {
            file_count: 10,
            chunk_count: 200,
        };
        let bus_event: rsi_common::rpc::BusEvent = event.into();
        assert_eq!(bus_event.event_type, "memory_index_updated");
    }

    #[test]
    fn test_memory_index_updated_serde_roundtrip() {
        let event = DaemonEvent::MemoryIndexUpdated {
            file_count: 5,
            chunk_count: 100,
        };
        let json = serde_json::to_string(&event).unwrap();
        let deser: DaemonEvent = serde_json::from_str(&json).unwrap();
        match deser {
            DaemonEvent::MemoryIndexUpdated {
                file_count,
                chunk_count,
            } => {
                assert_eq!(file_count, 5);
                assert_eq!(chunk_count, 100);
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_memory_index_updated_publish_subscribe() {
        let bus = EventBus::new(10);
        let mut rx = bus.subscribe();

        bus.publish(DaemonEvent::MemoryIndexUpdated {
            file_count: 3,
            chunk_count: 50,
        });

        let event = rx.try_recv().unwrap();
        match *event {
            DaemonEvent::MemoryIndexUpdated {
                file_count,
                chunk_count,
            } => {
                assert_eq!(file_count, 3);
                assert_eq!(chunk_count, 50);
            }
            _ => panic!("Wrong event type"),
        }

        bus.unsubscribe();
    }

    #[test]
    fn test_session_retrying_event_type() {
        let event = DaemonEvent::SessionRetrying {
            session_id: Uuid::new_v4(),
            attempt: 2,
            max_retries: 3,
            backoff_ms: 20000,
            reason: "rate limited".to_string(),
        };
        let bus_event: rsi_common::rpc::BusEvent = event.into();
        assert_eq!(bus_event.event_type, "session_retrying");
    }

    #[test]
    fn test_session_retrying_serde_roundtrip() {
        let event = DaemonEvent::SessionRetrying {
            session_id: Uuid::new_v4(),
            attempt: 1,
            max_retries: 3,
            backoff_ms: 10000,
            reason: "process exited with zero events".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let deser: DaemonEvent = serde_json::from_str(&json).unwrap();
        match deser {
            DaemonEvent::SessionRetrying {
                attempt,
                max_retries,
                backoff_ms,
                ..
            } => {
                assert_eq!(attempt, 1);
                assert_eq!(max_retries, 3);
                assert_eq!(backoff_ms, 10000);
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn context_usage_bus_keeps_scalars_and_adds_resolved_budget() {
        let budget = ResolvedContextBudget {
            active_tokens: 258_400,
            capacity: ContextCapacity::default(),
            evidence: CapabilityEvidence {
                source: CapabilitySource::RuntimeTelemetry,
                source_version: Some("codex-cli 0.155.1".to_string()),
                source_digest: Some(format!("sha256:{}", "b".repeat(64))),
                observed_at: Some(
                    "2026-09-02T12:34:56.123456789Z"
                        .parse()
                        .expect("fixed bus timestamp"),
                ),
                confidence: CapabilityConfidence::Authoritative,
            },
        };
        let event = DaemonEvent::ContextUsageUpdated {
            session_id: Uuid::new_v4(),
            pct: 13.4,
            input_tokens: 34_000,
            output_tokens: 600,
            daemon_total: 35_000,
            confidence: ContextUsageConfidence::Full,
            context_window: 258_400,
            resolved_context_budget: Some(budget.clone()),
        };

        let bus_event: rsi_common::rpc::BusEvent = event.into();
        assert_eq!(bus_event.event_type, "context_usage_updated");
        assert_eq!(bus_event.data["context_window"], 258_400);
        assert_eq!(bus_event.data["input_tokens"], 34_000);
        assert_eq!(
            serde_json::from_value::<ResolvedContextBudget>(
                bus_event.data["resolved_context_budget"].clone()
            )
            .expect("decode nested bus budget"),
            budget
        );

        let mut legacy_data = bus_event.data;
        legacy_data
            .as_object_mut()
            .expect("bus payload object")
            .remove("resolved_context_budget");
        let legacy = serde_json::json!({
            "type": "ContextUsageUpdated",
            "data": legacy_data,
        });
        assert!(matches!(
            serde_json::from_value::<DaemonEvent>(legacy).expect("decode legacy bus event"),
            DaemonEvent::ContextUsageUpdated {
                context_window: 258_400,
                resolved_context_budget: None,
                ..
            }
        ));

        let metadata_event = DaemonEvent::SessionMetadataChanged {
            session_id: Uuid::new_v4(),
            model: Some("gpt-6-astra".to_string()),
            pinned_at: None,
            project_id: None,
            parent_id: None,
            lead_session_id: None,
            testing_needed_at: None,
            rotation_disabled_at: None,
            resolved_context_budget: Some(budget.clone()),
        };
        let metadata_bus: rsi_common::rpc::BusEvent = metadata_event.into();
        assert_eq!(metadata_bus.event_type, "session_metadata_changed");
        assert_eq!(metadata_bus.data["model"], "gpt-6-astra");
        assert_eq!(
            serde_json::from_value::<ResolvedContextBudget>(
                metadata_bus.data["resolved_context_budget"].clone()
            )
            .expect("decode metadata bus budget"),
            budget
        );
    }

    #[test]
    fn test_system_message_event() {
        let event = DaemonEvent::SystemMessage {
            level: "warn".to_string(),
            message: "Test warning".to_string(),
        };
        let bus_event: rsi_common::rpc::BusEvent = event.into();
        assert_eq!(bus_event.event_type, "system_message");
    }

    #[test]
    fn test_session_stalled_event_type() {
        let event = DaemonEvent::SessionStalled {
            session_id: Uuid::new_v4(),
            status: SessionStatus::Running,
            idle_secs: 1800,
        };
        let bus_event: rsi_common::rpc::BusEvent = event.into();
        assert_eq!(bus_event.event_type, "session_stalled");
    }

    #[test]
    fn test_session_stalled_serde_roundtrip() {
        let event = DaemonEvent::SessionStalled {
            session_id: Uuid::new_v4(),
            status: SessionStatus::WaitingApproval,
            idle_secs: 3600,
        };
        let json = serde_json::to_string(&event).unwrap();
        let deser: DaemonEvent = serde_json::from_str(&json).unwrap();
        match deser {
            DaemonEvent::SessionStalled {
                status, idle_secs, ..
            } => {
                assert_eq!(status, SessionStatus::WaitingApproval);
                assert_eq!(idle_secs, 3600);
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_sandbox_orphan_cleaned_event_type() {
        let event = DaemonEvent::SandboxOrphanCleaned {
            session_id: Some(Uuid::new_v4()),
            sandbox_root: std::path::PathBuf::from("/tmp/rsi-sandbox/abc"),
        };
        let bus_event: rsi_common::rpc::BusEvent = event.into();
        assert_eq!(bus_event.event_type, "sandbox_orphan_cleaned");
    }

    #[test]
    fn test_sandbox_orphan_cleaned_serde_roundtrip() {
        let root = std::path::PathBuf::from("/tmp/rsi-sandbox/deadbeef");
        let event = DaemonEvent::SandboxOrphanCleaned {
            session_id: None,
            sandbox_root: root.clone(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let deser: DaemonEvent = serde_json::from_str(&json).unwrap();
        match deser {
            DaemonEvent::SandboxOrphanCleaned {
                session_id,
                sandbox_root,
            } => {
                assert!(session_id.is_none());
                assert_eq!(sandbox_root, root);
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_session_classified_event_type() {
        let event = DaemonEvent::SessionClassified {
            session_id: Uuid::new_v4(),
            verdict: crate::stall_classifier::types::Verdict::StalledContinue,
            confidence: 0.85,
            idle_secs: 720,
            action_taken: "continue".to_string(),
        };
        let bus_event: rsi_common::rpc::BusEvent = event.into();
        assert_eq!(bus_event.event_type, "session_classified");
    }

    #[test]
    fn test_session_classified_serde_roundtrip() {
        let event = DaemonEvent::SessionClassified {
            session_id: Uuid::new_v4(),
            verdict: crate::stall_classifier::types::Verdict::Finished,
            confidence: 0.99,
            idle_secs: 600,
            action_taken: "notify_only".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let deser: DaemonEvent = serde_json::from_str(&json).unwrap();
        match deser {
            DaemonEvent::SessionClassified {
                verdict,
                confidence,
                idle_secs,
                action_taken,
                ..
            } => {
                assert_eq!(verdict, crate::stall_classifier::types::Verdict::Finished);
                assert!((confidence - 0.99).abs() < 1e-9);
                assert_eq!(idle_secs, 600);
                assert_eq!(action_taken, "notify_only");
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_session_classified_bus_data_includes_verdict_payload() {
        let id = Uuid::new_v4();
        let event = DaemonEvent::SessionClassified {
            session_id: id,
            verdict: crate::stall_classifier::types::Verdict::NeedsUser,
            confidence: 0.55,
            idle_secs: 900,
            action_taken: "notify_only".to_string(),
        };
        let bus_event: rsi_common::rpc::BusEvent = event.into();
        let data = bus_event.data;
        assert_eq!(
            data.get("verdict").and_then(|v| v.as_str()),
            Some("NeedsUser")
        );
        assert_eq!(
            data.get("session_id").and_then(|v| v.as_str()),
            Some(id.to_string().as_str())
        );
    }

    #[test]
    fn test_session_reconciled_event_type() {
        let event = DaemonEvent::SessionReconciled {
            session_id: Uuid::new_v4(),
            old_status: SessionStatus::Running,
            new_status: SessionStatus::Failed,
            reason: crate::reconciliation::ReconciliationReason::ProcessDied,
        };
        let bus_event: rsi_common::rpc::BusEvent = event.into();
        assert_eq!(bus_event.event_type, "session_reconciled");
    }

    #[test]
    fn test_session_reconciled_serde_roundtrip() {
        let event = DaemonEvent::SessionReconciled {
            session_id: Uuid::new_v4(),
            old_status: SessionStatus::Running,
            new_status: SessionStatus::Failed,
            reason: crate::reconciliation::ReconciliationReason::ProcessDied,
        };
        let json = serde_json::to_string(&event).unwrap();
        let deser: DaemonEvent = serde_json::from_str(&json).unwrap();
        match deser {
            DaemonEvent::SessionReconciled {
                new_status, reason, ..
            } => {
                assert_eq!(new_status, SessionStatus::Failed);
                assert_eq!(
                    reason,
                    crate::reconciliation::ReconciliationReason::ProcessDied
                );
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_session_question_raised_event() {
        use rsi_common::types::PendingQuestion;

        let session_id = Uuid::new_v4();
        let question = PendingQuestion { questions: vec![] };
        let event = DaemonEvent::SessionQuestionRaised {
            session_id,
            question: question.clone(),
        };

        // Assert BusEvent mapping
        let bus_event: rsi_common::rpc::BusEvent = event.clone().into();
        assert_eq!(bus_event.event_type, "session_question_raised");

        #[derive(serde::Deserialize)]
        struct QInner {
            session_id: Uuid,
            question: PendingQuestion,
        }
        let parsed: QInner = serde_json::from_value(bus_event.data).unwrap();
        assert_eq!(parsed.session_id, session_id);
        assert_eq!(parsed.question.questions.len(), 0);

        // Assert serde round-trip
        let json = serde_json::to_string(&event).unwrap();
        let deser: DaemonEvent = serde_json::from_str(&json).unwrap();
        match deser {
            DaemonEvent::SessionQuestionRaised {
                session_id: sid,
                question: q,
            } => {
                assert_eq!(sid, session_id);
                assert_eq!(q.questions.len(), 0);
            }
            _ => panic!("Wrong variant"),
        }
    }
}
