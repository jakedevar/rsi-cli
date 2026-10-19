//! Grant editor (#2237): the one "what may this manager do" form behind both
//! places an operator creates a manager with a chosen grant.
//!
//! * The global manager's create flow (`global_manager_workspace_launch`)
//!   edits the grant it saves for itself: the launch allowlist for the project
//!   managers it appoints plus the policy body those managers receive.
//! * The new-session modal's back side (`launch_settings`) edits the policy a
//!   project manager receives when the launched session is appointed.
//!
//! The editor only edits a [`ManagerPolicyV2`] (and, for the global grant, a
//! separate launch allowlist). It never talks to the daemon: callers send the
//! result through the existing operator RPCs. The rows are bounded the way the
//! daemon bounds them (`ManagerPolicyV2::validate`), so a draft the editor
//! builds is valid by construction; [`GrantEditor::validate`] still checks it
//! before submit and names the row to fix.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rsi_common::harness_manager::HarnessManagerScopeModeV1;
use rsi_common::harness_manager_presets::{
    ManagerPolicyOrigin, ManagerPolicyPreset, ManagerPresetContext, apply_manager_policy_preset,
    classify_manager_policy,
};
use rsi_common::harness_manager_v2::{
    ManagerCapabilityV2, ManagerLaunchChoiceV2, ManagerOperatingModeV2, ManagerPolicyV2,
};
use std::collections::BTreeSet;

use crate::overlay::launch_settings::MANAGER_LAUNCH_PRESETS as PRESETS;
use crate::overlay::manager_v2::policy::{CAPABILITIES, capability_description, capability_label};

const MODES: [ManagerOperatingModeV2; 3] = [
    ManagerOperatingModeV2::Status,
    ManagerOperatingModeV2::Monitor,
    ManagerOperatingModeV2::Execute,
];

/// Daemon bounds, mirrored from `ManagerPolicyV2::validate`.
const MAX_CONTAINERS: u16 = 64;
const MAX_SESSIONS: u16 = 1024;
const MAX_ACTIVE: u16 = 100;
const SESSIONS_STEP: u16 = 4;
const SPEND_STEP: f64 = 5.0;

/// One selectable row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantRow {
    Preset,
    Mode,
    Capability(usize),
    Containers,
    Sessions,
    Active,
    Spend,
    Launch(usize),
}

/// The titled sections the rows fall under, for rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantSection {
    Authority,
    Capabilities,
    Allowances,
    Launches,
}

impl GrantSection {
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Authority => "AUTHORITY",
            Self::Capabilities => "CAPABILITIES",
            Self::Allowances => "ALLOWANCES",
            Self::Launches => "LAUNCHES",
        }
    }
}

impl GrantRow {
    #[must_use]
    pub const fn section(self) -> GrantSection {
        match self {
            Self::Preset | Self::Mode => GrantSection::Authority,
            Self::Capability(_) => GrantSection::Capabilities,
            Self::Containers | Self::Sessions | Self::Active | Self::Spend => {
                GrantSection::Allowances
            }
            Self::Launch(_) => GrantSection::Launches,
        }
    }

    /// Rows whose value is typed as a number.
    #[must_use]
    pub const fn is_numeric(self) -> bool {
        matches!(
            self,
            Self::Containers | Self::Sessions | Self::Active | Self::Spend
        )
    }
}

/// What a key did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantKey {
    /// Not a grant key: the caller may use it.
    Ignored,
    /// The draft or the selection changed.
    Handled,
    /// Esc: leave the editor.
    Leave,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GrantEditor {
    policy: ManagerPolicyV2,
    /// Every launch the editor offers, in display order.
    catalog: Vec<ManagerLaunchChoiceV2>,
    /// The checked subset of `catalog`.
    launches: Vec<ManagerLaunchChoiceV2>,
    /// Whether the launches live in the policy (a project manager's own
    /// allowlist) or beside it (the global grant's launch list).
    launches_in_policy: bool,
    /// A global grant must allow at least one launch.
    require_launches: bool,
    scope_mode: HarnessManagerScopeModeV1,
    pub selected: usize,
    /// Digits typed for the selected numeric row (Spend also holds a decimal
    /// prefix such as `0.`).
    typed: String,
    pub error: Option<String>,
}

fn dedup(choices: impl IntoIterator<Item = ManagerLaunchChoiceV2>) -> Vec<ManagerLaunchChoiceV2> {
    let mut out: Vec<ManagerLaunchChoiceV2> = Vec::new();
    for choice in choices {
        if !out.contains(&choice) {
            out.push(choice);
        }
    }
    out
}

