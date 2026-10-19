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
}

impl CoordinatorBudgetV1 {
    /// The same explanation in the authority guidance and operator settings.
    pub fn summary(&self) -> String {
        let effective = match (self.effective_tokens, self.effective_pct) {
            (Some(tokens), Some(pct)) => format!("{tokens} tokens ({pct:.2}%)"),
            _ => "disabled or unknown".into(),
        };
        format!(
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
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoordinatorSeatBudgetV1 {
    pub session_id: uuid::Uuid,
    pub budget: CoordinatorBudgetV1,
}
