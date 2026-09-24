use super::{
    catalog::{
        self, CatalogStatus, ManagerCatalogRequest, ManagerCatalogResult, ManagerLaunchPickerState,
        PickerOutcome,
    },
    label_for,
};
use rsi_common::harness_manager_presets::*;
use std::{
    collections::{BTreeSet, HashMap},
    path::PathBuf,
};
mod preview;
pub mod usage;
use crate::{app::App, types::OverlayState};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rsi_common::{
    harness_manager::HarnessManagerConfigV1,
    harness_manager_v2::*,
    types::{SessionKind, SessionProvider},
};
use uuid::Uuid;

pub const PROVIDERS: [SessionProvider; 9] = [
    SessionProvider::Claude,
    SessionProvider::Codex,
    SessionProvider::Pioneer,
    SessionProvider::OpenRouter,
    SessionProvider::Bedrock,
    SessionProvider::Local,
    SessionProvider::Antigravity,
    SessionProvider::CodexAppServer,
    SessionProvider::Harness,
];
const CAPABILITIES: [ManagerCapabilityV2; 11] = [
    ManagerCapabilityV2::WorkPlan,
    ManagerCapabilityV2::LeadControl,
    ManagerCapabilityV2::Topology,
    ManagerCapabilityV2::SessionCreate,
    ManagerCapabilityV2::LeadAssign,
    ManagerCapabilityV2::Integration,
    ManagerCapabilityV2::SelfSuccession,
    ManagerCapabilityV2::GitEffect,
    ManagerCapabilityV2::SessionControl,
    ManagerCapabilityV2::IssueCoordinate,
    ManagerCapabilityV2::OperatorDelegation,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Preset(ManagerPolicyPreset),
    Advanced,
    Preview,
    Detail(usize),
    Suggestions,
    EditLaunch(usize),
    AddRawLaunch,
    Mode,
    Paused,
    Capability(usize),
    Epic(Uuid),
    AddPausedEpic,
    Group(Uuid),
    AddGroup,
    CreateGroups,
    Containers,
    Sessions,
    Concurrency,
    ProviderLimit(usize),
    LaunchProvider(usize),
    LaunchModel(usize),
    LaunchEffort(usize),
    RemoveLaunch(usize),
    AddLaunch,
    Retries,
    RetryDelay,
    Deadline,
    Spend,
}
pub struct PolicyState {
    pub config: HarnessManagerConfigV1,
    pub identity: String,
    pub policy_version: i64,
    pub saved: Option<HarnessManagerPolicyConfigV2>,
    pub opened_draft: ManagerPolicyV2,
    pub draft: ManagerPolicyV2,
    pub touched: BTreeSet<ManagerPolicyField>,
    pub advanced_open: bool,
    pub preview_open: bool,
    pub picker: Option<ManagerLaunchPickerState>,
    pub catalog_request: Option<ManagerCatalogRequest>,
    pub catalog_generation: u64,
    pub catalog_status: HashMap<SessionProvider, CatalogStatus>,
    /// Created-session and active usage (#674), loaded in the background.
    pub usage: usage::UsageView,
    pub usage_request: Option<usage::UsageRequest>,
    /// Quota saved or reloaded after the usage snapshot was taken; the header
    /// shows it until the next snapshot reports the daemon's limit (K15B-1).
    pub usage_quota: Option<u16>,
    /// Bumped on save and same-scope reload; usage snapshots from requests
    /// started under an older generation are dropped (K15B-1-RACE).
    pub usage_generation: u64,
    /// Socket of the last usage request, for re-requesting after a drop.
    pub usage_socket: Option<PathBuf>,
    pub choice_labels: HashMap<(SessionProvider, String), String>,
    pub custom_provider_names: Vec<(Uuid, String)>,
    pub epics: Vec<(Uuid, String)>,
    pub groups: Vec<(Uuid, String)>,
    pub selected: usize,
    pub edit: Option<(Field, String)>,
    pub error: Option<String>,
    pub notice: String,
    pub idempotency_key: String,
}
/// Titled editor sections, in display order.
///
/// The most-edited rows (presets, budgets, launch choices) come first so they
/// are visible on open. Headers are rendered between rows; they are not
/// selectable rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Presets,
    Budgets,
    Launches,
    Authority,
    Scope,
    Recovery,
    Preview,
}
impl Section {
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Presets => "Authority · presets",
            Self::Budgets => "Budgets",
            Self::Authority => "Authority · mode and grants",
            Self::Scope => "Scope",
            Self::Recovery => "Recovery",
            Self::Launches => "Launches",
            Self::Preview => "Effective preview · read-only",
        }
    }
}
/// How Enter/Space acts on a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// Enter opens a text edit.
    Edit,
    /// Enter flips or cycles the value.
    Toggle,
    /// Enter runs an action (preset, picker, remove, expand).
    Action,
    /// Informational; Enter does nothing.
    ReadOnly,
}
pub struct PolicyRow {
    pub field: Field,
    pub label: String,
    pub value: String,
    pub section: Section,
    pub kind: RowKind,
    /// One-line description shown for the selected row.
    pub description: String,
    /// The draft value differs from the saved (opened) policy.
    pub modified: bool,
    /// The saved (opened) value of a modified scalar field, for display.
    pub saved: Option<String>,
    /// Client-side validation error for the draft value (mirrors
    /// `ManagerPolicyV2::validate`).
    pub error: Option<String>,
}