impl GrantEditor {
    /// The editor for the global grant: `policy` is the body its project
    /// managers receive, `launches` the allowlist it may launch them from.
    #[must_use]
    pub fn for_global(
        policy: ManagerPolicyV2,
        launches: Vec<ManagerLaunchChoiceV2>,
        catalog: Vec<ManagerLaunchChoiceV2>,
    ) -> Self {
        let catalog = dedup(catalog.into_iter().chain(launches.iter().cloned()));
        Self {
            policy,
            catalog,
            launches: dedup(launches),
            launches_in_policy: false,
            require_launches: true,
            scope_mode: HarnessManagerScopeModeV1::Project,
            selected: 0,
            typed: String::new(),
            error: None,
        }
    }

    /// The editor for one project manager's policy; its allowed launches are
    /// the policy's own.
    #[must_use]
    pub fn for_manager(
        policy: ManagerPolicyV2,
        scope_mode: HarnessManagerScopeModeV1,
        catalog: Vec<ManagerLaunchChoiceV2>,
    ) -> Self {
        let launches = dedup(policy.allowed_launches.clone());
        let catalog = dedup(catalog.into_iter().chain(launches.iter().cloned()));
        Self {
            policy,
            catalog,
            launches,
            launches_in_policy: true,
            require_launches: false,
            scope_mode,
            selected: 0,
            typed: String::new(),
            error: None,
        }
    }

    /// Every selectable row, top to bottom.
    #[must_use]
    pub fn rows(&self) -> Vec<GrantRow> {
        [GrantRow::Preset, GrantRow::Mode]
            .into_iter()
            .chain((0..CAPABILITIES.len()).map(GrantRow::Capability))
            .chain([
                GrantRow::Containers,
                GrantRow::Sessions,
                GrantRow::Active,
                GrantRow::Spend,
            ])
            .chain((0..self.catalog.len()).map(GrantRow::Launch))
            .collect()
    }

    #[must_use]
    pub fn selected_row(&self) -> GrantRow {
        self.rows()
            .get(self.selected)
            .copied()
            .unwrap_or(GrantRow::Preset)
    }

    /// The preset the draft currently matches (`Custom` when none does).
    #[must_use]
    pub fn preset(&self) -> ManagerPolicyPreset {
        classify_manager_policy(&self.finish(), self.scope_mode).preset
    }

    #[must_use]
    pub fn policy(&self) -> &ManagerPolicyV2 {
        &self.policy
    }

    /// The checked launches.
    #[must_use]
    pub fn launches(&self) -> &[ManagerLaunchChoiceV2] {
        &self.launches
    }

    /// Digits typed for the selected row but not yet a full value.
    #[must_use]
    pub fn typed(&self) -> &str {
        &self.typed
    }

    /// The policy to send: the draft with the launch selection folded in when
    /// the launches belong to the policy.
    #[must_use]
    pub fn finish(&self) -> ManagerPolicyV2 {
        let mut policy = self.policy.clone();
        if self.launches_in_policy {
            policy.allowed_launches = self.launches.clone();
        }
        policy
    }

    /// The row's label.
    #[must_use]
    pub fn label(&self, row: GrantRow) -> String {
        match row {
            GrantRow::Preset => "Preset".into(),
            GrantRow::Mode => "Mode".into(),
            GrantRow::Capability(index) => capability_label(CAPABILITIES[index]),
            GrantRow::Containers => "Containers".into(),
            GrantRow::Sessions => "Sessions".into(),
            GrantRow::Active => "Active".into(),
            GrantRow::Spend => "Spend".into(),
            GrantRow::Launch(index) => self
                .catalog
                .get(index)
                .map_or_else(String::new, ManagerLaunchChoiceV2::display),
        }
    }

    /// The row's value as shown: a check box, a number or a label.
    #[must_use]
    pub fn value(&self, row: GrantRow) -> String {
        match row {
            GrantRow::Preset => self.preset().label().into(),
            GrantRow::Mode => format!("{:?}", self.policy.mode),
            GrantRow::Capability(index) => {
                check(self.policy.capabilities.contains(&CAPABILITIES[index]))
            }
            GrantRow::Containers => self.policy.max_created_containers.to_string(),
            GrantRow::Sessions => self.policy.max_created_sessions.to_string(),
            GrantRow::Active => self.policy.max_active_sessions.to_string(),
            GrantRow::Spend if !self.typed.is_empty() && self.selected_row() == row => {
                // The edit buffer as typed, so `0.` and `.5` read faithfully.
                format!("${}", self.typed)
            }
            GrantRow::Spend => self
                .policy
                .max_spend_usd
                .map_or_else(|| "uncapped".into(), format_usd),
            GrantRow::Launch(index) => check(
                self.catalog
                    .get(index)
                    .is_some_and(|choice| self.launches.contains(choice)),
            ),
        }
    }

