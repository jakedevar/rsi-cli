//! Durable `ProgramRun` V1 contracts.
//!
//! This module intentionally contains no provider, workflow, or recursive
//! scheduler policy. It defines the closed serial state machine and the
//! canonical values stored by the daemon's `ProgramRun` kernel.

// Validation methods deliberately return stable wire-safe strings rather than
// exporting an additional error vocabulary from the shared contract module.
#![allow(clippy::missing_errors_doc)]

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;
use uuid::Uuid;

pub const PROGRAM_RUN_MAX_CURSORS: usize = 256;
pub const PROGRAM_RUN_MAX_GATES_PER_CURSOR: usize = 32;
pub const PROGRAM_RUN_MAX_KEY_BYTES: usize = 64;
pub const PROGRAM_RUN_MAX_IDEMPOTENCY_KEY_BYTES: usize = 256;
pub const PROGRAM_RUN_MAX_REASON_BYTES: usize = 2_048;
pub const PROGRAM_RUN_MAX_CANONICAL_JSON_BYTES: usize = 1_048_576;

macro_rules! string_enum {
    ($name:ident { $($variant:ident => $wire:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum $name {
            $(#[serde(rename = $wire)] $variant),+
        }

        impl $name {
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $wire),+ }
            }

            pub const ALL: &'static [Self] = &[$(Self::$variant),+];
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = String;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                match value {
                    $($wire => Ok(Self::$variant),)+
                    _ => Err(format!("invalid {} value: {value}", stringify!($name))),
                }
            }
        }
    };
}

string_enum!(ProgramRunStatusV1 {
    Pending => "pending",
    Ready => "ready",
    Running => "running",
    AwaitingGate => "awaiting_gate",
    RetryPending => "retry_pending",
    Blocked => "blocked",
    Settled => "settled",
    Cancelled => "cancelled",
    Failed => "failed",
});

impl ProgramRunStatusV1 {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Settled | Self::Cancelled | Self::Failed)
    }
}

string_enum!(ProgramRunOperationV1 {
    Create => "create",
    LocksGranted => "locks_granted",
    ActionClaimed => "action_claimed",
    AttemptTerminalObserved => "attempt_terminal_observed",
    AttemptOutputCommitted => "attempt_output_committed",
    GateEvaluated => "gate_evaluated",
    RetryScheduled => "retry_scheduled",
    WakeAcknowledged => "wake_acknowledged",
    OperatorUnblocked => "operator_unblocked",
    OperatorCancelled => "operator_cancelled",
    BudgetExhausted => "budget_exhausted",
    ControllerRebound => "controller_rebound",
    ReconciledQuarantine => "reconciled_quarantine",
});

string_enum!(ProgramRunActorKindV1 {
    Operator => "operator",
    Controller => "controller",
    Scheduler => "scheduler",
    System => "system",
});

string_enum!(ProgramRunGateResultV1 {
    Passed => "passed",
    Failed => "failed",
    Blocked => "blocked",
});

string_enum!(ProgramRunBudgetDimensionV1 {
    ProductiveTransitions => "productive_transitions",
    WorkAttempts => "work_attempts",
    LaunchRetries => "launch_retries",
    Revisions => "revisions",
    WakeReservations => "wake_reservations",
    ActionPublicationRetries => "action_publication_retries",
});

string_enum!(ProgramRunLockDomainV1 {
    IdeaController => "idea_controller",
    DangerousMutation => "dangerous_mutation",
});

string_enum!(ProgramRunLockStateV1 {
    Requested => "requested",
    Held => "held",
    Released => "released",
    Expired => "expired",
    Cancelled => "cancelled",
});

string_enum!(ProgramRunActionKindV1 {
    Work => "work",
    Wake => "wake",
});

string_enum!(ProgramRunActionPurposeV1 {
    ExecuteCursor => "execute_cursor",
    EvaluateGates => "evaluate_gates",
    RetryWake => "retry_wake",
});

string_enum!(ProgramRunActionStateV1 {
    Reserved => "reserved",
    Claimed => "claimed",
    Published => "published",
    Acknowledged => "acknowledged",
    Failed => "failed",
    Cancelled => "cancelled",
});

string_enum!(ProgramRunAttemptStateV1 {
    Reserved => "reserved",
    Launched => "launched",
    TerminalObserved => "terminal_observed",
    OutputCommitted => "output_committed",
    Failed => "failed",
    Interrupted => "interrupted",
    Cancelled => "cancelled",
});