/// Inclusive numeric bounds mirrored from `ManagerPolicyV2::validate`
/// (pinned by `policy_field_bounds_match_daemon_validation`).
#[must_use]
pub const fn numeric_bounds(field: Field) -> Option<(u32, u32)> {
    Some(match field {
        Field::Sessions => (0, 1024),
        Field::Containers => (0, 64),
        Field::Concurrency => (1, 100),
        Field::ProviderLimit(_) => (1, 64),
        Field::Retries => (0, 32),
        Field::RetryDelay => (1, 86_400),
        Field::Deadline => (30, 604_800),
        _ => return None,
    })
}
const MAX_PAUSED_EPICS: usize = 32;
/// Daemon bound on provider limit entries (`ManagerPolicyV2::validate`).
const MAX_PROVIDER_LIMITS: usize = 8;
const MAX_LAUNCHES: usize = 32;

/// Display value of a scalar policy field (the saved side of a marker).
fn scalar_value(p: &ManagerPolicyV2, field: Field) -> Option<String> {
    Some(match field {
        Field::Sessions => p.max_created_sessions.to_string(),
        Field::Containers => p.max_created_containers.to_string(),
        Field::Concurrency => p.max_active_sessions.to_string(),
        Field::ProviderLimit(i) => p
            .provider_limits
            .iter()
            .find(|l| l.provider == PROVIDERS[i])
            .map_or_else(|| "inherit".into(), |l| l.max_active.to_string()),
        Field::Spend => p
            .max_spend_usd
            .map_or_else(|| "uncapped".into(), |v| v.to_string()),
        Field::Mode => format!("{:?}", p.mode),
        Field::Paused => p.paused.to_string(),
        Field::Capability(i) => p.capabilities.contains(&CAPABILITIES[i]).to_string(),
        Field::Epic(id) => p.paused_epic_ids.contains(&id).to_string(),
        Field::Group(id) => p.group_ids.contains(&id).to_string(),
        Field::CreateGroups => p.allow_create_groups.to_string(),
        Field::Retries => p.max_recovery_attempts.to_string(),
        Field::RetryDelay => p.retry_delay_seconds.to_string(),
        Field::Deadline => p.request_timeout_seconds.to_string(),
        _ => return None,
    })
}

const fn capability_description(capability: ManagerCapabilityV2) -> &'static str {
    match capability {
        ManagerCapabilityV2::WorkPlan => {
            "Record work, stages, evidence and request DB-native reviews"
        }
        ManagerCapabilityV2::LeadControl => "Resume, pause, retry and replace Epic leads",
        ManagerCapabilityV2::Topology => "Create and manage Groups and Epics under granted parents",
        ManagerCapabilityV2::SessionCreate => "Create worker sessions (consumes the session quota)",
        ManagerCapabilityV2::LeadAssign => "Assign or unassign an Epic's lead",
        ManagerCapabilityV2::Integration => "Record independently checked integration evidence",
        ManagerCapabilityV2::SelfSuccession => "Reserve its own successor with a committed handoff",
        ManagerCapabilityV2::GitEffect => {
            "Advance integration target refs through the merge engine"
        }
        ManagerCapabilityV2::SessionControl => {
            "Halt, continue and message scoped leads and descendants"
        }
        ManagerCapabilityV2::IssueCoordinate => "Read project Issues through the guarded controls",
        ManagerCapabilityV2::OperatorDelegation => {
            "Call allowlisted operator methods (logical archive, restore, session list) in the project"
        }
    }
}

/// Row label after "Grant ". Existing grants keep their wire names; K14's
/// grant reads as the operator-facing phrase "Operator delegation".
fn capability_label(capability: ManagerCapabilityV2) -> String {
    match capability {
        ManagerCapabilityV2::OperatorDelegation => "Operator delegation".into(),
        other => format!("{other:?}"),
    }
}

