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
    /// #1977: built-in bounds that apply while the operator has set none for
    /// this key; an explicit operator bound replaces them (and may narrow).
    pub default_bounds: Option<(u64, u64)>,
}

/// The closed allowlist. Order is the TUI row order.
pub const MANAGER_ADJUSTABLE_DAEMON_SETTINGS: &[AdjustableDaemonSetting] = &[
    AdjustableDaemonSetting {
        key: "sandbox_max_source_roots",
        label: "Maximum sandbox roots",
        hard_min: 1,
        hard_max: 65_536,
        excluded_between: None,
        default_bounds: None,
    },
    AdjustableDaemonSetting {
        key: "sandbox_min_free_gib",
        label: "Minimum free space (GiB)",
        hard_min: 0,
        hard_max: 1024,
        excluded_between: None,
        default_bounds: None,
    },
    AdjustableDaemonSetting {
        key: "sandbox_build_cache_reclaim_high_watermark_pct",
        label: "Cache pressure high",
        hard_min: 2,
        hard_max: 99,
        excluded_between: None,
        default_bounds: None,
    },
    AdjustableDaemonSetting {
        key: "sandbox_build_cache_reclaim_low_watermark_pct",
        label: "Cache pressure low",
        hard_min: 1,
        hard_max: 98,
        excluded_between: None,
        default_bounds: None,
    },
    // #1254: `0` off, `1..=100` percent of the context window, or tokens in
    // `32_000..=2_000_000`; the daemon applies it with the same validation.
    AdjustableDaemonSetting {
        key: "worker_context_cap_tokens",
        label: "Worker context cap (0 off, 1-100 = % of window)",
        hard_min: 0,
        hard_max: 2_000_000,
        excluded_between: Some((100, 32_000)),
        default_bounds: None,
    },
    // #1777: deploy restarts per rolling hour; the ceiling is the
    // supervisor's own restart cap.
    AdjustableDaemonSetting {
        key: "deploy_restarts_per_hour",
        label: "Deploy restarts per hour",
        hard_min: 1,
        hard_max: 3,
        excluded_between: None,
        default_bounds: None,
    },
    // #1977: the operator's host-load guard and the merge-queue batch width.
    // Safe built-in bounds apply until the operator narrows them.
    AdjustableDaemonSetting {
        key: "host_load_admission_threshold",
        label: "Host load admission threshold",
        hard_min: 0,
        hard_max: 1024,
        excluded_between: None,
        default_bounds: Some((8, 64)),
    },
    AdjustableDaemonSetting {
        key: "rolling_queue_batch_size",
        label: "Rolling queue batch size",
        hard_min: 1,
        hard_max: 8,
        excluded_between: None,
        default_bounds: Some((1, 8)),
    },
    // #2123: the resource-governor concurrency limits and the full-QA cadence.
    // Managers can lower build/lander concurrency (or raise it back) and space
    // out full QA runs, e.g. to keep the operator's desktop cool; the built-in
    // bounds stop short of the daemon's own ceilings.
    AdjustableDaemonSetting {
        key: "governor_build_slots",
        label: "Build slots",
        hard_min: 1,
        hard_max: 16,
        excluded_between: None,
        default_bounds: Some((1, 4)),
    },
    AdjustableDaemonSetting {
        key: "governor_lander_slots",
        label: "Lander slots",
        hard_min: 1,
        hard_max: 16,
        excluded_between: None,
        default_bounds: Some((1, 5)),
    },
    // #186: the Cargo governor's free-disk floor (GiB; the daemon accepts
    // 0..=4096, 0 disables the gate). Raise-only by default: the built-in min is
    // the registry default (30), so a manager cannot lower the floor and risk
    // filling the disk unless the operator's explicit bound allows it; the max
    // keeps a mistaken raise from holding every build and lander.
    AdjustableDaemonSetting {
        key: "governor_min_free_disk_gb",
        label: "Governor min free disk (GiB)",
        hard_min: 0,
        hard_max: 4096,
        excluded_between: None,
        default_bounds: Some((30, 100)),
    },
    AdjustableDaemonSetting {
        key: "rolling_queue_full_qa_interval_mins",
        label: "Merge queue full-QA interval (min)",
        hard_min: 0,
        hard_max: 240,
        excluded_between: None,
        default_bounds: Some((30, 240)),
    },
    // #2119: hub build CPU budget (percent of logical CPUs). Lower-only by
    // default: the built-in max is the registry default; an explicit operator
    // bound may widen it.
    AdjustableDaemonSetting {
        key: "hub_build_cpu_percent",
        label: "Hub build CPU budget (%)",
        hard_min: 10,
        hard_max: 100,
        excluded_between: None,
        default_bounds: Some((10, 50)),
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
    /// #1997: the explicit "disable" entry for a key that has built-in default
    /// bounds. `0-0` lies outside every such key's hard range, so it cannot be
    /// a real bound; it makes the key not adjustable instead of defaulted.
    #[must_use]
    pub fn disabled(key: &str) -> Self {
        Self {
            key: key.into(),
            min: 0,
            max: 0,
        }
    }

    /// Whether this entry is the explicit "not adjustable" marker.
    #[must_use]
    pub fn is_disabled(&self) -> bool {
        self.min == 0 && self.max == 0
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        let setting = adjustable_daemon_setting(&self.key).ok_or(DAEMON_SETTING_NOT_ALLOWLISTED)?;
        if self.is_disabled() && setting.default_bounds.is_some() && setting.hard_min > 0 {
            return Ok(());
        }
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
    let (min, max) = match bounds.iter().find(|bound| bound.key == key) {
        Some(bound) if bound.is_disabled() => return Err(DAEMON_SETTING_NOT_ADJUSTABLE),
        Some(bound) => (bound.min, bound.max),
        None => setting
            .default_bounds
            .ok_or(DAEMON_SETTING_NOT_ADJUSTABLE)?,
    };
    if value < min || value > max || !setting.accepts(value) {
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

    /// #1997: `0-0` disables a defaulted key; it is invalid for other keys.
    #[test]
    fn disabled_entry_turns_a_defaulted_key_off() {
        let key = "host_load_admission_threshold";
        let off = vec![ManagerDaemonSettingBoundV2::disabled(key)];
        assert!(validate_daemon_setting_bounds(&off).is_ok());
        assert_eq!(
            check_daemon_setting_proposal(&off, key, 16),
            Err(DAEMON_SETTING_NOT_ADJUSTABLE)
        );
        assert!(check_daemon_setting_proposal(&[], key, 16).is_ok());
        assert!(
            validate_daemon_setting_bounds(&[ManagerDaemonSettingBoundV2::disabled(
                "sandbox_max_source_roots"
            )])
            .is_err()
        );
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

    /// #3141: the API route settings stay operator-only. The explicit
    /// `api_route.openrouter = codex_cli` opt-in (global and per-model), the
    /// Bedrock route and every fallback key (including the legacy alias) are
    /// outside the manager allowlist, so a `ProposeDaemonSetting` for one is
    /// refused as not allowlisted whatever bounds the operator set.
    #[test]
    fn api_route_settings_are_operator_only() {
        let prefixed: Vec<&str> = MANAGER_ADJUSTABLE_DAEMON_SETTINGS
            .iter()
            .map(|setting| setting.key)
            .filter(|key| key.starts_with("api_route."))
            .collect();
        assert_eq!(prefixed, Vec::<&str>::new());
        let bounds = [
            bound("api_route.openrouter", 0, 1),
            bound("api_route.fallback.openrouter", 0, 1),
        ];
        for key in [
            "api_route.openrouter",
            "api_route.openrouter.qwen/qwen3-coder-next",
            "api_route.bedrock",
            "api_route.fallback",
            "api_route.fallback.openrouter",
            "api_route.fallback.bedrock",
        ] {
            assert!(adjustable_daemon_setting(key).is_none(), "{key}");
            assert_eq!(
                check_daemon_setting_proposal(&bounds, key, 1),
                Err(DAEMON_SETTING_NOT_ALLOWLISTED),
                "{key}"
            );
        }
        for entry in bounds {
            assert_eq!(entry.validate(), Err(DAEMON_SETTING_NOT_ALLOWLISTED));
        }
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

    /// #1977: keys with built-in default bounds are adjustable until the
    /// operator narrows them; an explicit bound replaces the default.
    #[test]
    fn default_bounds_apply_until_the_operator_narrows_them() {
        let k = "host_load_admission_threshold";
        assert!(check_daemon_setting_proposal(&[], k, 24).is_ok());
        for v in [7, 65] {
            assert_eq!(
                check_daemon_setting_proposal(&[], k, v),
                Err(DAEMON_SETTING_OUT_OF_BOUNDS)
            );
        }
        let q = "rolling_queue_batch_size";
        assert!(check_daemon_setting_proposal(&[], q, 8).is_ok());
        assert!(check_daemon_setting_proposal(&[], q, 0).is_err());
        assert!(check_daemon_setting_proposal(&[], q, 9).is_err());
        let narrow = vec![bound(k, 16, 32)];
        assert!(check_daemon_setting_proposal(&narrow, k, 24).is_ok());
        assert!(check_daemon_setting_proposal(&narrow, k, 40).is_err());
    }

    /// #2123: governor slots and the full-QA interval are manager-adjustable
    /// inside built-in bounds; the operator may narrow or replace them.
    #[test]
    fn governor_slots_and_qa_interval_apply_inside_default_bounds() {
        for (key, ok, bad) in [
            ("governor_build_slots", [1, 4], [0, 5]),
            ("hub_build_cpu_percent", [10, 50], [9, 51]),
            ("governor_lander_slots", [1, 5], [0, 6]),
            ("governor_min_free_disk_gb", [30, 100], [29, 101]),
            ("rolling_queue_full_qa_interval_mins", [30, 240], [29, 241]),
        ] {
            for v in ok {
                assert_eq!(
                    check_daemon_setting_proposal(&[], key, v),
                    Ok(()),
                    "{key} {v}"
                );
            }
            for v in bad {
                assert_eq!(
                    check_daemon_setting_proposal(&[], key, v),
                    Err(DAEMON_SETTING_OUT_OF_BOUNDS),
                    "{key} {v}"
                );
            }
        }
        // #186: the floor can only be raised by default; an operator bound may
        // lower it (down to the daemon's own hard range, 0 disables the gate).
        let floor = "governor_min_free_disk_gb";
        let lowered = vec![bound(floor, 10, 100)];
        assert!(validate_daemon_setting_bounds(&lowered).is_ok());
        assert!(check_daemon_setting_proposal(&lowered, floor, 10).is_ok());
        assert!(check_daemon_setting_proposal(&[], floor, 10).is_err());
        let widened = vec![bound("governor_build_slots", 1, 16)];
        assert!(validate_daemon_setting_bounds(&widened).is_ok());
        assert!(check_daemon_setting_proposal(&widened, "governor_build_slots", 12).is_ok());
        assert!(
            validate_daemon_setting_bounds(&[ManagerDaemonSettingBoundV2::disabled(
                "governor_lander_slots"
            )])
            .is_ok()
        );
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