    /// One line saying what the selected row does.
    #[must_use]
    pub fn description(&self, row: GrantRow) -> String {
        match row {
            GrantRow::Preset => {
                "Observe watches; Execute runs work; Full grants every capability. Applying one resets the mode, capabilities and creation allowances."
                    .into()
            }
            GrantRow::Mode => {
                "Status reports only; Monitor watches the project; Execute lets the manager act."
                    .into()
            }
            GrantRow::Capability(index) => capability_description(CAPABILITIES[index]).into(),
            GrantRow::Containers => {
                format!("Groups and Epics the manager may create (0-{MAX_CONTAINERS}).")
            }
            GrantRow::Sessions => format!(
                "Sessions the manager may create over its life (0-{MAX_SESSIONS}); type a number or use h/l."
            ),
            GrantRow::Active => {
                format!("Sessions it may run at once (1-{MAX_ACTIVE}).")
            }
            GrantRow::Spend => {
                "Spend ceiling in USD; h at the lowest value lifts the cap.".into()
            }
            GrantRow::Launch(_) => {
                if self.launches_in_policy {
                    "Launches this manager may use for the sessions it creates.".into()
                } else {
                    "Launches the global manager may start project managers from.".into()
                }
            }
        }
    }

    /// Advisory lines (not errors) about the draft.
    #[must_use]
    pub fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self
            .policy
            .capabilities
            .contains(&ManagerCapabilityV2::SessionCreate)
            && self.policy.max_created_sessions == 0
        {
            out.push(
                "Session creation is granted with a 0 allowance: raise Sessions or the manager cannot create sessions."
                    .into(),
            );
        }
        if self.policy.mode == ManagerOperatingModeV2::Status
            && !self.policy.capabilities.is_empty()
        {
            out.push(
                "Mode Status only reports: set Execute for the capabilities to take effect.".into(),
            );
        }
        out
    }

    /// One line digest of the draft, for a collapsed row.
    #[must_use]
    pub fn summary(&self) -> String {
        let capabilities = self.policy.capabilities.len();
        format!(
            "{} · {capabilities} capabilit{} · sessions {} · active {} · {} launch{}",
            self.preset().label(),
            if capabilities == 1 { "y" } else { "ies" },
            self.policy.max_created_sessions,
            self.policy.max_active_sessions,
            self.launches.len(),
            if self.launches.len() == 1 { "" } else { "es" },
        )
    }

    /// Check the draft the way the daemon will.
    ///
    /// # Errors
    /// The row to fix and one sentence for the operator.
    pub fn validate(&self) -> Result<(), (GrantRow, String)> {
        if self.require_launches && self.launches.is_empty() {
            return Err((
                GrantRow::Launch(0),
                "Allow at least one launch: the global manager starts project managers from this list".into(),
            ));
        }
        if self.launches.len() > rsi_common::global_manager::GLOBAL_MANAGER_MAX_LAUNCHES {
            return Err((
                GrantRow::Launch(0),
                format!(
                    "At most {} launches may be allowed",
                    rsi_common::global_manager::GLOBAL_MANAGER_MAX_LAUNCHES
                ),
            ));
        }
        self.finish().validate().map_err(|code| {
            (
                GrantRow::Preset,
                format!("The daemon would refuse this grant ({code}); apply a preset and adjust from there"),
            )
        })
    }

    /// Reset the draft to `preset`, keeping the spend ceiling and concurrency.
    pub fn apply_preset(&mut self, preset: ManagerPolicyPreset) {
        let touched = BTreeSet::new();
        self.policy = apply_manager_policy_preset(
            &self.policy,
            preset,
            &ManagerPresetContext {
                origin: ManagerPolicyOrigin::New,
                touched: &touched,
                scope_mode: self.scope_mode,
            },
        )
        .policy;
        self.typed.clear();
        self.error = None;
    }

    /// The scope the manager is appointed over changed: presets that follow
    /// the scope (group creation under Full) use the new mode from now on.
    pub fn set_scope_mode(&mut self, scope_mode: HarnessManagerScopeModeV1) {
        self.scope_mode = scope_mode;
    }

    /// Put the cursor on `row` (the row a validation error names).
    pub fn select_row(&mut self, row: GrantRow) {
        if let Some(index) = self.rows().iter().position(|candidate| *candidate == row) {
            self.move_to(index);
        }
    }

    fn move_to(&mut self, index: usize) {
        let last = self.rows().len().saturating_sub(1);
        self.selected = index.min(last);
        self.typed.clear();
    }

    fn toggle_capability(&mut self, capability: ManagerCapabilityV2) {
        let capabilities = &mut self.policy.capabilities;
        if let Some(position) = capabilities.iter().position(|c| *c == capability) {
            capabilities.remove(position);
        } else {
            capabilities.push(capability);
        }
        // The daemon refuses group creation without Topology.
        if !self
            .policy
            .capabilities
            .contains(&ManagerCapabilityV2::Topology)
        {
            self.policy.allow_create_groups = false;
        }
    }

    fn toggle_launch(&mut self, index: usize) {
        let Some(choice) = self.catalog.get(index).cloned() else {
            return;
        };
        if let Some(position) = self.launches.iter().position(|c| *c == choice) {
            self.launches.remove(position);
        } else {
            self.launches.push(choice);
        }
    }

    fn cycle<T: Copy + PartialEq>(values: &[T], current: T, delta: isize) -> T {
        let len = values.len() as isize;
        let index = values.iter().position(|v| *v == current).unwrap_or(0) as isize;
        values[(index + delta).rem_euclid(len) as usize]
    }

    /// `h`/`l`: step the selected row.
    fn adjust(&mut self, delta: isize) {
        self.typed.clear();
        match self.selected_row() {
            GrantRow::Preset => {
                let current = PRESETS
                    .iter()
                    .copied()
                    .find(|preset| *preset == self.preset())
                    .unwrap_or(ManagerPolicyPreset::Execute);
                let next = Self::cycle(&PRESETS, current, delta);
                self.apply_preset(next);
            }
            GrantRow::Mode => self.policy.mode = Self::cycle(&MODES, self.policy.mode, delta),
            GrantRow::Capability(index) => self.toggle_capability(CAPABILITIES[index]),
            GrantRow::Launch(index) => self.toggle_launch(index),
            GrantRow::Containers => {
                self.policy.max_created_containers = step_u16(
                    self.policy.max_created_containers,
                    delta,
                    1,
                    0,
                    MAX_CONTAINERS,
                );
            }
            GrantRow::Sessions => {
                self.policy.max_created_sessions = step_u16(
                    self.policy.max_created_sessions,
                    delta,
                    SESSIONS_STEP,
                    0,
                    MAX_SESSIONS,
                );
            }
            GrantRow::Active => {
                self.policy.max_active_sessions =
                    step_u16(self.policy.max_active_sessions, delta, 1, 1, MAX_ACTIVE);
            }
            GrantRow::Spend => {
                self.policy.max_spend_usd = match (self.policy.max_spend_usd, delta > 0) {
                    (None, true) => Some(SPEND_STEP),
                    (None, false) => None,
                    (Some(usd), true) => Some(usd + SPEND_STEP),
                    (Some(usd), false) if usd - SPEND_STEP >= SPEND_STEP => Some(usd - SPEND_STEP),
                    (Some(_), false) => None,
                };
            }
        }
        self.error = None;
    }

    /// Space / Enter: toggle a check box, otherwise step forward.
    fn activate(&mut self) {
        self.adjust(1);
    }

    /// Type into the selected numeric row. Every keystroke that makes a valid
    /// in-bounds value applies at once; others are dropped.
    fn type_char(&mut self, ch: char) {
        let row = self.selected_row();
        if !row.is_numeric() || !(ch.is_ascii_digit() || (ch == '.' && row == GrantRow::Spend)) {
            return;
        }
        let mut next = self.typed.clone();
        next.push(ch);
        if self.apply_typed(row, &next) {
            self.typed = next;
            self.error = None;
        } else {
            self.error = Some(format!(
                "{} must be {}",
                self.label(row),
                match row {
                    GrantRow::Containers => format!("0-{MAX_CONTAINERS}"),
                    GrantRow::Sessions => format!("0-{MAX_SESSIONS}"),
                    GrantRow::Active => format!("1-{MAX_ACTIVE}"),
                    _ => "a positive number".into(),
                }
            ));
        }
    }

    fn backspace(&mut self) {
        let row = self.selected_row();
        if !row.is_numeric() {
            return;
        }
        self.typed.pop();
        let text = self.typed.clone();
        if text.is_empty() {
            match row {
                GrantRow::Containers => self.policy.max_created_containers = 0,
                GrantRow::Sessions => self.policy.max_created_sessions = 0,
                GrantRow::Active => self.policy.max_active_sessions = 1,
                GrantRow::Spend => self.policy.max_spend_usd = None,
                _ => {}
            }
        } else {
            self.apply_typed(row, &text);
        }
        self.error = None;
    }

    /// Apply `text` to a numeric row when it parses inside the row's bounds.
    fn apply_typed(&mut self, row: GrantRow, text: &str) -> bool {
        let within =
            |max: u16, min: u16| text.parse::<u16>().ok().filter(|n| (min..=max).contains(n));
        match row {
            GrantRow::Containers => within(MAX_CONTAINERS, 0)
                .map(|n| self.policy.max_created_containers = n)
                .is_some(),
            GrantRow::Sessions => within(MAX_SESSIONS, 0)
                .map(|n| self.policy.max_created_sessions = n)
                .is_some(),
            GrantRow::Active => within(MAX_ACTIVE, 1)
                .map(|n| self.policy.max_active_sessions = n)
                .is_some(),
            GrantRow::Spend => match parse_spend(text) {
                SpendText::Invalid => false,
                // `0`, `0.` and `.` are prefixes of a value: they stay in the
                // edit buffer and the last complete value stands.
                SpendText::Incomplete => true,
                SpendText::Value(usd) => {
                    self.policy.max_spend_usd = Some(usd);
                    true
                }
            },
            _ => false,
        }
    }

    /// Route one key. Navigation: `j`/`k`/arrows/`g`/`G`. Change: `h`/`l`/arrows
    /// step, `Space`/`Enter` toggle, digits type a number, `Esc` leaves.
    pub fn handle_key(&mut self, key: KeyEvent) -> GrantKey {
        let plain = key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT;
        if !plain {
            return GrantKey::Ignored;
        }
        match key.code {
            KeyCode::Esc => return GrantKey::Leave,
            KeyCode::Char('j') | KeyCode::Down => self.move_to(self.selected + 1),
            KeyCode::Char('k') | KeyCode::Up => self.move_to(self.selected.saturating_sub(1)),
            KeyCode::Char('g') | KeyCode::Home => self.move_to(0),
            KeyCode::Char('G') | KeyCode::End => self.move_to(usize::MAX),
            KeyCode::Char('h') | KeyCode::Left => self.adjust(-1),
            KeyCode::Char('l') | KeyCode::Right => self.adjust(1),
            KeyCode::Char(' ') | KeyCode::Enter => self.activate(),
            KeyCode::Char(ch) if ch.is_ascii_digit() || ch == '.' => self.type_char(ch),
            KeyCode::Backspace => self.backspace(),
            _ => return GrantKey::Ignored,
        }
        GrantKey::Handled
    }
}