string_enum!(ProgramRunReconciliationClassV1 {
    Healthy => "healthy",
    RecoverAction => "recover_action",
    RecoverClaim => "recover_claim",
    AwaitExternal => "await_external",
    Blocked => "blocked",
    Terminal => "terminal",
    Quarantined => "quarantined",
});

string_enum!(ProgramRunNextActionV1 {
    ReconcileLocks => "reconcile_locks",
    ClaimAction => "claim_action",
    AwaitWorkPublisher => "await_work_publisher",
    AwaitPublication => "await_publication",
    ReclaimClaim => "reclaim_claim",
    AwaitAttempt => "await_attempt",
    CommitOutput => "commit_output",
    EvaluateGate => "evaluate_gate",
    AckWake => "ack_wake",
    OperatorUnblock => "operator_unblock",
    OperatorCancelOnly => "operator_cancel_only",
    None => "none",
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgramRunGateRequirementV1 {
    pub gate_key: String,
    pub policy_key: String,
    pub policy_version: u32,
}

impl ProgramRunGateRequirementV1 {
    fn validate(&self) -> Result<(), String> {
        validate_key("gate_key", &self.gate_key)?;
        validate_key("policy_key", &self.policy_key)?;
        if self.policy_version == 0 {
            return Err("gate policy_version must be positive".to_string());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgramRunCursorV1 {
    pub key: String,
    pub phase: String,
    #[serde(default)]
    pub required_gates: Vec<ProgramRunGateRequirementV1>,
    #[serde(default)]
    pub revision_target_ordinal: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgramRunBudgetLimitsV1 {
    pub productive_transitions: u64,
    pub work_attempts: u64,
    pub launch_retries: u64,
    pub revisions: u64,
    pub wake_reservations: u64,
    pub action_publication_retries: u64,
}

impl ProgramRunBudgetLimitsV1 {
    pub fn validate(&self) -> Result<(), String> {
        if self.as_pairs().iter().any(|(_, value)| *value == 0) {
            return Err("all six ProgramRun budget limits must be positive".to_string());
        }
        Ok(())
    }

    #[must_use]
    pub const fn as_pairs(&self) -> [(ProgramRunBudgetDimensionV1, u64); 6] {
        [
            (
                ProgramRunBudgetDimensionV1::ProductiveTransitions,
                self.productive_transitions,
            ),
            (
                ProgramRunBudgetDimensionV1::WorkAttempts,
                self.work_attempts,
            ),
            (
                ProgramRunBudgetDimensionV1::LaunchRetries,
                self.launch_retries,
            ),
            (ProgramRunBudgetDimensionV1::Revisions, self.revisions),
            (
                ProgramRunBudgetDimensionV1::WakeReservations,
                self.wake_reservations,
            ),
            (
                ProgramRunBudgetDimensionV1::ActionPublicationRetries,
                self.action_publication_retries,
            ),
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgramRunLockRequirementV1 {
    pub conflict_domain: ProgramRunLockDomainV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgramRunTemplateV1 {
    pub template_key: String,
    pub template_version: u32,
    pub cursors: Vec<ProgramRunCursorV1>,
    pub budgets: ProgramRunBudgetLimitsV1,
    #[serde(default)]
    pub locks: Vec<ProgramRunLockRequirementV1>,
    pub max_publication_attempts: u32,
}

impl ProgramRunTemplateV1 {
    pub fn normalized(&self) -> Result<Self, String> {
        validate_key("template_key", &self.template_key)?;
        if self.template_version == 0 || self.max_publication_attempts == 0 {
            return Err(
                "template_version and max_publication_attempts must be positive".to_string(),
            );
        }
        if self.cursors.is_empty() || self.cursors.len() > PROGRAM_RUN_MAX_CURSORS {
            return Err("template must contain 1..=256 cursors".to_string());
        }
        self.budgets.validate()?;

        let mut normalized = self.clone();
        let mut cursor_keys = BTreeSet::new();
        let mut phases = BTreeSet::new();
        for (ordinal, cursor) in normalized.cursors.iter_mut().enumerate() {
            validate_key("cursor key", &cursor.key)?;
            validate_key("cursor phase", &cursor.phase)?;
            if !cursor_keys.insert(cursor.key.clone()) || !phases.insert(cursor.phase.clone()) {
                return Err("cursor keys and phases must be unique".to_string());
            }
            if cursor.required_gates.len() > PROGRAM_RUN_MAX_GATES_PER_CURSOR {
                return Err("cursor may contain at most 32 required gates".to_string());
            }
            for gate in &cursor.required_gates {
                gate.validate()?;
            }
            cursor
                .required_gates
                .sort_by(|left, right| left.gate_key.cmp(&right.gate_key));
            if cursor
                .required_gates
                .windows(2)
                .any(|pair| pair[0].gate_key == pair[1].gate_key)
            {
                return Err("required gate keys must be unique per cursor".to_string());
            }
            if cursor
                .revision_target_ordinal
                .is_some_and(|target| target as usize > ordinal)
            {
                return Err(
                    "revision target must name the current or an earlier cursor".to_string()
                );
            }
        }
        normalized
            .locks
            .sort_by_key(|lock| lock.conflict_domain.as_str());
        if normalized
            .locks
            .windows(2)
            .any(|pair| pair[0].conflict_domain == pair[1].conflict_domain)
        {
            return Err("lock domains must be unique".to_string());
        }
        let canonical = canonical_program_run_json(&normalized)?;
        if canonical.len() > PROGRAM_RUN_MAX_CANONICAL_JSON_BYTES {
            return Err("normalized template exceeds 1 MiB".to_string());
        }
        Ok(normalized)
    }

    pub fn canonical_json(&self) -> Result<String, String> {
        canonical_program_run_json(&self.normalized()?)
    }

    pub fn digest(&self) -> Result<String, String> {
        let canonical = self.canonical_json()?;
        Ok(program_run_fingerprint(
            "program-run-template:v1",
            canonical.as_bytes(),
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgramRunV1 {
    pub id: Uuid,
    pub project_id: Uuid,
    pub idea_id: Uuid,
    pub template_key: String,
    pub template_version: u32,
    pub template_digest: String,
    #[serde(skip_serializing)]
    pub template_json: String,
    pub status: ProgramRunStatusV1,
    #[serde(default)]
    pub cursor_ordinal: Option<u32>,
    #[serde(default)]
    pub cursor_key: Option<String>,
    #[serde(default)]
    pub cursor_phase: Option<String>,
    pub revision_no: u32,
    pub controller_session_id: Uuid,
    pub controller_epoch: u64,
    pub idea_row_version: u64,
    pub row_version: u64,
    pub next_transition_sequence: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub settled_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub cancelled_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub failed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgramRunTransitionV1 {
    pub id: Uuid,
    pub program_run_id: Uuid,
    pub sequence: u64,
    pub operation: ProgramRunOperationV1,
    #[serde(default)]
    pub from_status: Option<ProgramRunStatusV1>,
    pub to_status: ProgramRunStatusV1,
    #[serde(default)]
    pub old_cursor_ordinal: Option<u32>,
    #[serde(default)]
    pub new_cursor_ordinal: Option<u32>,
    pub old_revision_no: u32,
    pub new_revision_no: u32,
    pub actor_kind: ProgramRunActorKindV1,
    #[serde(default)]
    pub actor_session_id: Option<Uuid>,
    pub controller_epoch: u64,
    pub expected_run_version: u64,
    pub resulting_run_version: u64,
    pub expected_idea_version: u64,
    pub resulting_idea_version: u64,
    pub idea_event_id: Uuid,
    pub idempotency_key: String,
    pub request_fingerprint: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgramRunGateEvaluationV1 {
    pub id: Uuid,
    pub program_run_id: Uuid,
    pub cursor_ordinal: u32,
    pub cursor_key: String,
    pub revision_no: u32,
    pub gate_key: String,
    pub evaluation_no: u32,
    pub result: ProgramRunGateResultV1,
    pub policy_key: String,
    pub policy_version: u32,
    pub evidence_digest: String,
    pub transition_id: Uuid,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgramRunBudgetV1 {
    pub program_run_id: Uuid,
    pub dimension: ProgramRunBudgetDimensionV1,
    pub limit_value: u64,
    pub reserved_value: u64,
    pub used_value: u64,
    pub row_version: u64,
    pub updated_at: DateTime<Utc>,
}

impl ProgramRunBudgetV1 {
    #[must_use]
    pub const fn remaining(&self) -> u64 {
        self.limit_value
            .saturating_sub(self.reserved_value + self.used_value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgramRunLockV1 {
    pub id: Uuid,
    pub queue_sequence: u64,
    pub project_id: Uuid,
    pub lock_key: String,
    pub conflict_domain: ProgramRunLockDomainV1,
    pub program_run_id: Uuid,
    pub state: ProgramRunLockStateV1,
    pub controller_session_id: Uuid,
    pub controller_epoch: u64,
    pub lease_generation: u64,
    #[serde(default)]
    pub owner_boot_id: Option<Uuid>,
    pub requested_at: DateTime<Utc>,
    #[serde(default)]
    pub acquired_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub heartbeat_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub released_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub queue_position: Option<u32>,
    pub queue_depth: u32,
    pub expiry_eligible: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgramRunActionV1 {
    pub id: Uuid,
    pub program_run_id: Uuid,
    pub transition_id: Uuid,
    pub action_kind: ProgramRunActionKindV1,
    pub purpose: ProgramRunActionPurposeV1,
    pub request_fingerprint: String,
    pub downstream_dedup_key: String,
    pub controller_session_id: Uuid,
    pub controller_epoch: u64,
    pub not_before: DateTime<Utc>,
    pub state: ProgramRunActionStateV1,
    #[serde(default)]
    pub claim_boot_id: Option<Uuid>,
    pub claim_generation: u64,
    #[serde(default)]
    pub claim_run_version: Option<u64>,
    #[serde(default)]
    pub claim_lease_generation: Option<u64>,
    #[serde(default)]
    pub claimed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub claim_expires_at: Option<DateTime<Utc>>,
    pub publication_attempts: u32,
    pub max_publication_attempts: u32,
    #[serde(default)]
    pub external_model_invocation_id: Option<Uuid>,
    #[serde(default)]
    pub external_session_id: Option<Uuid>,
    #[serde(default)]
    pub scheduled_job_id: Option<Uuid>,
    #[serde(default)]
    pub last_error_class: Option<String>,
    #[serde(default)]
    pub last_error_message: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub published_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub acknowledged_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgramRunAttemptRefV1 {
    pub id: Uuid,
    pub program_run_id: Uuid,
    pub action_id: Uuid,
    pub cursor_ordinal: u32,
    pub cursor_key: String,
    pub revision_no: u32,
    pub attempt_no: u32,
    pub state: ProgramRunAttemptStateV1,
    #[serde(default)]
    pub session_id: Option<Uuid>,
    #[serde(default)]
    pub model_invocation_id: Option<Uuid>,
    #[serde(default)]
    pub observed_session_status: Option<String>,
    #[serde(default)]
    pub observed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub output_digest: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgramRunPageCursorV1 {
    pub updated_at: DateTime<Utc>,
    pub id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgramRunPageV1 {
    pub items: Vec<ProgramRunV1>,
    #[serde(default)]
    pub next_cursor: Option<ProgramRunPageCursorV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgramRunTransitionPageV1 {
    pub items: Vec<ProgramRunTransitionV1>,
    #[serde(default)]
    pub next_sequence: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgramRunOperationalStatusV1 {
    pub run: ProgramRunV1,
    #[serde(default)]
    pub idea_controller_session_id: Option<Uuid>,
    pub idea_controller_epoch: u64,
    pub controller_a6_live: bool,
    pub controller_matches_idea: bool,
    pub locks: Vec<ProgramRunLockV1>,
    #[serde(default)]
    pub active_action: Option<ProgramRunActionV1>,
    #[serde(default)]
    pub current_attempt: Option<ProgramRunAttemptRefV1>,
    pub gates: Vec<ProgramRunGateEvaluationV1>,
    pub required_gates: Vec<ProgramRunRequiredGateStatusV1>,
    pub budgets: Vec<ProgramRunBudgetV1>,
    pub reconciliation_class: ProgramRunReconciliationClassV1,
    pub next_action: ProgramRunNextActionV1,
    #[serde(default)]
    pub safe_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgramRunRequiredGateStatusV1 {
    pub gate_key: String,
    pub policy_key: String,
    pub policy_version: u32,
    #[serde(default)]
    pub latest_evaluation: Option<ProgramRunGateEvaluationV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateProgramRunRequestV1 {
    pub idea_id: Uuid,
    pub expected_idea_row_version: u64,
    pub idempotency_key: String,
    pub template: ProgramRunTemplateV1,
}

impl CreateProgramRunRequestV1 {
    pub fn normalized(&self) -> Result<Self, String> {
        validate_uuid("idea_id", self.idea_id)?;
        validate_idempotency_key(&self.idempotency_key)?;
        Ok(Self {
            template: self.template.normalized()?,
            ..self.clone()
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgramRunTransitionRequestV1 {
    pub program_run_id: Uuid,
    pub expected_run_version: u64,
    pub expected_idea_version: u64,
    pub operation: ProgramRunOperationV1,
    pub idempotency_key: String,
    #[serde(default)]
    pub reason: Option<String>,
}

impl ProgramRunTransitionRequestV1 {
    pub fn validate(&self) -> Result<(), String> {
        validate_uuid("program_run_id", self.program_run_id)?;
        validate_idempotency_key(&self.idempotency_key)?;
        if self
            .reason
            .as_deref()
            .is_some_and(|reason| reason.is_empty() || reason.len() > PROGRAM_RUN_MAX_REASON_BYTES)
        {
            return Err("reason must contain 1..=2048 bytes when present".to_string());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimProgramRunActionRequestV1 {
    pub program_run_id: Uuid,
    pub expected_run_version: u64,
    pub expected_idea_version: u64,
    pub idempotency_key: String,
    pub action_id: Uuid,
    pub claim_boot_id: Uuid,
    pub claim_generation: u64,
    pub claim_run_version: u64,
    pub claim_lease_generation: u64,
}

impl ClaimProgramRunActionRequestV1 {
    pub fn validate(&self) -> Result<(), String> {
        validate_semantic_action_request(
            self.program_run_id,
            self.action_id,
            self.claim_boot_id,
            self.claim_generation,
            self.claim_run_version,
            &self.idempotency_key,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcknowledgeProgramRunWakeRequestV1 {
    pub program_run_id: Uuid,
    pub expected_run_version: u64,
    pub expected_idea_version: u64,
    pub idempotency_key: String,
    pub action_id: Uuid,
    pub claim_boot_id: Uuid,
    pub claim_generation: u64,
    pub claim_run_version: u64,
    pub claim_lease_generation: u64,
}

impl AcknowledgeProgramRunWakeRequestV1 {
    pub fn validate(&self) -> Result<(), String> {
        validate_semantic_action_request(
            self.program_run_id,
            self.action_id,
            self.claim_boot_id,
            self.claim_generation,
            self.claim_run_version,
            &self.idempotency_key,
        )
    }
}

fn validate_semantic_action_request(
    program_run_id: Uuid,
    action_id: Uuid,
    claim_boot_id: Uuid,
    claim_generation: u64,
    claim_run_version: u64,
    idempotency_key: &str,
) -> Result<(), String> {
    validate_uuid("program_run_id", program_run_id)?;
    validate_uuid("action_id", action_id)?;
    validate_uuid("claim_boot_id", claim_boot_id)?;
    validate_idempotency_key(idempotency_key)?;
    if claim_generation == 0 || claim_run_version == 0 {
        return Err("claim_generation and claim_run_version must be positive".to_string());
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitProgramRunOutputRequestV1 {
    pub program_run_id: Uuid,
    pub attempt_id: Uuid,
    pub expected_run_version: u64,
    pub expected_idea_version: u64,
    pub idempotency_key: String,
    pub output_ref: String,
    pub output_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordProgramRunGateRequestV1 {
    pub program_run_id: Uuid,
    pub expected_run_version: u64,
    pub expected_idea_version: u64,
    pub idempotency_key: String,
    pub gate_key: String,
    pub result: ProgramRunGateResultV1,
    pub policy_key: String,
    pub policy_version: u32,
    pub evidence_ref: String,
    pub evidence_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelProgramRunRequestV1 {
    pub program_run_id: Uuid,
    pub expected_run_version: u64,
    pub expected_idea_version: u64,
    pub idempotency_key: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeBlockedProgramRunRequestV1 {
    pub program_run_id: Uuid,
    pub expected_run_version: u64,
    pub expected_idea_version: u64,
    pub idempotency_key: String,
    pub changed_evidence_digest: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgramRunMutationResultV1 {
    pub run: ProgramRunV1,
    pub transition: ProgramRunTransitionV1,
    pub idea_event_id: Uuid,
    pub deduplicated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgramRunActionClaimResultV1 {
    pub action: ProgramRunActionV1,
    #[serde(default)]
    pub semantic_claim: Option<ClaimProgramRunActionRequestV1>,
    pub deduplicated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgramRunActionPublicationResultV1 {
    pub action: ProgramRunActionV1,
    pub deduplicated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgramRunExternalReferenceResultV1 {
    pub action: ProgramRunActionV1,
    pub deduplicated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgramRunActionAcknowledgementResultV1 {
    pub action: ProgramRunActionV1,
    pub deduplicated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgramRunReconciliationItemV1 {
    pub program_run_id: Uuid,
    pub class: ProgramRunReconciliationClassV1,
    pub next_action: ProgramRunNextActionV1,
    pub mutated: bool,
    #[serde(default)]
    pub safe_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgramRunReconciliationPageV1 {
    pub items: Vec<ProgramRunReconciliationItemV1>,
    #[serde(default)]
    pub next_cursor: Option<ProgramRunPageCursorV1>,
    pub deadline_reached: bool,
    pub dry_run: bool,
}

pub fn canonical_program_run_json<T: Serialize>(value: &T) -> Result<String, String> {
    let value = serde_json::to_value(value).map_err(|error| error.to_string())?;
    let value = canonicalize_json(value);
    let canonical = serde_json::to_string(&value).map_err(|error| error.to_string())?;
    if canonical.len() > PROGRAM_RUN_MAX_CANONICAL_JSON_BYTES {
        return Err("canonical ProgramRun JSON exceeds 1 MiB".to_string());
    }
    Ok(canonical)
}

fn canonicalize_json(value: Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .into_iter()
                .map(|(key, value)| (key, canonicalize_json(value)))
                .collect::<BTreeMap<_, _>>()
                .into_iter()
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.into_iter().map(canonicalize_json).collect()),
        scalar => scalar,
    }
}

#[must_use]
#[allow(clippy::format_collect)]
pub fn program_run_fingerprint(domain: &str, bytes: &[u8]) -> String {
    let mut input = Vec::with_capacity(domain.len() + 1 + bytes.len());
    input.extend_from_slice(domain.as_bytes());
    input.push(0);
    input.extend_from_slice(bytes);
    let digest = sha256(&input);
    let hex = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("sha256:{hex}")
}

const PROGRAM_RUN_NAMESPACE: Uuid = Uuid::from_u128(0x7d05_0001_c50d_5e11_8a11_d05d_0000_0001);
const TRANSITION_NAMESPACE: Uuid = Uuid::from_u128(0x7d05_0002_c50d_5e11_8a11_d05d_0000_0002);
const GATE_NAMESPACE: Uuid = Uuid::from_u128(0x7d05_0003_c50d_5e11_8a11_d05d_0000_0003);
const ACTION_NAMESPACE: Uuid = Uuid::from_u128(0x7d05_0004_c50d_5e11_8a11_d05d_0000_0004);
const ATTEMPT_NAMESPACE: Uuid = Uuid::from_u128(0x7d05_0005_c50d_5e11_8a11_d05d_0000_0005);
const LOCK_NAMESPACE: Uuid = Uuid::from_u128(0x7d05_0006_c50d_5e11_8a11_d05d_0000_0006);

#[must_use]
pub fn deterministic_program_run_id(idea_id: Uuid, idempotency_key: &str) -> Uuid {
    deterministic_id(
        PROGRAM_RUN_NAMESPACE,
        &[idea_id.as_bytes(), idempotency_key.as_bytes()],
    )
}

#[must_use]
pub fn deterministic_program_run_transition_id(run_id: Uuid, idempotency_key: &str) -> Uuid {
    deterministic_id(
        TRANSITION_NAMESPACE,
        &[run_id.as_bytes(), idempotency_key.as_bytes()],
    )
}

#[must_use]
pub fn deterministic_program_run_gate_id(run_id: Uuid, idempotency_key: &str) -> Uuid {
    deterministic_id(
        GATE_NAMESPACE,
        &[run_id.as_bytes(), idempotency_key.as_bytes()],
    )
}

#[must_use]
pub fn deterministic_program_run_action_id(
    transition_id: Uuid,
    purpose: ProgramRunActionPurposeV1,
) -> Uuid {
    deterministic_id(
        ACTION_NAMESPACE,
        &[transition_id.as_bytes(), purpose.as_str().as_bytes()],
    )
}

#[must_use]
pub fn deterministic_program_run_attempt_id(action_id: Uuid, attempt_no: u32) -> Uuid {
    deterministic_id(
        ATTEMPT_NAMESPACE,
        &[action_id.as_bytes(), &attempt_no.to_be_bytes()],
    )
}

#[must_use]
pub fn deterministic_program_run_lock_id(run_id: Uuid, domain: ProgramRunLockDomainV1) -> Uuid {
    deterministic_id(
        LOCK_NAMESPACE,
        &[run_id.as_bytes(), domain.as_str().as_bytes()],
    )
}

fn deterministic_id(namespace: Uuid, parts: &[&[u8]]) -> Uuid {
    let mut bytes = Vec::new();
    for part in parts {
        bytes.extend_from_slice(&(part.len() as u64).to_be_bytes());
        bytes.extend_from_slice(part);
    }
    Uuid::new_v5(&namespace, &bytes)
}

#[must_use]
pub fn canonical_program_run_timestamp(timestamp: DateTime<Utc>) -> String {
    timestamp.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

#[must_use]
#[allow(clippy::unnested_or_patterns)]
pub const fn program_run_transition_allowed(
    from: ProgramRunStatusV1,
    to: ProgramRunStatusV1,
) -> bool {
    use ProgramRunStatusV1 as S;
    matches!(
        (from, to),
        (S::Pending, S::Ready | S::Blocked | S::Failed | S::Cancelled)
            | (S::Ready, S::Running | S::Blocked | S::Failed | S::Cancelled)
            | (
                S::Running,
                S::Running
                    | S::Ready
                    | S::AwaitingGate
                    | S::RetryPending
                    | S::Blocked
                    | S::Settled
                    | S::Failed
                    | S::Cancelled
            )
            | (
                S::AwaitingGate,
                S::AwaitingGate
                    | S::Ready
                    | S::RetryPending
                    | S::Blocked
                    | S::Settled
                    | S::Failed
                    | S::Cancelled
            )
            | (
                S::RetryPending,
                S::Ready | S::Blocked | S::Failed | S::Cancelled
            )
            | (S::Blocked, S::Ready | S::Failed | S::Cancelled)
    )
}

fn validate_uuid(name: &str, value: Uuid) -> Result<(), String> {
    if value.is_nil() {
        return Err(format!("{name} must not be nil"));
    }
    Ok(())
}

fn validate_key(name: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() || value.len() > PROGRAM_RUN_MAX_KEY_BYTES {
        return Err(format!("{name} must contain 1..=64 bytes"));
    }
    Ok(())
}

fn validate_idempotency_key(value: &str) -> Result<(), String> {
    if value.trim().is_empty() || value.len() > PROGRAM_RUN_MAX_IDEMPOTENCY_KEY_BYTES {
        return Err("idempotency_key must contain 1..=256 bytes".to_string());
    }
    Ok(())
}

// Small dependency-free SHA-256 implementation. rsi-common intentionally
// keeps its dependency surface fixed, so this canonical wire helper cannot use
// the daemon-only sha2 dependency.
#[allow(
    clippy::expect_used,
    clippy::many_single_char_names,
    clippy::unreadable_literal
)]
fn sha256(input: &[u8]) -> [u8; 32] {
    const INITIAL: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let bit_len = (input.len() as u64) * 8;
    let mut padded = input.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());
    let mut state = INITIAL;
    for chunk in padded.chunks_exact(64) {
        let mut words = [0_u32; 64];
        for (index, word) in words.iter_mut().take(16).enumerate() {
            *word = u32::from_be_bytes(chunk[index * 4..index * 4 + 4].try_into().expect("word"));
        }
        for index in 16..64 {
            let s0 = words[index - 15].rotate_right(7)
                ^ words[index - 15].rotate_right(18)
                ^ (words[index - 15] >> 3);
            let s1 = words[index - 2].rotate_right(17)
                ^ words[index - 2].rotate_right(19)
                ^ (words[index - 2] >> 10);
            words[index] = words[index - 16]
                .wrapping_add(s0)
                .wrapping_add(words[index - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(choice)
                .wrapping_add(K[index])
                .wrapping_add(words[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }
    let mut output = [0_u8; 32];
    for (index, word) in state.iter().enumerate() {
        output[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    output
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn template() -> ProgramRunTemplateV1 {
        ProgramRunTemplateV1 {
            template_key: "implementation".to_string(),
            template_version: 1,
            cursors: vec![
                ProgramRunCursorV1 {
                    key: "research".to_string(),
                    phase: "research".to_string(),
                    required_gates: vec![ProgramRunGateRequirementV1 {
                        gate_key: "accepted".to_string(),
                        policy_key: "artifact".to_string(),
                        policy_version: 1,
                    }],
                    revision_target_ordinal: Some(0),
                },
                ProgramRunCursorV1 {
                    key: "implement".to_string(),
                    phase: "implementation".to_string(),
                    required_gates: vec![],
                    revision_target_ordinal: Some(0),
                },
            ],
            budgets: ProgramRunBudgetLimitsV1 {
                productive_transitions: 12,
                work_attempts: 4,
                launch_retries: 1,
                revisions: 1,
                wake_reservations: 4,
                action_publication_retries: 2,
            },
            locks: vec![ProgramRunLockRequirementV1 {
                conflict_domain: ProgramRunLockDomainV1::IdeaController,
            }],
            max_publication_attempts: 3,
        }
    }

    #[test]
    fn d05_program_run_wire_contracts_are_exact_and_redacted() {
        let idea_id = Uuid::new_v4();
        let request: CreateProgramRunRequestV1 = serde_json::from_value(serde_json::json!({
            "idea_id": idea_id,
            "expected_idea_row_version": 3,
            "idempotency_key": "create-1",
            "template": template(),
        }))
        .unwrap();
        assert_eq!(request.normalized().unwrap().template.cursors.len(), 2);
        for forbidden in [
            "controller_session_id",
            "controller_epoch",
            "cursor_ordinal",
            "status",
            "claim_generation",
        ] {
            let mut value = serde_json::to_value(&request).unwrap();
            value
                .as_object_mut()
                .unwrap()
                .insert(forbidden.to_string(), Value::String("forged".to_string()));
            assert!(
                serde_json::from_value::<CreateProgramRunRequestV1>(value).is_err(),
                "accepted spoof field {forbidden}"
            );
        }
        let mut run = ProgramRunV1 {
            id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            idea_id,
            template_key: "implementation".to_string(),
            template_version: 1,
            template_digest:
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    .to_string(),
            template_json: "{\"secret\":true}".to_string(),
            status: ProgramRunStatusV1::Pending,
            cursor_ordinal: Some(0),
            cursor_key: Some("research".to_string()),
            cursor_phase: Some("research".to_string()),
            revision_no: 0,
            controller_session_id: Uuid::new_v4(),
            controller_epoch: 1,
            idea_row_version: 4,
            row_version: 0,
            next_transition_sequence: 2,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            settled_at: None,
            cancelled_at: None,
            failed_at: None,
        };
        let encoded = serde_json::to_value(&run).unwrap();
        assert!(encoded.get("template_json").is_none());
        run.template_json.clear();

        let semantic_claim = ClaimProgramRunActionRequestV1 {
            program_run_id: run.id,
            expected_run_version: 1,
            expected_idea_version: 5,
            idempotency_key: "claim-action-1".into(),
            action_id: Uuid::new_v4(),
            claim_boot_id: Uuid::new_v4(),
            claim_generation: 2,
            claim_run_version: 1,
            claim_lease_generation: 0,
        };
        semantic_claim.validate().unwrap();
        let mut forged = serde_json::to_value(&semantic_claim).unwrap();
        forged.as_object_mut().unwrap().insert(
            "reason".into(),
            Value::String("claim witness cannot be smuggled through reason".into()),
        );
        assert!(serde_json::from_value::<ClaimProgramRunActionRequestV1>(forged).is_err());
        let mut missing_generation = semantic_claim;
        missing_generation.claim_generation = 0;
        assert!(missing_generation.validate().is_err());
    }

    #[test]
    fn d05_program_run_state_table_and_cursor_gates_are_exhaustive() {
        for from in ProgramRunStatusV1::ALL {
            for to in ProgramRunStatusV1::ALL {
                if from.is_terminal() {
                    assert!(!program_run_transition_allowed(*from, *to));
                }
            }
        }
        assert!(program_run_transition_allowed(
            ProgramRunStatusV1::AwaitingGate,
            ProgramRunStatusV1::Settled
        ));
        assert!(!program_run_transition_allowed(
            ProgramRunStatusV1::Ready,
            ProgramRunStatusV1::Settled
        ));
        let mut duplicate_gate = template();
        let duplicated_requirement = duplicate_gate.cursors[0].required_gates[0].clone();
        duplicate_gate.cursors[0]
            .required_gates
            .push(duplicated_requirement);
        assert!(duplicate_gate.normalized().is_err());
        let mut invalid_revision = template();
        invalid_revision.cursors[0].revision_target_ordinal = Some(1);
        assert!(invalid_revision.normalized().is_err());
    }

    #[test]
    fn d05_program_run_canonicalization_and_identity_domains_are_stable() {
        assert_eq!(
            program_run_fingerprint("", b"abc"),
            "sha256:609f6e36d2405585188d5cfd761f407c7cc46a7d3f314c88270469dde315fcd1"
        );
        let value_a = serde_json::json!({"z": [3, 2, 1], "a": {"y": 2, "x": 1}});
        let value_b = serde_json::json!({"a": {"x": 1, "y": 2}, "z": [3, 2, 1]});
        assert_eq!(
            canonical_program_run_json(&value_a).unwrap(),
            canonical_program_run_json(&value_b).unwrap()
        );
        let run = deterministic_program_run_id(Uuid::nil(), "same");
        let transition = deterministic_program_run_transition_id(run, "same");
        let action = deterministic_program_run_action_id(
            transition,
            ProgramRunActionPurposeV1::ExecuteCursor,
        );
        assert_ne!(run, transition);
        assert_ne!(transition, action);
        assert_eq!(run, deterministic_program_run_id(Uuid::nil(), "same"));
    }
}
