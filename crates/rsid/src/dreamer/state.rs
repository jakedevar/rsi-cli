use crate::error::{DaemonError, Result};
use crate::store::Store;
use chrono::{DateTime, Utc};
use rsi_common::model_control::ModelUsageConfidence;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const DREAM_RUNTIME_STATE_KEY: &str = "runtime_state_v2";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DreamPhase {
    Extraction,
    Deduction,
    Induction,
}

impl DreamPhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Extraction => "extraction",
            Self::Deduction => "deduction",
            Self::Induction => "induction",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DreamRunStatus {
    Running,
    Paused,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DreamConsumption {
    pub model_calls: u32,
    pub estimated_input_tokens: u64,
    pub estimated_output_tokens: u64,
    pub estimated_total_tokens: u64,
    pub wall_time_ms: u64,
    pub confidence: ModelUsageConfidence,
}

impl Default for DreamConsumption {
    fn default() -> Self {
        Self {
            model_calls: 0,
            estimated_input_tokens: 0,
            estimated_output_tokens: 0,
            estimated_total_tokens: 0,
            wall_time_ms: 0,
            confidence: ModelUsageConfidence::Unavailable,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DreamProgress {
    pub total_items: u64,
    pub completed_items: u64,
    pub sessions_processed: u64,
    pub observations_extracted: u64,
    pub deductions_created: u64,
    pub patterns_identified: u64,
    pub observations_superseded: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DreamCaps {
    pub max_items_per_phase: usize,
    pub max_model_calls: u32,
    pub max_estimated_input_tokens: u64,
    pub max_estimated_output_tokens: u64,
    pub max_estimated_total_tokens: u64,
    pub max_wall_time_ms: u64,
    pub max_concurrency: u32,
    pub cooldown_secs: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DreamCheckpoint {
    pub extract_session_ids: Vec<Uuid>,
    pub extract_index: usize,
    pub deduction_project_ids: Vec<Option<Uuid>>,
    pub deduction_index: usize,
    pub induction_project_ids: Vec<Option<Uuid>>,
    pub induction_index: usize,
    pub current_item_key: Option<String>,
    pub current_response: Option<String>,
    pub current_dedup_key: Option<String>,
    pub current_fingerprint: Option<String>,
    #[serde(default)]
    pub current_invocation_id: Option<Uuid>,
    #[serde(default)]
    pub current_input_estimate: Option<u64>,
    #[serde(default)]
    pub current_output_estimate: Option<u64>,
    #[serde(default)]
    pub current_wall_time_ms: Option<u64>,
    #[serde(default)]
    pub current_result_hash: Option<String>,
    #[serde(default)]
    pub current_settled: bool,
    #[serde(default)]
    pub current_consumption_recorded: bool,
}

impl DreamCheckpoint {
    pub fn phase_item_count(&self, phase: DreamPhase) -> usize {
        match phase {
            DreamPhase::Extraction => self.extract_session_ids.len(),
            DreamPhase::Deduction => self.deduction_project_ids.len(),
            DreamPhase::Induction => self.induction_project_ids.len(),
        }
    }

    pub fn phase_index(&self, phase: DreamPhase) -> usize {
        match phase {
            DreamPhase::Extraction => self.extract_index,
            DreamPhase::Deduction => self.deduction_index,
            DreamPhase::Induction => self.induction_index,
        }
    }

    pub fn clear_current_call(&mut self) {
        self.current_item_key = None;
        self.current_response = None;
        self.current_dedup_key = None;
        self.current_fingerprint = None;
        self.current_invocation_id = None;
        self.current_input_estimate = None;
        self.current_output_estimate = None;
        self.current_wall_time_ms = None;
        self.current_result_hash = None;
        self.current_settled = false;
        self.current_consumption_recorded = false;
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DreamRunState {
    pub run_id: Uuid,
    pub owner: String,
    pub trigger: String,
    pub status: DreamRunStatus,
    pub phase: DreamPhase,
    pub checkpoint: DreamCheckpoint,
    pub progress: DreamProgress,
    pub consumption: DreamConsumption,
    pub caps: DreamCaps,
    pub started_at: String,
    pub updated_at: String,
    pub completed_at: Option<String>,
    pub terminal_reason: Option<String>,
    pub pause_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DreamRuntimeState {
    pub version: u32,
    pub active_run: Option<DreamRunState>,
    pub last_success_at: Option<String>,
    pub cooldown_until: Option<String>,
    pub last_terminal_reason: Option<String>,
    pub recent_consumption: DreamConsumption,
}

impl Default for DreamRuntimeState {
    fn default() -> Self {
        Self {
            version: 3,
            active_run: None,
            last_success_at: None,
            cooldown_until: None,
            last_terminal_reason: None,
            recent_consumption: DreamConsumption::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DreamStatusSnapshot {
    pub enabled: bool,
    pub active_run_id: Option<Uuid>,
    pub active_owner: Option<String>,
    pub status: Option<DreamRunStatus>,
    pub phase: Option<DreamPhase>,
    pub progress: DreamProgress,
    pub last_success_at: Option<String>,
    pub cooldown_until: Option<String>,
    pub recent_consumption: DreamConsumption,
    pub reason: Option<String>,
}

pub fn load(store: &Store) -> Result<DreamRuntimeState> {
    let Some(raw) = store.get_dream_state(DREAM_RUNTIME_STATE_KEY)? else {
        return Ok(DreamRuntimeState::default());
    };
    serde_json::from_str(&raw).map_err(|e| {
        DaemonError::Store(format!(
            "failed to decode dream runtime state from {DREAM_RUNTIME_STATE_KEY}: {e}"
        ))
    })
}

pub fn save(store: &Store, state: &DreamRuntimeState, now: DateTime<Utc>) -> Result<()> {
    let encoded = serde_json::to_string(state)
        .map_err(|e| DaemonError::Store(format!("failed to encode dream runtime state: {e}")))?;
    store.set_dream_state_at(DREAM_RUNTIME_STATE_KEY, &encoded, now)
}

pub fn snapshot(
    state: &DreamRuntimeState,
    enabled: bool,
    reason: Option<String>,
) -> DreamStatusSnapshot {
    let active = state.active_run.as_ref();
    DreamStatusSnapshot {
        enabled,
        active_run_id: active.map(|run| run.run_id),
        active_owner: active.map(|run| run.owner.clone()),
        status: active.map(|run| run.status),
        phase: active.map(|run| run.phase),
        progress: active.map(|run| run.progress.clone()).unwrap_or_default(),
        last_success_at: state.last_success_at.clone(),
        cooldown_until: state.cooldown_until.clone(),
        recent_consumption: active
            .map(|run| run.consumption.clone())
            .unwrap_or_else(|| state.recent_consumption.clone()),
        reason: reason.or_else(|| active.and_then(|run| run.pause_reason.clone())),
    }
}
