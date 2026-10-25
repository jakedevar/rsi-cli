//! Host-local, operator-owned resource preferences and bounded saved presets (#3532).
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const STORAGE_KEY: &str = "host_tuning.v1";
/// Numeric resource settings only. Validation still goes through RuntimeConfig.
pub const RESOURCE_KEYS: &[(&str, &str)] = &[
    ("worker_cpu_quota_percent", "next_launch"),
    ("hub_build_cpu_percent", "live"),
    ("host_load_admission_threshold", "live"),
    ("governor_build_slots", "live"),
    ("governor_check_slots", "live"),
    ("governor_lander_slots", "live"),
    ("governor_min_avail_mem_gb", "live"),
    ("governor_min_free_disk_gb", "live"),
    ("agent_build_jobs", "next_launch"),
    ("agent_build_slots", "next_launch"),
    ("agent_build_sccache_cache_gib", "next_launch"),
    ("sandbox_min_free_gib", "live"),
    ("disk_pressure_min_free_gb", "live"),
    ("target_pool_keep", "live"),
    ("sandbox_build_cache_reclaim_ttl_secs", "live"),
];

pub type Values = BTreeMap<String, u64>;
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostProfile {
    #[default]
    Unconfigured,
    Shared {
        reserve_percent: u8,
    },
    Dedicated,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bounds {
    pub min: u64,
    pub max: u64,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TuningConfiguration {
    pub profile: HostProfile,
    /// Every preset value requires an explicit operator bound for that key.
    pub bounds: BTreeMap<String, Bounds>,
    pub presets: BTreeMap<String, Values>,
    /// Guidance only; this never widens a manager's session-creation grant.
    pub live_session_cap_guidance: Option<u32>,
}
impl TuningConfiguration {
    pub fn validate_values(&self, values: &Values) -> Result<(), String> {
        if values.is_empty() {
            return Err("a preset must contain at least one resource setting".into());
        }
        for (key, value) in values {
            if !RESOURCE_KEYS.iter().any(|(allowed, _)| allowed == key) {
                return Err(format!("{key}: unsupported resource setting"));
            }
            let bound = self
                .bounds
                .get(key)
                .ok_or_else(|| format!("{key}: operator bounds required"))?;
            if bound.min > bound.max || !(bound.min..=bound.max).contains(value) {
                return Err(format!(
                    "{key}: outside operator bounds {}..{}",
                    bound.min, bound.max
                ));
            }
        }
        Ok(())
    }
    pub fn validate(&self) -> Result<(), String> {
        if matches!(self.profile, HostProfile::Shared { reserve_percent } if reserve_percent > 100)
        {
            return Err("reserve_percent must be 0..100".into());
        }
        if self.presets.len() > 32 || self.bounds.len() > RESOURCE_KEYS.len() {
            return Err("at most 32 presets and only supported resource bounds".into());
        }
        for (key, bound) in &self.bounds {
            if !RESOURCE_KEYS.iter().any(|(allowed, _)| allowed == key) || bound.min > bound.max {
                return Err(format!("{key}: invalid resource bounds"));
            }
        }
        for (name, values) in &self.presets {
            if name.is_empty()
                || name.len() > 64
                || !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_/ .".contains(c))
            {
                return Err("preset name must be 1..64 plain characters".into());
            }
            self.validate_values(values)?;
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigureHostTuning {
    pub expected_revision: u64,
    pub configuration: TuningConfiguration,
}
/// The single agent verb accepts only a saved preset name or undo, never values.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum SwitchHostPreset {
    Apply { name: String },
    Undo {},
}
impl SwitchHostPreset {
    pub fn validate(&self) -> Result<(), &'static str> {
        if matches!(self, Self::Apply { name } if name.is_empty() || name.len() > 64) {
            return Err("host_tuning_invalid_request");
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoRecord {
    pub values: Values,
    pub previous_active: Option<String>,
    pub target: String,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TuningState {
    pub revision: u64,
    pub configuration: TuningConfiguration,
    pub active: Option<String>,
    pub undo: Option<UndoRecord>,
    /// Set before applying; retained across a failure/crash so undo remains possible.
    pub pending: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Reading<T> {
    Known { value: T },
    Unknown { reason: String },
    Unsupported,
}
impl<T> Reading<T> {
    pub fn from_option(value: Option<T>, reason: &str) -> Self {
        value.map_or_else(
            || Self::Unknown {
                reason: reason.into(),
            },
            |value| Self::Known { value },
        )
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceProbe {
    pub effective_cpus: Reading<f64>,
    pub cgroup_cpu_quota: Reading<f64>,
    pub cgroup: Reading<String>,
    pub total_ram_bytes: Reading<u64>,
    pub available_ram_bytes: Reading<u64>,
    pub data_dir_free_bytes: Reading<u64>,
    pub sandbox_base_free_bytes: Reading<u64>,
    pub load_average: Reading<[f64; 3]>,
    pub cpu_temperature_c: Reading<f64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuningView {
    pub state: TuningState,
    pub probe: ResourceProbe,
    pub actual: Values,
    pub apply_classes: BTreeMap<String, String>,
    pub notes: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwitchReceipt {
    pub changed: bool,
    pub complete: bool,
    pub error: Option<String>,
    /// Includes side-effect reports (e.g. systemd unavailable); never claims enforcement from saved values.
    pub settings_result: Option<serde_json::Value>,
    pub view: TuningView,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn switch_shape_and_resource_bounds_exclude_raw_mutations() {
        for value in [
            serde_json::json!({"action":"apply","name":"day","values":{}}),
            serde_json::json!({"action":"apply"}),
            serde_json::json!({"action":"undo","name":"day"}),
            serde_json::json!({"action":"undo","configuration":{}}),
        ] {
            assert!(serde_json::from_value::<SwitchHostPreset>(value).is_err());
        }
        assert!(
            serde_json::from_value::<SwitchHostPreset>(serde_json::json!({"action":"undo"}))
                .is_ok()
        );
        let mut configuration = TuningConfiguration::default();
        configuration
            .bounds
            .insert("hub_build_cpu_percent".into(), Bounds { min: 10, max: 80 });
        let valid = [("hub_build_cpu_percent".into(), 75)].into();
        configuration.validate_values(&valid).unwrap();
        assert!(
            configuration
                .validate_values(&[("hub_build_cpu_percent".into(), 90)].into())
                .is_err()
        );
        assert!(
            configuration
                .validate_values(&[("worker_cpu_quota_percent".into(), 100)].into())
                .is_err()
        );
        configuration
            .bounds
            .insert("credentials".into(), Bounds { min: 0, max: 100 });
        assert!(configuration.validate().is_err());
    }
}
