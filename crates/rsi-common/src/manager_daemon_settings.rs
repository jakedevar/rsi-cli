//! #1046: operator-bounded daemon settings the appointed manager may correct.
//!
//! The curated allowlist below names the operational knobs that can stop the
//! whole fleet (sandbox capacity and reclaim pressure). The operator marks a key
//! manager-adjustable by giving it `min`/`max` bounds in the manager policy and
//! grants the separate `DaemonSettings` capability. Spend policy, credentials,
//! appointment and scope are never in this table (AGENTS.md rule 10).

use serde::{Deserialize, Serialize};

/// One curated, manager-adjustable daemon setting and its hard range (the range
/// the daemon itself accepts; operator bounds must sit inside it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdjustableDaemonSetting {
    /// Daemon settings registry key (`UpdateDaemonConfig` field name).
    pub key: &'static str,
    /// Operator-facing label, as in the settings registry.
    pub label: &'static str,
    pub hard_min: u64,
    pub hard_max: u64,
    /// Values strictly between these two are refused although they sit in
    /// the hard range (a field with two units, e.g. #1254's percentage or
    /// tokens worker cap).
    pub excluded_between: Option<(u64, u64)>,
}

/// The closed allowlist. Order is the TUI row order.
pub const MANAGER_ADJUSTABLE_DAEMON_SETTINGS: &[AdjustableDaemonSetting] = &[
    AdjustableDaemonSetting {
        key: "sandbox_max_source_roots",
        label: "Maximum sandbox roots",
        hard_min: 1,
        hard_max: 65_536,
        excluded_between: None,
    },
    AdjustableDaemonSetting {
        key: "sandbox_min_free_gib",
        label: "Minimum free space (GiB)",
        hard_min: 0,
        hard_max: 1024,
        excluded_between: None,
    },
    AdjustableDaemonSetting {
        key: "sandbox_build_cache_reclaim_high_watermark_pct",
        label: "Cache pressure high",
        hard_min: 2,
        hard_max: 99,
        excluded_between: None,
    },
    AdjustableDaemonSetting {
        key: "sandbox_build_cache_reclaim_low_watermark_pct",
        label: "Cache pressure low",
        hard_min: 1,
        hard_max: 98,
        excluded_between: None,
    },
    // #1254: `0` off, `1..=100` percent of the context window, or tokens in
    // `32_000..=2_000_000`; the daemon applies it with the same validation.
    AdjustableDaemonSetting {
        key: "worker_context_cap_tokens",
        label: "Worker context cap (0 off, 1-100 = % of window)",
        hard_min: 0,
        hard_max: 2_000_000,
        excluded_between: Some((100, 32_000)),
    },
];

impl AdjustableDaemonSetting {
    /// Whether `value` is one the daemon accepts for this key.
    #[must_use]
    pub fn accepts(&self, value: u64) -> bool {
        (self.hard_min..=self.hard_max).contains(&value)
            && self
                .excluded_between
                .is_none_or(|(low, high)| value <= low || value >= high)
    }
}

/// Refusal: the key is not in the curated allowlist.
pub const DAEMON_SETTING_NOT_ALLOWLISTED: &str = "manager_v2_daemon_setting_not_allowlisted";
/// Refusal: the operator has not given the key manager bounds.
pub const DAEMON_SETTING_NOT_ADJUSTABLE: &str = "manager_v2_daemon_setting_not_adjustable";
/// Refusal: the proposed value is outside the operator's bounds.
pub const DAEMON_SETTING_OUT_OF_BOUNDS: &str = "manager_v2_daemon_setting_out_of_bounds";

#[must_use]
pub fn adjustable_daemon_setting(key: &str) -> Option<&'static AdjustableDaemonSetting> {
    MANAGER_ADJUSTABLE_DAEMON_SETTINGS
        .iter()
        .find(|setting| setting.key == key)
}

/// Operator-set bounds for one allowlisted key, stored in the manager policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagerDaemonSettingBoundV2 {
    pub key: String,
    pub min: u64,
    pub max: u64,
}

impl ManagerDaemonSettingBoundV2 {
    pub fn validate(&self) -> Result<(), &'static str> {
        let setting = adjustable_daemon_setting(&self.key).ok_or(DAEMON_SETTING_NOT_ALLOWLISTED)?;
        if self.min > self.max || self.min < setting.hard_min || self.max > setting.hard_max {
            return Err("manager_v2_invalid_daemon_setting_bound");
        }
        Ok(())
    }
}

/// Validate a policy's bound list: allowlisted, in range, no duplicate key.
pub fn validate_daemon_setting_bounds(
    bounds: &[ManagerDaemonSettingBoundV2],
) -> Result<(), &'static str> {
    if bounds.len() > MANAGER_ADJUSTABLE_DAEMON_SETTINGS.len() {
        return Err("manager_v2_invalid_daemon_setting_bound");
    }
    for (i, bound) in bounds.iter().enumerate() {
        bound.validate()?;
        if bounds[..i].iter().any(|old| old.key == bound.key) {
            return Err("manager_v2_invalid_daemon_setting_bound");
        }
    }
    Ok(())
}

