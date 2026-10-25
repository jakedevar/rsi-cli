//! Read-only explanation of the live coordinator policy; never a setting.
use crate::{provider_capabilities::ResolvedContextBudget, types::SessionProvider};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoordinatorBudgetV1 {
    pub provider: SessionProvider,
    pub model: Option<String>,
    /// Includes the active denominator and its evidence; a fallback is not a measurement.
    pub window: ResolvedContextBudget,
    pub measured_window_tokens: Option<u64>,
    /// 0 disables the cap, 1..=100 is percent, larger values are tokens.
    pub coordinator_cap: u64,
    pub coordinator_cap_unit: String,
    pub coordinator_cap_source: String,
    pub coordinator_cap_tokens: Option<u64>,
    pub succession_pct: f64,
    pub succession_source: String,
    pub rotation_enabled: bool,
    pub effective_tokens: Option<u64>,
    pub effective_pct: Option<f64>,
    pub binding_limit: Option<String>,
    /// Issue #4021: the seat's wake-driven turn rate against the operator's
    /// per-hour cap. `None` for a read that carries no seat (settings page).
    #[serde(default)]
    pub turn_budget: Option<ManagerTurnBudgetV1>,
}

/// Issue #4021: how many wake-driven turns a manager seat has used in the
/// last rolling hour against `manager_max_turns_per_hour`, and what the
/// scheduler did about the rest. `held_wakes` and `urgent_bypasses` count
/// since the daemon started; `turns_last_hour` is read from the durable
/// invocation ledger.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagerTurnBudgetV1 {
    /// 0 = no cap (every wake delivers at once).
    pub cap_per_hour: u32,
    pub turns_last_hour: u32,
    /// Routine deliveries coalesced into a later turn because the cap was spent.
    pub held_wakes: u32,
    /// Urgent deliveries (gate, operator, queue outcome, safety) made past a spent cap.
    pub urgent_bypasses: u32,
}

impl ManagerTurnBudgetV1 {
    pub fn summary(&self) -> String {
        if self.cap_per_hour == 0 {
            return format!(
                "turn budget off ({} wake turns in the last hour)",
                self.turns_last_hour
            );
        }
        format!(
            "turn budget {}/{} wake turns in the last hour; {} routine wakes coalesced, {} urgent bypasses since daemon start",
            self.turns_last_hour, self.cap_per_hour, self.held_wakes, self.urgent_bypasses
        )
    }
}

/// Setting key of the per-seat turn cap (Issue #4021).
pub const MANAGER_TURNS_PER_HOUR_SETTING: &str = "manager_max_turns_per_hour";
/// Default: no cap.
pub const MANAGER_TURNS_PER_HOUR_DEFAULT: u32 = 0;
/// Largest cap the operator can set.
pub const MANAGER_TURNS_PER_HOUR_MAX: u32 = 60;

impl CoordinatorBudgetV1 {
    /// The same explanation in the authority guidance and operator settings.
    pub fn summary(&self) -> String {
        let effective = match (self.effective_tokens, self.effective_pct) {
            (Some(tokens), Some(pct)) => format!("{tokens} tokens ({pct:.2}%)"),
            _ => "disabled or unknown".into(),
        };
        let mut text = format!(
            "{:?}/{}: window {} tokens ({:?}); cap {} {} [{}]; succession {:.2}% [{}]; effective {} binds {}",
            self.provider,
            self.model.as_deref().unwrap_or("unknown"),
            self.window.active_tokens,
            self.window.evidence.source,
            self.coordinator_cap,
            self.coordinator_cap_unit,
            self.coordinator_cap_source,
            self.succession_pct,
            self.succession_source,
            effective,
            self.binding_limit.as_deref().unwrap_or("none"),
        );
        if let Some(turns) = self.turn_budget {
            text.push_str("; ");
            text.push_str(&turns.summary());
        }
        text
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoordinatorSeatBudgetV1 {
    pub session_id: uuid::Uuid,
    pub budget: CoordinatorBudgetV1,
}
