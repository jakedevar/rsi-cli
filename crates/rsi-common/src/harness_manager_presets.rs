//! Pure operator policy templates. Preset labels are derived, never authority or wire data.
use std::collections::BTreeSet;

use crate::harness_manager::HarnessManagerScopeModeV1;
use crate::harness_manager_v2::{ManagerCapabilityV2, ManagerOperatingModeV2, ManagerPolicyV2};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagerPolicyPreset {
    Observe,
    Execute,
    FullProjectControl,
    Custom,
}
impl ManagerPolicyPreset {
    pub fn label(self) -> &'static str {
        match self {
            Self::Observe => "Observe",
            Self::Execute => "Execute",
            Self::FullProjectControl => "Full project control",
            Self::Custom => "Custom",
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagerPolicyOrigin {
    New,
    Saved,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ManagerPolicyField {
    Mode,
    Capabilities,
    AllowCreateGroups,
    GroupIds,
    Paused,
    PausedEpicIds,
    CreatedContainers,
    CreatedSessions,
    ActiveSessions,
    ProviderLimits,
    AllowedLaunches,
    RecoveryAttempts,
    RetryDelaySeconds,
    RequestTimeoutSeconds,
    MaxSpendUsd,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ManagerAllowanceField {
    CreatedContainers,
    CreatedSessions,
    RecoveryAttempts,
}
impl ManagerAllowanceField {
    pub fn policy_field(self) -> ManagerPolicyField {
        match self {
            Self::CreatedContainers => ManagerPolicyField::CreatedContainers,
            Self::CreatedSessions => ManagerPolicyField::CreatedSessions,
            Self::RecoveryAttempts => ManagerPolicyField::RecoveryAttempts,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::CreatedContainers => "Container creation",
            Self::CreatedSessions => "Session creation / self-succession",
            Self::RecoveryAttempts => "Automatic recovery",
        }
    }
    fn value(self, policy: &ManagerPolicyV2) -> u16 {
        match self {
            Self::CreatedContainers => policy.max_created_containers,
            Self::CreatedSessions => policy.max_created_sessions,
            Self::RecoveryAttempts => policy.max_recovery_attempts,
        }
    }
    fn suggested(self) -> u16 {
        match self {
            Self::CreatedContainers => 8,
            Self::CreatedSessions => 32,
            Self::RecoveryAttempts => 3,
        }
    }
    fn set(self, policy: &mut ManagerPolicyV2, value: u16) {
        match self {
            Self::CreatedContainers => policy.max_created_containers = value,
            Self::CreatedSessions => policy.max_created_sessions = value,
            Self::RecoveryAttempts => policy.max_recovery_attempts = value,
        }
    }
}
#[derive(Debug)]
pub struct ManagerPresetContext<'a> {
    pub origin: ManagerPolicyOrigin,
    pub touched: &'a BTreeSet<ManagerPolicyField>,
    pub scope_mode: HarnessManagerScopeModeV1,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagerPresetClassification {
    pub preset: ManagerPolicyPreset,
    pub permission_profile: Option<ManagerPolicyPreset>,
    pub zero_conflicts: Vec<ManagerAllowanceField>,
    pub root_group_permission_mismatch: bool,
    pub validation_error: Option<&'static str>,
}
#[derive(Debug, Clone, PartialEq)]
pub struct ManagerPresetEdit {
    pub policy: ManagerPolicyV2,
    pub changed_fields: Vec<ManagerPolicyField>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagerAllowanceSuggestion {
    pub field: ManagerAllowanceField,
    pub from: u16,
    pub to: u16,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagerPresetEditError {
    NotCurrentlySuggested(ManagerAllowanceField),
}

const FULL: &[ManagerCapabilityV2] = &[
    ManagerCapabilityV2::WorkPlan,
    ManagerCapabilityV2::LeadControl,
    ManagerCapabilityV2::Topology,
    ManagerCapabilityV2::SessionCreate,
    ManagerCapabilityV2::LeadAssign,
    ManagerCapabilityV2::Integration,
    ManagerCapabilityV2::SelfSuccession,
    ManagerCapabilityV2::SessionControl,
    ManagerCapabilityV2::IssueCoordinate,
    // K14 (#672): the operator's "full access policy". A policy saved before
    // this grant existed keeps its exact grants and classifies as Custom until
    // the operator re-applies Full and saves; it is never widened silently.
    ManagerCapabilityV2::OperatorDelegation,
];
const EXECUTE: &[ManagerCapabilityV2] = &[
    ManagerCapabilityV2::WorkPlan,
    ManagerCapabilityV2::LeadControl,
    ManagerCapabilityV2::SessionCreate,
    ManagerCapabilityV2::LeadAssign,
    ManagerCapabilityV2::Integration,
    ManagerCapabilityV2::SelfSuccession,
    ManagerCapabilityV2::SessionControl,
    ManagerCapabilityV2::IssueCoordinate,
];
const ALLOWANCES: &[ManagerAllowanceField] = &[
    ManagerAllowanceField::CreatedContainers,
    ManagerAllowanceField::CreatedSessions,
    ManagerAllowanceField::RecoveryAttempts,
];
fn grants(preset: ManagerPolicyPreset) -> &'static [ManagerCapabilityV2] {
    match preset {
        ManagerPolicyPreset::FullProjectControl => FULL,
        ManagerPolicyPreset::Execute => EXECUTE,
        _ => &[],
    }
}
fn same_grants(actual: &[ManagerCapabilityV2], expected: &[ManagerCapabilityV2]) -> bool {
    actual.len() == expected.len() && expected.iter().all(|c| actual.contains(c))
}
fn permission_profile(policy: &ManagerPolicyV2) -> Option<ManagerPolicyPreset> {
    match policy.mode {
        ManagerOperatingModeV2::Monitor if policy.capabilities.is_empty() => {
            Some(ManagerPolicyPreset::Observe)
        }
        ManagerOperatingModeV2::Execute if same_grants(&policy.capabilities, FULL) => {
            Some(ManagerPolicyPreset::FullProjectControl)
        }
        ManagerOperatingModeV2::Execute if same_grants(&policy.capabilities, EXECUTE) => {
            Some(ManagerPolicyPreset::Execute)
        }
        _ => None,
    }
}

pub fn classify_manager_policy(
    policy: &ManagerPolicyV2,
    scope_mode: HarnessManagerScopeModeV1,
) -> ManagerPresetClassification {
    let permission_profile = permission_profile(policy);
    let zero_conflicts: Vec<_> = permission_profile
        .map(|profile| {
            suggested_manager_allowances(policy, profile)
                .into_iter()
                .map(|s| s.field)
                .collect()
        })
        .unwrap_or_default();
    let root_group_permission_mismatch = permission_profile.is_some_and(|profile| {
        let expected = profile == ManagerPolicyPreset::FullProjectControl
            && scope_mode == HarnessManagerScopeModeV1::Project;
        policy.allow_create_groups != expected
    });
    let validation_error = policy.validate().err();
    let preset = if zero_conflicts.is_empty()
        && !root_group_permission_mismatch
        && validation_error.is_none()
    {
        permission_profile.unwrap_or(ManagerPolicyPreset::Custom)
    } else {
        ManagerPolicyPreset::Custom
    };
    ManagerPresetClassification {
        preset,
        permission_profile,
        zero_conflicts,
        root_group_permission_mismatch,
        validation_error,
    }
}

pub fn apply_manager_policy_preset(
    policy: &ManagerPolicyV2,
    preset: ManagerPolicyPreset,
    context: &ManagerPresetContext<'_>,
) -> ManagerPresetEdit {
    let mut draft = policy.clone();
    let mut changed_fields = Vec::new();
    if preset == ManagerPolicyPreset::Custom {
        return ManagerPresetEdit {
            policy: draft,
            changed_fields,
        };
    }
    draft.mode = if preset == ManagerPolicyPreset::Observe {
        ManagerOperatingModeV2::Monitor
    } else {
        ManagerOperatingModeV2::Execute
    };
    if draft.mode != policy.mode {
        changed_fields.push(ManagerPolicyField::Mode);
    }
    if !same_grants(&draft.capabilities, grants(preset)) {
        draft.capabilities = grants(preset).to_vec();
        changed_fields.push(ManagerPolicyField::Capabilities);
    }
    // Selected scope must not newly enable this bit, but a previously explicit
    // opt-in is retained. Presets are not a new authoritative scope restriction.
    draft.allow_create_groups = match preset {
        ManagerPolicyPreset::FullProjectControl => {
            context.scope_mode == HarnessManagerScopeModeV1::Project || policy.allow_create_groups
        }
        _ => false,
    };
    if draft.allow_create_groups != policy.allow_create_groups {
        changed_fields.push(ManagerPolicyField::AllowCreateGroups);
    }
    if context.origin == ManagerPolicyOrigin::New {
        for field in ALLOWANCES {
            if context.touched.contains(&field.policy_field()) {
                continue;
            }
            let value = if preset == ManagerPolicyPreset::Observe {
                0
            } else {
                field.suggested()
            };
            if field.value(&draft) != value {
                field.set(&mut draft, value);
                changed_fields.push(field.policy_field());
            }
        }
    }
    ManagerPresetEdit {
        policy: draft,
        changed_fields,
    }
}

pub fn suggested_manager_allowances(
    policy: &ManagerPolicyV2,
    profile: ManagerPolicyPreset,
) -> Vec<ManagerAllowanceSuggestion> {
    if permission_profile(policy) != Some(profile)
        || !matches!(
            profile,
            ManagerPolicyPreset::Execute | ManagerPolicyPreset::FullProjectControl
        )
    {
        return Vec::new();
    }
    ALLOWANCES
        .iter()
        .copied()
        .filter(|field| {
            (profile == ManagerPolicyPreset::FullProjectControl
                || *field != ManagerAllowanceField::CreatedContainers)
                && field.value(policy) == 0
        })
        .map(|field| ManagerAllowanceSuggestion {
            field,
            from: 0,
            to: field.suggested(),
        })
        .collect()
}

/// Rechecks the current draft before applying any field; stale lists are atomic errors.
///
/// # Errors
/// Returns the first duplicate or no-longer-suggested field without changing policy.
pub fn apply_suggested_manager_allowances(
    policy: &ManagerPolicyV2,
    profile: ManagerPolicyPreset,
    fields: &[ManagerAllowanceField],
) -> Result<ManagerPresetEdit, ManagerPresetEditError> {
    let suggestions = suggested_manager_allowances(policy, profile);
    let mut seen = BTreeSet::new();
    for field in fields {
        if !seen.insert(*field) || !suggestions.iter().any(|s| s.field == *field) {
            return Err(ManagerPresetEditError::NotCurrentlySuggested(*field));
        }
    }
    let mut draft = policy.clone();
    for field in fields {
        field.set(&mut draft, field.suggested());
    }
    Ok(ManagerPresetEdit {
        policy: draft,
        changed_fields: fields.iter().map(|f| f.policy_field()).collect(),
    })
}

#[cfg(test)]
mod tests;