/// Check one proposed value against the operator's bounds: the key must be
/// allowlisted, the operator must have bounded it, and the value must sit inside.
pub fn check_daemon_setting_proposal(
    bounds: &[ManagerDaemonSettingBoundV2],
    key: &str,
    value: u64,
) -> Result<(), &'static str> {
    let setting = adjustable_daemon_setting(key).ok_or(DAEMON_SETTING_NOT_ALLOWLISTED)?;
    let bound = bounds
        .iter()
        .find(|bound| bound.key == key)
        .ok_or(DAEMON_SETTING_NOT_ADJUSTABLE)?;
    if value < bound.min || value > bound.max || !setting.accepts(value) {
        return Err(DAEMON_SETTING_OUT_OF_BOUNDS);
    }
    Ok(())
}

/// Params of the delegated `ProposeDaemonSetting` operator method.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProposeDaemonSettingParamsV1 {
    pub key: String,
    pub value: u64,
    /// Why the manager is changing the setting; journaled with the action.
    pub reason: String,
}

impl ProposeDaemonSettingParamsV1 {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.reason.trim().is_empty() || self.reason.len() > 512 || self.reason.contains('\0') {
            return Err("manager_v2_operator_params_invalid");
        }
        // The key is checked against the allowlist and bounds at admission so
        // the caller gets a typed refusal, not a decode error.
        if self.key.is_empty() || self.key.len() > 128 {
            return Err("manager_v2_operator_params_invalid");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bound(key: &str, min: u64, max: u64) -> ManagerDaemonSettingBoundV2 {
        ManagerDaemonSettingBoundV2 {
            key: key.into(),
            min,
            max,
        }
    }

    #[test]
    fn proposal_is_checked_against_allowlist_and_operator_bounds() {
        let bounds = vec![bound("sandbox_max_source_roots", 4096, 32_768)];
        assert!(check_daemon_setting_proposal(&bounds, "sandbox_max_source_roots", 16_384).is_ok());
        assert!(check_daemon_setting_proposal(&bounds, "sandbox_max_source_roots", 4096).is_ok());
        assert!(check_daemon_setting_proposal(&bounds, "sandbox_max_source_roots", 32_768).is_ok());
        for value in [0, 512, 4095, 32_769] {
            assert_eq!(
                check_daemon_setting_proposal(&bounds, "sandbox_max_source_roots", value),
                Err(DAEMON_SETTING_OUT_OF_BOUNDS)
            );
        }
        // In the allowlist but never bounded by the operator: default none.
        assert_eq!(
            check_daemon_setting_proposal(&bounds, "sandbox_min_free_gib", 10),
            Err(DAEMON_SETTING_NOT_ADJUSTABLE)
        );
        // Spend, credentials and any other key are not in the allowlist.
        for key in [
            "max_spend_usd",
            "governor_max_load",
            "anthropic_api_key",
            "",
        ] {
            assert_eq!(
                check_daemon_setting_proposal(&bounds, key, 1),
                Err(DAEMON_SETTING_NOT_ALLOWLISTED)
            );
        }
        assert_eq!(
            check_daemon_setting_proposal(&[], "sandbox_max_source_roots", 16_384),
            Err(DAEMON_SETTING_NOT_ADJUSTABLE)
        );
    }

    /// #1254: the worker cap is curated; a proposal inside the operator's
    /// bounds passes only when the daemon accepts it (percent or tokens).
    #[test]
    fn worker_context_cap_is_curated_and_its_unit_gap_is_refused() {
        let setting = adjustable_daemon_setting("worker_context_cap_tokens").expect("curated");
        assert_eq!((setting.hard_min, setting.hard_max), (0, 2_000_000));
        let bounds = vec![bound("worker_context_cap_tokens", 0, 400_000)];
        assert!(validate_daemon_setting_bounds(&bounds).is_ok());
        for value in [0, 1, 60, 100, 32_000, 200_000, 400_000] {
            assert_eq!(
                check_daemon_setting_proposal(&bounds, "worker_context_cap_tokens", value),
                Ok(()),
                "{value}"
            );
        }
        for value in [101, 31_999, 400_001] {
            assert_eq!(
                check_daemon_setting_proposal(&bounds, "worker_context_cap_tokens", value),
                Err(DAEMON_SETTING_OUT_OF_BOUNDS),
                "{value}"
            );
        }
    }

    #[test]
    fn operator_bounds_validate_inside_the_hard_range() {
        assert!(validate_daemon_setting_bounds(&[]).is_ok());
        assert!(
            validate_daemon_setting_bounds(&[bound("sandbox_max_source_roots", 1, 65_536)]).is_ok()
        );
        for bad in [
            bound("sandbox_max_source_roots", 0, 10),
            bound("sandbox_max_source_roots", 10, 65_537),
            bound("sandbox_max_source_roots", 20, 10),
            bound("max_spend_usd", 1, 2),
        ] {
            assert!(validate_daemon_setting_bounds(&[bad]).is_err());
        }
        let dup = bound("sandbox_min_free_gib", 1, 2);
        assert!(validate_daemon_setting_bounds(&[dup.clone(), dup]).is_err());
    }

    #[test]
    fn proposal_params_need_a_reason_and_an_allowlisted_key() {
        let params = |key: &str, reason: &str| ProposeDaemonSettingParamsV1 {
            key: key.into(),
            value: 1,
            reason: reason.into(),
        };
        assert!(
            params("sandbox_min_free_gib", "disk recovered")
                .validate()
                .is_ok()
        );
        assert!(params("sandbox_min_free_gib", "  ").validate().is_err());
        assert!(params("", "raise").validate().is_err());
    }
}