/// What the Spend edit buffer holds.
#[derive(Debug, Clone, Copy, PartialEq)]
enum SpendText {
    /// Not a dollar amount, or past the ceiling.
    Invalid,
    /// A valid prefix with no positive value yet (`0`, `0.`, `.`, `0.0`).
    Incomplete,
    Value(f64),
}

/// Parse a dollar amount typed with at most two decimals.
fn parse_spend(text: &str) -> SpendText {
    let (whole, cents) = text.split_once('.').unwrap_or((text, ""));
    if whole.len() > 7
        || cents.len() > 2
        || !whole.chars().all(|ch| ch.is_ascii_digit())
        || !cents.chars().all(|ch| ch.is_ascii_digit())
    {
        return SpendText::Invalid;
    }
    let whole: f64 = if whole.is_empty() {
        0.0
    } else {
        whole.parse().unwrap_or(f64::NAN)
    };
    let cents: f64 = if cents.is_empty() {
        0.0
    } else {
        cents.parse::<f64>().unwrap_or(f64::NAN) / 10f64.powi(cents.len() as i32)
    };
    let usd = whole + cents;
    if !usd.is_finite() || usd > 1_000_000.0 {
        SpendText::Invalid
    } else if usd > 0.0 {
        SpendText::Value(usd)
    } else {
        SpendText::Incomplete
    }
}

/// Dollars as shown: whole amounts bare, anything else with its cents.
fn format_usd(usd: f64) -> String {
    if (usd - usd.round()).abs() < 0.005 {
        format!("${usd:.0}")
    } else {
        format!("${usd:.2}")
    }
}

fn check(on: bool) -> String {
    String::from(if on { "[x]" } else { "[ ]" })
}

fn step_u16(value: u16, delta: isize, step: u16, min: u16, max: u16) -> u16 {
    let next = i64::from(value) + i64::from(step) * delta.signum() as i64;
    u16::try_from(next.clamp(i64::from(min), i64::from(max))).unwrap_or(min)
}

#[cfg(test)]
#[path = "grant_editor_tests.rs"]
mod tests;