impl PolicyState {
    pub fn rows(&self) -> Vec<PolicyRow> {
        let mut rows = vec![];
        let mut section = Section::Presets;
        let d = &self.draft;
        let mut push =
            |section: Section, field: Field, label: &str, value: String, description: &str| {
                let kind = match field {
                    Field::Detail(_) => RowKind::ReadOnly,
                    Field::Mode
                    | Field::Paused
                    | Field::Capability(_)
                    | Field::Epic(_)
                    | Field::Group(_)
                    | Field::CreateGroups
                    | Field::LaunchProvider(_) => RowKind::Toggle,
                    Field::Preset(_)
                    | Field::Suggestions
                    | Field::Advanced
                    | Field::Preview
                    | Field::EditLaunch(_)
                    | Field::RemoveLaunch(_)
                    | Field::AddLaunch
                    | Field::AddRawLaunch => RowKind::Action,
                    _ => RowKind::Edit,
                };
                rows.push(PolicyRow {
                    field,
                    label: label.into(),
                    value,
                    section,
                    kind,
                    description: description.into(),
                    modified: self.modified(field),
                    saved: self
                        .modified(field)
                        .then(|| scalar_value(&self.opened_draft, field))
                        .flatten(),
                    error: self.field_error(field),
                });
            };

        for preset in [
            ManagerPolicyPreset::Observe,
            ManagerPolicyPreset::Execute,
            ManagerPolicyPreset::FullProjectControl,
            ManagerPolicyPreset::Custom,
        ] {
            push(
                section,
                Field::Preset(preset),
                preset.label(),
                if preset == ManagerPolicyPreset::Custom {
                    "Keep exact draft; show exact launch tuples".into()
                } else {
                    "Apply permissions to draft; preserve explicit limits".into()
                },
                if preset == ManagerPolicyPreset::Custom {
                    "Keep the draft as edited and show the raw launch tuple rows"
                } else {
                    "Replace mode and grants with this preset; explicit limits are kept"
                },
            );
        }
        let suggestions = self.suggestions();
        if !suggestions.is_empty() {
            push(
                section,
                Field::Suggestions,
                "Use suggested allowances",
                suggestions
                    .iter()
                    .map(|s| format!("{} {} → {}", s.field.label(), s.from, s.to))
                    .collect::<Vec<_>>()
                    .join("; "),
                "Raise the zero limits that block the chosen permissions",
            );
        }
        section = Section::Budgets;
        push(
            section,
            Field::Sessions,
            "Created session quota",
            d.max_created_sessions.to_string(),
            "Lifetime sessions the manager may create in this scope; DB-native reviews are not counted",
        );
        push(
            section,
            Field::Containers,
            "Created container quota",
            d.max_created_containers.to_string(),
            "Lifetime Groups/Epics the manager may create in this scope",
        );
        push(
            section,
            Field::Concurrency,
            "Active session limit",
            d.max_active_sessions.to_string(),
            "Sessions the manager's cohort may run at once",
        );
        push(
            section,
            Field::Spend,
            "Spend cap (USD)",
            d.max_spend_usd
                .map_or_else(|| "uncapped".into(), |v| v.to_string()),
            "Manager USD spend cap; blank removes the cap",
        );

        for (i, p) in PROVIDERS.iter().enumerate() {
            push(
                section,
                Field::ProviderLimit(i),
                &format!("{p:?} active limit"),
                d.provider_limits
                    .iter()
                    .find(|l| l.provider == *p)
                    .map_or_else(|| "inherit".into(), |l| l.max_active.to_string()),
                "Per-provider active ceiling; blank inherits the active session limit",
            );
        }
        section = Section::Launches;
        for (i, choice) in d.allowed_launches.iter().enumerate() {
            push(
                section,
                Field::EditLaunch(i),
                &format!("Launch {} · {:?}/{}", i + 1, choice.provider, choice.model),
                format!(
                    "{} · {} · Enter catalog",
                    choice
                        .effort
                        .as_deref()
                        .unwrap_or("Default (no explicit effort)"),
                    self.choice_status(choice)
                ),
                "Enter opens the provider/model catalog for this choice",
            );
            push(
                section,
                Field::RemoveLaunch(i),
                &format!("Remove launch {}", i + 1),
                if d.allowed_launches.len() == 1 {
                    "Enter remove → Any valid choice"
                } else {
                    "Enter remove"
                }
                .into(),
                "Remove this launch restriction",
            );
        }
        push(
            section,
            Field::AddLaunch,
            "Allowed models",
            if d.allowed_launches.is_empty() {
                "Any provider/model/effort (valid choices, including future models) · Enter to restrict".into()
            } else {
                format!(
                    "Restricted to {} exact choices · Enter catalog",
                    d.allowed_launches.len()
                )
            },
            "Restrict launches to exact catalog choices; an empty list allows any valid choice",
        );
        push(
            section,
            Field::Advanced,
            "Exact launch tuples",
            if self.advanced_open {
                "Expanded · Enter collapse"
            } else {
                "Enter expand raw provider/model/effort rows"
            }
            .into(),
            "Show or hide the raw provider/model/effort rows",
        );
        if self.advanced_open {
            for (i, choice) in d.allowed_launches.iter().enumerate() {
                push(
                    section,
                    Field::LaunchProvider(i),
                    &format!("Launch {} provider", i + 1),
                    format!("{:?}", choice.provider),
                    "Enter cycles the provider",
                );
                push(
                    section,
                    Field::LaunchModel(i),
                    &format!("Launch {} model", i + 1),
                    choice.model.clone(),
                    "Exact model id; daemon admission validates the actual launch",
                );
                push(
                    section,
                    Field::LaunchEffort(i),
                    &format!("Launch {} effort", i + 1),
                    choice.effort.clone().unwrap_or_else(|| "default".into()),
                    "Exact effort; blank means the provider default",
                );
            }
            push(
                section,
                Field::AddRawLaunch,
                "Advanced: add exact tuple manually",
                "Enter add; daemon admission validates actual launch".into(),
                "Add an exact provider/model/effort tuple without the catalog",
            );
        }

        section = Section::Authority;
        push(
            section,
            Field::Mode,
            "Operating mode",
            format!("{:?}", d.mode),
            "Status reports only · Monitor watches · Execute may act (Enter cycles)",
        );
        push(
            section,
            Field::Paused,
            "Operator pause",
            d.paused.to_string(),
            "Stop every manager action until the operator clears the pause",
        );
        for (i, c) in CAPABILITIES.iter().enumerate() {
            push(
                section,
                Field::Capability(i),
                &format!("Grant {}", capability_label(*c)),
                d.capabilities.contains(c).to_string(),
                capability_description(*c),
            );
        }

        section = Section::Scope;
        for (id, name) in &self.epics {
            push(
                section,
                Field::Epic(*id),
                &format!("Pause Epic: {name}"),
                d.paused_epic_ids.contains(id).to_string(),
                "Pause manager actions on this Epic only",
            );
        }
        push(
            section,
            Field::AddPausedEpic,
            "Pause scoped Epic by ID",
            "Enter UUID".into(),
            "Pause an Epic by UUID (at most 32 paused Epics)",
        );
        for (id, name) in &self.groups {
            push(
                section,
                Field::Group(*id),
                &format!("Grant Group: {name}"),
                d.group_ids.contains(id).to_string(),
                "Grant the manager this Group in addition to its scope",
            );
        }
        push(
            section,
            Field::AddGroup,
            "Grant Group by ID",
            "Enter UUID".into(),
            "Grant a Group by UUID from the Topology section of :manager board",
        );
        push(
            section,
            Field::CreateGroups,
            "Allow root Group creation",
            d.allow_create_groups.to_string(),
            "Create new root Groups; requires Grant Topology",
        );

        section = Section::Recovery;
        push(
            section,
            Field::Retries,
            "Recovery attempt limit",
            d.max_recovery_attempts.to_string(),
            "Automatic recovery attempts per Epic; 0 disables automatic recovery",
        );
        push(
            section,
            Field::RetryDelay,
            "Retry delay (seconds)",
            d.retry_delay_seconds.to_string(),
            "Wait before an automatic recovery launches",
        );
        push(
            section,
            Field::Deadline,
            "Request timeout (seconds)",
            d.request_timeout_seconds.to_string(),
            "How long a manager request may stay unanswered before it is overdue",
        );

        section = Section::Preview;
        push(
            section,
            Field::Preview,
            "Effective preview",
            if self.preview_open {
                "Expanded · Enter collapse"
            } else {
                "Enter expand"
            }
            .into(),
            "Show or hide the read-only summary computed from the draft",
        );
        if self.preview_open {
            for (i, (label, value)) in self.preview_rows().into_iter().enumerate() {
                push(
                    section,
                    Field::Detail(i),
                    &label,
                    value,
                    "Read-only: computed from the draft",
                );
            }
        }
        rows
    }
    /// The draft value of `field` differs from the saved (opened) policy.
    #[must_use]
    pub fn modified(&self, field: Field) -> bool {
        let (d, o) = (&self.draft, &self.opened_draft);
        let limit = |p: &ManagerPolicyV2, i: usize| {
            p.provider_limits
                .iter()
                .find(|l| l.provider == PROVIDERS[i])
                .map(|l| l.max_active)
        };
        match field {
            Field::Sessions => d.max_created_sessions != o.max_created_sessions,
            Field::Containers => d.max_created_containers != o.max_created_containers,
            Field::Concurrency => d.max_active_sessions != o.max_active_sessions,
            Field::ProviderLimit(i) => limit(d, i) != limit(o, i),
            Field::Spend => d.max_spend_usd != o.max_spend_usd,
            Field::Mode => d.mode != o.mode,
            Field::Paused => d.paused != o.paused,
            Field::Capability(i) => {
                d.capabilities.contains(&CAPABILITIES[i])
                    != o.capabilities.contains(&CAPABILITIES[i])
            }
            Field::Epic(id) => d.paused_epic_ids.contains(&id) != o.paused_epic_ids.contains(&id),
            Field::Group(id) => d.group_ids.contains(&id) != o.group_ids.contains(&id),
            Field::CreateGroups => d.allow_create_groups != o.allow_create_groups,
            Field::Retries => d.max_recovery_attempts != o.max_recovery_attempts,
            Field::RetryDelay => d.retry_delay_seconds != o.retry_delay_seconds,
            Field::Deadline => d.request_timeout_seconds != o.request_timeout_seconds,
            Field::EditLaunch(i)
            | Field::LaunchProvider(i)
            | Field::LaunchModel(i)
            | Field::LaunchEffort(i) => d.allowed_launches.get(i) != o.allowed_launches.get(i),
            Field::AddLaunch => d.allowed_launches != o.allowed_launches,
            _ => false,
        }
    }
    /// Client-side mirror of `ManagerPolicyV2::validate` for one row.
    #[must_use]
    pub fn field_error(&self, field: Field) -> Option<String> {
        let d = &self.draft;
        let value = match field {
            Field::Sessions => Some(u32::from(d.max_created_sessions)),
            Field::Containers => Some(u32::from(d.max_created_containers)),
            Field::Concurrency => Some(u32::from(d.max_active_sessions)),
            Field::ProviderLimit(i) => d
                .provider_limits
                .iter()
                .find(|l| l.provider == PROVIDERS[i])
                .map(|l| u32::from(l.max_active)),
            Field::Retries => Some(u32::from(d.max_recovery_attempts)),
            Field::RetryDelay => Some(d.retry_delay_seconds),
            Field::Deadline => Some(d.request_timeout_seconds),
            _ => None,
        };
        if let (Some(value), Some((min, max))) = (value, numeric_bounds(field)) {
            if !(min..=max).contains(&value) {
                return Some(format!("must be {min}–{max}"));
            }
        }
        if let Field::ProviderLimit(i) = field {
            // Entries past the daemon's collection bound are each invalid.
            return d
                .provider_limits
                .iter()
                .position(|l| l.provider == PROVIDERS[i])
                .filter(|position| *position >= MAX_PROVIDER_LIMITS)
                .map(|_| format!("at most {MAX_PROVIDER_LIMITS} provider limits; blank one"));
        }
        if value.is_some() {
            return None;
        }
        match field {
            Field::Spend => d
                .max_spend_usd
                .filter(|v| !v.is_finite() || *v <= 0.0)
                .map(|_| "must be a positive amount or blank".into()),
            Field::CreateGroups => (d.allow_create_groups
                && !d.capabilities.contains(&ManagerCapabilityV2::Topology))
            .then(|| "needs Grant Topology".into()),
            Field::AddGroup => (d.group_ids.len() > MANAGER_V2_MAX_GROUPS)
                .then(|| format!("at most {MANAGER_V2_MAX_GROUPS} Groups")),
            Field::AddPausedEpic => (d.paused_epic_ids.len() > MAX_PAUSED_EPICS)
                .then(|| format!("at most {MAX_PAUSED_EPICS} paused Epics")),
            Field::AddLaunch => (d.allowed_launches.len() > MAX_LAUNCHES)
                .then(|| format!("at most {MAX_LAUNCHES} launch choices")),
            Field::LaunchModel(i) | Field::EditLaunch(i) => d
                .allowed_launches
                .get(i)
                .and_then(|c| c.validate().err())
                .map(|_| "model must be 1–256 characters".into()),
            _ => None,
        }
    }
    pub fn request(&self) -> ConfigureHarnessManagerPolicyRequestV2 {
        ConfigureHarnessManagerPolicyRequestV2 {
            project_id: self.config.project_id,
            expected_scope_version: self.config.row_version,
            expected_policy_version: self.policy_version,
            idempotency_key: self.idempotency_key.clone(),
            policy: self.draft.clone(),
        }
    }
    fn changed(&mut self) {
        self.idempotency_key = Uuid::new_v4().to_string();
        self.notice = "Draft changed · s saves the complete policy".into();
    }
    pub fn activate(&mut self, field: Field) {
        let before = self.draft.clone();
        match field {
            Field::Preset(preset) => {
                self.apply_preset(preset);
                return;
            }
            Field::Advanced => {
                self.advanced_open = !self.advanced_open;
                return;
            }
            Field::Preview => {
                self.preview_open = !self.preview_open;
                return;
            }
            Field::Detail(_) => return,
            Field::Suggestions => {
                if let Some(profile) = self.classification().permission_profile {
                    let fields = self
                        .suggestions()
                        .iter()
                        .map(|s| s.field)
                        .collect::<Vec<_>>();
                    if let Err(error) = self.apply_suggestions(profile, &fields) {
                        self.error = Some(error);
                    }
                }
                return;
            }
            Field::EditLaunch(i) => {
                self.picker = Some(ManagerLaunchPickerState::new(
                    Some(i),
                    self.draft.allowed_launches.get(i).cloned(),
                ));
                return;
            }
            Field::AddLaunch => {
                if self.draft.allowed_launches.len() >= 32 {
                    self.error = Some("At most 32 allowed launch choices.".into());
                    return;
                }
                self.picker = Some(ManagerLaunchPickerState::new(None, None));
                return;
            }
            Field::Mode => {
                self.draft.mode = match self.draft.mode {
                    ManagerOperatingModeV2::Status => ManagerOperatingModeV2::Monitor,
                    ManagerOperatingModeV2::Monitor => ManagerOperatingModeV2::Execute,
                    ManagerOperatingModeV2::Execute => ManagerOperatingModeV2::Status,
                }
            }
            Field::Paused => self.draft.paused = !self.draft.paused,
            Field::Capability(i) => toggle(&mut self.draft.capabilities, CAPABILITIES[i]),
            Field::Epic(id) => toggle(&mut self.draft.paused_epic_ids, id),
            Field::Group(id) => toggle(&mut self.draft.group_ids, id),
            Field::CreateGroups => self.draft.allow_create_groups = !self.draft.allow_create_groups,
            Field::LaunchProvider(i) => {
                let pos = PROVIDERS
                    .iter()
                    .position(|p| *p == self.draft.allowed_launches[i].provider)
                    .unwrap_or(0);
                self.draft.allowed_launches[i].provider = PROVIDERS[(pos + 1) % PROVIDERS.len()];
            }
            Field::RemoveLaunch(i) => {
                self.draft.allowed_launches.remove(i);
            }
            Field::AddRawLaunch => {
                if self.draft.allowed_launches.len() >= 32 {
                    self.error = Some("At most 32 allowed launch choices.".into());
                    return;
                }
                self.draft.allowed_launches.push(ManagerLaunchChoiceV2 {
                    provider: SessionProvider::Claude,
                    model: String::new(),
                    effort: None,
                });
                let i = self.draft.allowed_launches.len() - 1;
                self.selected = self
                    .rows()
                    .iter()
                    .position(|r| r.field == Field::LaunchModel(i))
                    .unwrap();
                self.edit = Some((Field::LaunchModel(i), String::new()));
            }
            _ => {
                let value = self
                    .rows()
                    .into_iter()
                    .find(|r| r.field == field)
                    .map(|r| r.value)
                    .unwrap_or_default();
                self.edit = Some((
                    field,
                    if ["inherit", "default", "uncapped", "Enter UUID"].contains(&value.as_str()) {
                        String::new()
                    } else {
                        value
                    },
                ));
                return;
            }
        }
        self.record_edit(before, field);
    }
    pub fn apply_text(&mut self, field: Field, text: &str) -> Result<(), String> {
        let before = self.draft.clone();
        let value = text.trim();
        // Range-checked entry mirrors ManagerPolicyV2::validate: an
        // out-of-range value is refused here with the field's bounds instead
        // of failing the whole save.
        let bounded = || -> Result<u32, String> {
            let (min, max) = numeric_bounds(field).unwrap_or((0, u32::MAX));
            value
                .parse::<u32>()
                .ok()
                .filter(|n| (min..=max).contains(n))
                .ok_or_else(|| format!("Enter a whole number from {min} to {max}."))
        };
        let number = || bounded().map(|n| u16::try_from(n).unwrap_or(u16::MAX));
        match field {
            Field::Containers => self.draft.max_created_containers = number()?,
            Field::Sessions => self.draft.max_created_sessions = number()?,
            Field::Concurrency => self.draft.max_active_sessions = number()?,
            Field::Retries => self.draft.max_recovery_attempts = number()?,
            Field::RetryDelay => self.draft.retry_delay_seconds = bounded()?,
            Field::Deadline => self.draft.request_timeout_seconds = bounded()?,
            Field::Spend => {
                self.draft.max_spend_usd = if value.is_empty() {
                    None
                } else {
                    Some(
                        value
                            .parse::<f64>()
                            .ok()
                            .filter(|v| v.is_finite() && *v > 0.0)
                            .ok_or("Enter a positive USD amount; blank removes the cap.")?,
                    )
                }
            }
            Field::ProviderLimit(i) => {
                let max = if value.is_empty() {
                    None
                } else {
                    Some(number()?)
                };
                if let Some(position) = self
                    .draft
                    .provider_limits
                    .iter()
                    .position(|l| l.provider == PROVIDERS[i])
                {
                    if let Some(max_active) = max {
                        self.draft.provider_limits[position].max_active = max_active;
                    } else {
                        self.draft.provider_limits.remove(position);
                    }
                } else if let Some(max_active) = max {
                    if self.draft.provider_limits.len() >= MAX_PROVIDER_LIMITS {
                        return Err(format!(
                            "At most {MAX_PROVIDER_LIMITS} provider limits; blank another provider's limit to inherit first."
                        ));
                    }
                    self.draft.provider_limits.push(ManagerProviderLimitV2 {
                        provider: PROVIDERS[i],
                        max_active,
                    });
                }
            }
            Field::LaunchModel(i) => {
                if value.is_empty() {
                    return Err("Enter an exact model id.".into());
                }
                self.draft.allowed_launches[i].model = value.into();
            }
            Field::LaunchEffort(i) => {
                self.draft.allowed_launches[i].effort = (!value.is_empty()).then(|| value.into())
            }
            Field::AddPausedEpic | Field::AddGroup => {
                let id = Uuid::parse_str(value).map_err(
                    |_| "Enter a scoped container UUID from the Topology section of :manager board.",
                )?;
                if id.is_nil() {
                    return Err("A nil container UUID is invalid.".into());
                }
                let (ids, choices) = if field == Field::AddPausedEpic {
                    (&mut self.draft.paused_epic_ids, &mut self.epics)
                } else {
                    (&mut self.draft.group_ids, &mut self.groups)
                };
                if !ids.contains(&id) {
                    ids.push(id);
                }
                if !choices.iter().any(|g| g.0 == id) {
                    choices.push((id, id.to_string()));
                }
            }
            _ => return Err("This field is a toggle.".into()),
        }
        self.record_edit(before, field);
        Ok(())
    }
}
impl Field {
    fn policy_field(self) -> Option<ManagerPolicyField> {
        Some(match self {
            Self::Mode => ManagerPolicyField::Mode,
            Self::Paused => ManagerPolicyField::Paused,
            Self::Capability(_) => ManagerPolicyField::Capabilities,
            Self::Epic(_) | Self::AddPausedEpic => ManagerPolicyField::PausedEpicIds,
            Self::Group(_) | Self::AddGroup => ManagerPolicyField::GroupIds,
            Self::CreateGroups => ManagerPolicyField::AllowCreateGroups,
            Self::Containers => ManagerPolicyField::CreatedContainers,
            Self::Sessions => ManagerPolicyField::CreatedSessions,
            Self::Concurrency => ManagerPolicyField::ActiveSessions,
            Self::ProviderLimit(_) => ManagerPolicyField::ProviderLimits,
            Self::LaunchProvider(_)
            | Self::LaunchModel(_)
            | Self::LaunchEffort(_)
            | Self::RemoveLaunch(_)
            | Self::AddRawLaunch
            | Self::EditLaunch(_)
            | Self::AddLaunch => ManagerPolicyField::AllowedLaunches,
            Self::Retries => ManagerPolicyField::RecoveryAttempts,
            Self::RetryDelay => ManagerPolicyField::RetryDelaySeconds,
            Self::Deadline => ManagerPolicyField::RequestTimeoutSeconds,
            Self::Spend => ManagerPolicyField::MaxSpendUsd,
            _ => return None,
        })
    }
}
impl PolicyState {
    fn record_edit(&mut self, before: ManagerPolicyV2, field: Field) {
        if let Some(field) = field.policy_field() {
            self.touched.insert(field);
        }
        if self.draft != before {
            self.changed();
            self.error = None;
        }
        self.choice_labels.retain(|(provider, model), _| {
            self.draft
                .allowed_launches
                .iter()
                .any(|c| c.provider == *provider && c.model == *model)
        });
    }
    pub fn apply_preset(&mut self, preset: ManagerPolicyPreset) {
        if preset == ManagerPolicyPreset::Custom {
            self.advanced_open = true;
            return;
        }
        let edit = apply_manager_policy_preset(
            &self.draft,
            preset,
            &ManagerPresetContext {
                origin: if self.saved.is_some() {
                    ManagerPolicyOrigin::Saved
                } else {
                    ManagerPolicyOrigin::New
                },
                touched: &self.touched,
                scope_mode: self.config.scope_mode,
            },
        );
        if !edit.changed_fields.is_empty() {
            self.draft = edit.policy;
            self.changed();
            self.error = None;
        }
        self.touched.extend([
            ManagerPolicyField::Mode,
            ManagerPolicyField::Capabilities,
            ManagerPolicyField::AllowCreateGroups,
        ]);
    }
    pub fn apply_suggestions(
        &mut self,
        profile: ManagerPolicyPreset,
        fields: &[ManagerAllowanceField],
    ) -> Result<(), String> {
        let edit =
            apply_suggested_manager_allowances(&self.draft, profile, fields).map_err(|_| {
                "Suggestions changed; review the current conflicting zeros.".to_string()
            })?;
        self.touched.extend(edit.changed_fields.iter().copied());
        if !edit.changed_fields.is_empty() {
            self.draft = edit.policy;
            self.changed();
            self.error = None;
        }
        Ok(())
    }
    pub fn start_catalog_request(&mut self, socket: PathBuf) {
        let Some(picker) = &mut self.picker else {
            return;
        };
        if !picker.needs_refresh {
            return;
        }
        let Some(next) = self.catalog_generation.checked_add(1) else {
            picker.error = Some("Reopen policy editor to refresh catalog.".into());
            return;
        };
        self.catalog_generation = next;
        picker.request_generation = next;
        picker.needs_refresh = false;
        // Drop aborts the previous reader before starting another; no global selection ownership.
        self.catalog_request = None;
        self.catalog_request = Some(catalog::request_catalog(
            socket,
            next,
            picker.model_dropdown.provider,
        ));
    }
    pub fn apply_catalog_result(&mut self, result: ManagerCatalogResult) {
        let Some(picker) = &mut self.picker else {
            return;
        };
        if picker.request_generation != result.generation
            || picker.model_dropdown.provider != result.provider
        {
            return;
        }
        self.catalog_request = None;
        match result.outcome {
            Ok(models) => {
                self.choice_labels
                    .retain(|(provider, _), _| *provider != result.provider);
                for choice in self
                    .draft
                    .allowed_launches
                    .iter()
                    .filter(|c| c.provider == result.provider)
                {
                    if let Some((_, label)) = models.iter().find(|(id, _)| *id == choice.model) {
                        self.choice_labels
                            .insert((choice.provider, choice.model.clone()), label.clone());
                    }
                }
                picker.model_dropdown.models = models;
                picker.model_dropdown.selected_index = picker
                    .original
                    .as_ref()
                    .and_then(|c| {
                        picker
                            .model_dropdown
                            .models
                            .iter()
                            .position(|(id, _)| *id == c.model)
                    })
                    .unwrap_or(0);
                picker.status = CatalogStatus::Loaded;
            }
            Err(error) => picker.status = CatalogStatus::Failed(error),
        }
        self.catalog_status
            .insert(result.provider, picker.status.clone());
    }
    pub fn confirm_choice(&mut self, choice: ManagerLaunchChoiceV2) -> Result<(), String> {
        let picker = self.picker.as_ref().ok_or("No launch selection open")?;
        if picker.status != CatalogStatus::Loaded
            || picker.model_dropdown.provider != choice.provider
            || !picker.effort_choices.values().contains(&choice.effort)
        {
            return Err("Select a valid catalog model and effort first.".into());
        }
        let label = picker
            .model_dropdown
            .models
            .iter()
            .find(|(id, _)| *id == choice.model)
            .map(|(_, label)| label.clone())
            .ok_or("Model no longer in this catalog")?;
        choice.validate().map_err(str::to_string)?;
        let row = picker.row;
        if let Some(i) = row {
            if self.draft.allowed_launches.get(i) != picker.original.as_ref() {
                return Err("Restriction changed; cancel and select it again.".into());
            }
        }
        if let Some(i) = self
            .draft
            .allowed_launches
            .iter()
            .position(|c| c == &choice)
        {
            if row != Some(i) {
                self.picker = None;
                self.catalog_request = None;
                self.selected = self
                    .rows()
                    .iter()
                    .position(|r| r.field == Field::EditLaunch(i))
                    .unwrap_or(0);
                self.touched.insert(ManagerPolicyField::AllowedLaunches);
                self.notice = "Already listed; existing restriction retained.".into();
                return Ok(());
            }
        }
        let before = self.draft.clone();
        if let Some(i) = row {
            self.draft.allowed_launches[i] = choice.clone();
        } else if self.draft.allowed_launches.len() < 32 {
            self.draft.allowed_launches.push(choice.clone());
        } else {
            return Err("At most 32 allowed launch choices.".into());
        }
        self.record_edit(before, Field::AddLaunch);
        self.choice_labels
            .insert((choice.provider, choice.model), label);
        self.picker = None;
        self.catalog_request = None;
        Ok(())
    }
}
/// States what the daemon actually granted, so a save that lands as Observe
/// (Status mode, no write grants) is never silent.
fn saved_confirmation(config: &HarnessManagerConfigV1, policy: &ManagerPolicyV2) -> String {
    let preset = classify_manager_policy(policy, config.scope_mode).preset;
    let grants = if policy.capabilities.is_empty() {
        "no write grants".to_string()
    } else {
        format!(
            "grants {}",
            policy
                .capabilities
                .iter()
                .map(|c| format!("{c:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    format!(
        "Policy saved as {} · mode {:?} · {grants}{} · existing work and usage retained",
        preset.label(),
        policy.mode,
        if policy.paused {
            " · OPERATOR PAUSE"
        } else {
            ""
        }
    )
}
fn toggle<T: PartialEq>(values: &mut Vec<T>, value: T) {
    if let Some(i) = values.iter().position(|v| *v == value) {
        values.remove(i);
    } else {
        values.push(value);
    }
}

pub(super) async fn load(
    app: &mut App,
    config: HarnessManagerConfigV1,
    identity: String,
) -> Result<PolicyState, String> {
    let stored = app
        .client
        .get_harness_manager_policy(config.project_id)
        .await
        .map_err(|e| format!("Manager policy: {e}"))?;
    let version = stored.as_ref().map_or(0, |p| p.row_version);
    let revoked = stored.as_ref().is_some_and(|p| p.revoked);
    let policy = stored
        .as_ref()
        .map(|p| p.policy.clone())
        .unwrap_or_default();
    let mut epics = config
        .epic_ids
        .iter()
        .chain(policy.paused_epic_ids.iter())
        .copied()
        .collect::<Vec<_>>();
    epics.sort();
    epics.dedup();
    let epics = epics
        .into_iter()
        .map(|id| (id, label_for(app, id)))
        .collect();
    let mut groups = app
        .sessions
        .values()
        .filter(|s| {
            s.session.project_id == Some(config.project_id)
                && s.session.session_kind == SessionKind::Group
        })
        .map(|s| s.session.id)
        .chain(policy.group_ids.iter().copied())
        .collect::<Vec<_>>();
    groups.sort();
    groups.dedup();
    let groups = groups
        .into_iter()
        .map(|id| (id, label_for(app, id)))
        .collect();
    app.overlay_leader_pending = false;
    // The floating global picker intercepts keys before overlays. Dismiss its
    // old focus while preserving the selected global provider/model/effort.
    app.model_dropdown.close();
    Ok(PolicyState {
        config,
        identity,
        policy_version: version,
        saved: stored,
        opened_draft: policy.clone(),
        draft: policy,
        touched: BTreeSet::new(),
        advanced_open: false,
        preview_open: true,
        picker: None,
        catalog_request: None,
        catalog_generation: 0,
        catalog_status: HashMap::new(),
        usage: usage::UsageView::Loading,
        usage_request: None,
        usage_quota: None,
        usage_generation: 0,
        usage_socket: None,
        choice_labels: HashMap::new(),
        custom_provider_names: app.settings.custom_providers.iter().map(|p| (p.id, p.name.clone())).collect(),
        epics,
        groups,
        selected: 0,
        edit: None,
        error: None,
        idempotency_key: Uuid::new_v4().to_string(),
        notice: if revoked {
            "Saved grant revoked. Explicit s saves and regrants this exact draft under current scope."
        } else {
            "Explicit operator grant · Enter/Space edit · s save"
        }
        .into(),
    })
}

#[cfg(test)]
pub(super) async fn open(
    app: &mut App,
    config: HarnessManagerConfigV1,
    identity: String,
) -> Result<(), String> {
    let project_id = config.project_id;
    let state = load(app, config.clone(), identity.clone()).await?;
    let query = AgentManagerInspectRequestV2 {
        section: ManagerInspectSectionV2::Overview,
        ..Default::default()
    };
    let ledger = super::board::BoardState {
        project_id,
        identity: identity.clone(),
        query,
        inspection: ManagerInspectionV2 {
            observed_at: chrono::Utc::now(),
            scope_version: config.row_version,
            policy: None,
            section: ManagerInspectSectionV2::Overview,
            rows: vec![],
            next_cursor: None,
            complete: true,
        },
        previous: vec![],
        selected: 0,
        detail_scroll: 0,
        error: None,
        notice: String::new(),
        answer: None,
        composite: None,
    };
    app.overlay_leader_pending = false;
    app.overlay = OverlayState::HarnessManagerV2(Box::new(super::ManagerSurface {
        project_id,
        identity,
        section: super::ManagerSection::Policy,
        ledger,
        policy: Some(state),
        config,
    }));
    Ok(())
}

pub(super) async fn handle_key(app: &mut App, key: KeyEvent) {
    let socket = app.client.socket_path().to_path_buf();
    let Some(state) = super::policy_mut(app) else {
        return;
    };
    if let Some(picker) = &mut state.picker {
        match picker.handle_key(key) {
            PickerOutcome::Dismissed => {
                state.picker = None;
                state.catalog_request = None;
            }
            PickerOutcome::Confirmed(choice) => {
                if let Err(error) = state.confirm_choice(choice) {
                    if let Some(picker) = &mut state.picker {
                        picker.error = Some(error);
                    }
                }
            }
            PickerOutcome::Consumed => {}
        }
        state.start_catalog_request(socket);
        return;
    }
    if let Some((field, value)) = &mut state.edit {
        match key.code {
            KeyCode::Esc => state.edit = None,
            KeyCode::Enter => {
                let (field, value) = (*field, value.clone());
                match state.apply_text(field, &value) {
                    Ok(()) => state.edit = None,
                    Err(e) => state.error = Some(e),
                }
            }
            KeyCode::Backspace => {
                value.pop();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => value.clear(),
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                value.push(c)
            }
            _ => {}
        }
        return;
    }
    let rows = state.rows();
    if super::super::list::handle_list_nav_key(&mut state.selected, rows.len(), &key) {
        return;
    }
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => app.overlay = OverlayState::None,
        KeyCode::Enter | KeyCode::Char(' ') => {
            if let Some(row) = rows.get(state.selected) {
                state.activate(row.field);
                state.start_catalog_request(socket);
            }
        }
        KeyCode::Char('s') => {
            if let Err(e) = state.draft.validate() {
                // Point at the first invalid row instead of a bare code.
                let rows = state.rows();
                state.error = Some(
                    if let Some((i, row)) = rows.iter().enumerate().find(|(_, r)| r.error.is_some())
                    {
                        state.selected = i;
                        format!(
                            "Policy not saved: {} {}.",
                            row.label,
                            row.error.as_deref().unwrap_or_default()
                        )
                    } else {
                        format!(
                            "Policy not saved: {e}. Check quotas, delays, grants and launch choices."
                        )
                    },
                );
                return;
            }
            let request = state.request();
            match app.client.configure_harness_manager_policy(request).await {
                Ok(saved) => {
                    if let Some(state) = super::policy_mut(app) {
                        state.opened_draft = saved.policy.clone();
                        state.draft = saved.policy.clone();
                        state.policy_version = saved.row_version;
                        state.saved = Some(saved);
                        state.touched.clear();
                        state.error = None;
                        state.notice = saved_confirmation(&state.config, &state.draft);
                        state.usage_quota = Some(state.draft.max_created_sessions);
                        state.bump_usage_generation();
                        state.idempotency_key = Uuid::new_v4().to_string();
                    }
                }
                Err(e) => {
                    if let Some(state) = super::policy_mut(app) {
                        state.error = Some(format!(
                            "Policy not saved: {e}. Draft retained; r reloads current grant."
                        ));
                    }
                }
            }
        }
        KeyCode::Char('r') => {
            super::reload_policy(app).await;
        }
        _ => {}
    }
}
