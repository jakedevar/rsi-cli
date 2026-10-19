//! Staged ("pending") edits for the Settings panel (#2241).
//!
//! A settings dialog works like any other: a value moves both up and down,
//! nothing is committed until the operator applies, and the operator can see
//! what is pending. This module is the whole model of that:
//!
//! - [`step_index`] / [`RowValue::stepped`] are the one stepping function every
//!   row uses. Numeric rows clamp at their minimum and maximum and never wrap
//!   (a wrap once turned "Maximum sandbox roots" from its maximum back to 512
//!   and refused every sandbox); enum and Cycle rows wrap.
//! - [`PendingSettings`] holds the staged values. A keypress only changes this
//!   copy. `w` applies every pending row in one action (daemon rows through
//!   `UpdateDaemonConfig`, local preferences into `UserSettings`); Esc or `q`
//!   discards the lot.
//! - [`LocalPref`] names the local TUI preference rows that follow the same
//!   rule, with the one read/write pair each of them needs.
//!
//! Dirty is derived, never stored: a staged value is pending only while it
//! differs from the live value, so stepping a row back to where it started
//! clears its marker and a daemon refresh that lands on the staged value
//! settles it.

use std::borrow::Cow;

use crate::app::App;
use crate::settings::{
    ActivityIndicatorStyle, DaemonFeatureEntry, DaemonFeatureValue, DetailColumnAlignment,
    FORMULATION_ANIM_DURATIONS, NAVIGATOR_SETTINGS_ROW_COUNT, UserSettings,
};
use crate::settings_registry::{
    SettingApply, SettingKind, SettingOwner, SettingSpec, SettingsSection,
};
use crate::types::{CardField, NavigatorOptionalColumn, NavigatorPreset};

/// Activity indicator styles in `ActivityIndicatorStyle::next` order.
pub(crate) const ACTIVITY_STYLES: [ActivityIndicatorStyle; 6] = [
    ActivityIndicatorStyle::Semantic,
    ActivityIndicatorStyle::RainbowClassic,
    ActivityIndicatorStyle::RainbowCompact,
    ActivityIndicatorStyle::RainbowClassicCompact,
    ActivityIndicatorStyle::SonicSpeedUp,
    ActivityIndicatorStyle::RainbowStarlight,
];

/// Detail column alignments in `DetailColumnAlignment::next` order.
pub(crate) const DETAIL_ALIGNMENTS: [DetailColumnAlignment; 3] = [
    DetailColumnAlignment::Dynamic,
    DetailColumnAlignment::Left,
    DetailColumnAlignment::Center,
];

/// The daemon field behind the System prompt row.
pub(crate) const SYSTEM_PROMPT_FIELD: &str = "system_prompt_preset";

/// What a key does to the selected row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Enter / Space: the row's forward meaning (flip a switch, next choice).
    Activate,
    /// `+`: switch on / next choice.
    Next,
    /// `-`: switch off / previous choice.
    Previous,
}

/// What stepping does at the first and last option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepEdge {
    /// Stay at the end: numeric ranges.
    Clamp,
    /// Continue at the other end: enum choices.
    Wrap,
}

/// Move `index` one place through `len` options. An index outside the options
/// (an unknown value) enters at the first option going forward and the last
/// one going back.
#[must_use]
pub fn step_index(index: usize, len: usize, forward: bool, edge: StepEdge) -> usize {
    if len == 0 {
        return 0;
    }
    if index >= len {
        return if forward { 0 } else { len - 1 };
    }
    match (forward, edge) {
        (true, StepEdge::Wrap) => (index + 1) % len,
        (true, StepEdge::Clamp) => (index + 1).min(len - 1),
        (false, StepEdge::Wrap) => (index + len - 1) % len,
        (false, StepEdge::Clamp) => index.saturating_sub(1),
    }
}

/// The edge behavior of a Cycle row's options: numeric option lists (a leading
/// "Default" is allowed) clamp, everything else wraps.
#[must_use]
pub fn edge_for_options(options: &[String]) -> StepEdge {
    let mut numeric = 0usize;
    for option in options {
        if option == "Default" {
            continue;
        }
        if option.parse::<f64>().is_err() {
            return StepEdge::Wrap;
        }
        numeric += 1;
    }
    if numeric > 0 {
        StepEdge::Clamp
    } else {
        StepEdge::Wrap
    }
}

/// A staged value: a switch position or an option index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StagedValue {
    Bool(bool),
    Choice(usize),
}

/// A row's value with the shape it steps through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowValue {
    Bool(bool),
    Choice {
        index: usize,
        len: usize,
        edge: StepEdge,
    },
}

impl RowValue {
    #[must_use]
    pub const fn staged(self) -> StagedValue {
        match self {
            Self::Bool(value) => StagedValue::Bool(value),
            Self::Choice { index, .. } => StagedValue::Choice(index),
        }
    }

    /// This row shape holding `staged` (a mismatched kind keeps the row as is).
    #[must_use]
    pub const fn with_staged(self, staged: StagedValue) -> Self {
        match (self, staged) {
            (Self::Bool(_), StagedValue::Bool(value)) => Self::Bool(value),
            (Self::Choice { len, edge, .. }, StagedValue::Choice(index)) => {
                Self::Choice { index, len, edge }
            }
            (value, _) => value,
        }
    }

    /// The one stepping function. A switch flips on Activate, turns on with
    /// Next and off with Previous. A choice moves one option, clamping or
    /// wrapping by its edge.
    #[must_use]
    pub fn stepped(self, step: Step) -> Self {
        match self {
            Self::Bool(value) => Self::Bool(match step {
                Step::Activate => !value,
                Step::Next => true,
                Step::Previous => false,
            }),
            Self::Choice { index, len, edge } => Self::Choice {
                index: step_index(index, len, step != Step::Previous, edge),
                len,
                edge,
            },
        }
    }

    /// True when `step` cannot move a clamped choice any further.
    #[must_use]
    pub fn is_clamped_at(self, step: Step) -> bool {
        matches!(
            self,
            Self::Choice {
                edge: StepEdge::Clamp,
                ..
            }
        ) && self.stepped(step) == self
    }
}

/// The steppable value of a daemon feature row; `None` for read-only rows and
/// action buttons.
#[must_use]
pub fn row_value_of_entry(entry: &DaemonFeatureEntry) -> Option<RowValue> {
    match &entry.value {
        DaemonFeatureValue::Bool(value) => Some(RowValue::Bool(*value)),
        DaemonFeatureValue::Cycle { options, current } if !options.is_empty() => {
            Some(RowValue::Choice {
                index: *current,
                len: options.len(),
                edge: edge_for_options(options),
            })
        }
        DaemonFeatureValue::Cycle { .. } | DaemonFeatureValue::Display(_) => None,
    }
}

/// A local TUI preference row that stages like a daemon row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalPref {
    TextAreaBackfill,
    FormulationAnim,
    FormulationSpeed,
    ActivityIndicator,
    DetailAlignment,
    SubmitOnEnter,
    AutoOpenQuestion,
    PromptCompiler,
    ShowSystemEvents,
    ShowThinkingEvents,
    HideToolResults,
    NavigatorPreset,
    NavigatorColumn(NavigatorOptionalColumn),
    CardField(CardField),
}

impl LocalPref {
    /// The preference on `section` row `idx`, resolved against `settings` (the
    /// navigator block's rows depend on the active preset's column order).
    #[must_use]
    pub fn at(settings: &UserSettings, section: SettingsSection, idx: usize) -> Option<Self> {
        match section {
            SettingsSection::Screen => match idx {
                0 => Some(Self::TextAreaBackfill),
                2 => Some(Self::FormulationAnim),
                3 => Some(Self::FormulationSpeed),
                4 => Some(Self::ActivityIndicator),
                5 => Some(Self::DetailAlignment),
                // Row 1 opens the background color editor.
                _ => None,
            },
            SettingsSection::InputPrompts => match idx {
                0 => Some(Self::SubmitOnEnter),
                1 => Some(Self::AutoOpenQuestion),
                2 => Some(Self::PromptCompiler),
                _ => None,
            },
            SettingsSection::TranscriptDefaults => match idx {
                0 => Some(Self::ShowSystemEvents),
                1 => Some(Self::ShowThinkingEvents),
                2 => Some(Self::HideToolResults),
                _ => None,
            },
            SettingsSection::SessionList => {
                if idx == 0 {
                    return Some(Self::NavigatorPreset);
                }
                if let Some(column) = settings.navigator_column_at_row(idx) {
                    return Some(Self::NavigatorColumn(column));
                }
                settings
                    .card_fields
                    .get(idx.checked_sub(NAVIGATOR_SETTINGS_ROW_COUNT)?)
                    .map(|entry| Self::CardField(entry.field))
            }
            _ => None,
        }
    }

    #[must_use]
    pub fn label(self) -> String {
        match self {
            Self::TextAreaBackfill => "Text area background".to_string(),
            Self::FormulationAnim => "Formulation animation".to_string(),
            Self::FormulationSpeed => "Formulation speed".to_string(),
            Self::ActivityIndicator => "Activity indicator".to_string(),
            Self::DetailAlignment => "Detail column".to_string(),
            Self::SubmitOnEnter => "Submit on Enter".to_string(),
            Self::AutoOpenQuestion => "Auto-open question panel".to_string(),
            Self::PromptCompiler => "Prompt compiler".to_string(),
            Self::ShowSystemEvents => "Show system events".to_string(),
            Self::ShowThinkingEvents => "Show thinking events".to_string(),
            Self::HideToolResults => "Hide tool results".to_string(),
            Self::NavigatorPreset => "Navigator preset".to_string(),
            Self::NavigatorColumn(column) => column.label().to_string(),
            Self::CardField(field) => field.label().to_string(),
        }
    }

    /// The live value of this preference in `settings`.
    #[must_use]
    pub fn read(self, settings: &UserSettings) -> RowValue {
        let choice = |index: Option<usize>, len: usize, edge: StepEdge| RowValue::Choice {
            index: index.unwrap_or(len),
            len,
            edge,
        };
        match self {
            Self::TextAreaBackfill => RowValue::Bool(settings.text_area_backfill_enabled),
            Self::FormulationAnim => RowValue::Bool(settings.formulation_anim_enabled),
            Self::FormulationSpeed => choice(
                FORMULATION_ANIM_DURATIONS
                    .iter()
                    .position(|ms| *ms == settings.formulation_anim_ms),
                FORMULATION_ANIM_DURATIONS.len(),
                StepEdge::Clamp,
            ),
            Self::ActivityIndicator => choice(
                ACTIVITY_STYLES
                    .iter()
                    .position(|style| *style == settings.activity_indicator_style),
                ACTIVITY_STYLES.len(),
                StepEdge::Wrap,
            ),
            Self::DetailAlignment => choice(
                DETAIL_ALIGNMENTS
                    .iter()
                    .position(|alignment| *alignment == settings.detail_column_alignment),
                DETAIL_ALIGNMENTS.len(),
                StepEdge::Wrap,
            ),
            Self::SubmitOnEnter => RowValue::Bool(settings.submit_on_enter),
            Self::AutoOpenQuestion => RowValue::Bool(settings.auto_open_question_modal),
            Self::PromptCompiler => RowValue::Bool(settings.prompt_processor.enabled),
            Self::ShowSystemEvents => RowValue::Bool(settings.default_show_system_events),
            Self::ShowThinkingEvents => RowValue::Bool(settings.default_show_thinking_events),
            Self::HideToolResults => RowValue::Bool(settings.default_hide_tool_results),
            Self::NavigatorPreset => choice(
                NavigatorPreset::ALL
                    .iter()
                    .position(|preset| *preset == settings.navigator_preset),
                NavigatorPreset::ALL.len(),
                StepEdge::Wrap,
            ),
            Self::NavigatorColumn(column) => {
                RowValue::Bool(navigator_column_enabled(settings, column))
            }
            Self::CardField(field) => RowValue::Bool(settings.is_card_field_enabled(field)),
        }
    }

    /// Write `value` into `settings`. A value of the wrong kind or an option
    /// index past the end changes nothing.
    pub fn write(self, settings: &mut UserSettings, value: StagedValue) {
        match (self, value) {
            (Self::TextAreaBackfill, StagedValue::Bool(on)) => {
                settings.text_area_backfill_enabled = on;
            }
            (Self::FormulationAnim, StagedValue::Bool(on)) => {
                settings.formulation_anim_enabled = on
            }
            (Self::FormulationSpeed, StagedValue::Choice(index)) => {
                if let Some(ms) = FORMULATION_ANIM_DURATIONS.get(index) {
                    settings.formulation_anim_ms = *ms;
                }
            }
            (Self::ActivityIndicator, StagedValue::Choice(index)) => {
                if let Some(style) = ACTIVITY_STYLES.get(index) {
                    settings.activity_indicator_style = *style;
                }
            }
            (Self::DetailAlignment, StagedValue::Choice(index)) => {
                if let Some(alignment) = DETAIL_ALIGNMENTS.get(index) {
                    settings.detail_column_alignment = *alignment;
                }
            }
            (Self::SubmitOnEnter, StagedValue::Bool(on)) => settings.submit_on_enter = on,
            (Self::AutoOpenQuestion, StagedValue::Bool(on)) => {
                settings.auto_open_question_modal = on;
            }
            (Self::PromptCompiler, StagedValue::Bool(on)) => settings.prompt_processor.enabled = on,
            (Self::ShowSystemEvents, StagedValue::Bool(on)) => {
                settings.default_show_system_events = on;
            }
            (Self::ShowThinkingEvents, StagedValue::Bool(on)) => {
                settings.default_show_thinking_events = on;
            }
            (Self::HideToolResults, StagedValue::Bool(on)) => {
                settings.default_hide_tool_results = on;
            }
            (Self::NavigatorPreset, StagedValue::Choice(index)) => {
                if let Some(preset) = NavigatorPreset::ALL.get(index)
                    && *preset != settings.navigator_preset
                {
                    settings.navigator_preset = *preset;
                    // Choosing a preset resets the advanced column overrides
                    // to that preset's declared columns.
                    settings.navigator_optional_columns = None;
                }
            }
            (Self::NavigatorColumn(column), StagedValue::Bool(on)) => {
                if navigator_column_enabled(settings, column) != on {
                    settings.toggle_navigator_optional_column(column);
                }
            }
            (Self::CardField(field), StagedValue::Bool(on)) => {
                if let Some(entry) = settings
                    .card_fields
                    .iter_mut()
                    .find(|entry| entry.field == field)
                {
                    entry.enabled = on;
                }
            }
            _ => {}
        }
    }
}

fn navigator_column_enabled(settings: &UserSettings, column: NavigatorOptionalColumn) -> bool {
    settings.navigator_optional_columns.as_ref().map_or_else(
        || crate::ui::navigator_layout::preset_columns(settings.navigator_preset).contains(&column),
        |columns| columns.contains(&column),
    )
}

/// Every staged edit of the Settings panel. Lives in `SettingsState`, so
/// leaving the panel drops it.
#[derive(Debug, Clone, Default)]
pub struct PendingSettings {
    daemon: Vec<(String, StagedValue)>,
    local: Vec<(LocalPref, StagedValue)>,
}

impl PendingSettings {
    pub fn clear(&mut self) {
        self.daemon.clear();
        self.local.clear();
    }

    /// True when nothing is staged (it may still hold values that equal the
    /// live ones; [`Self::count`] counts only real differences).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.daemon.is_empty() && self.local.is_empty()
    }

    #[must_use]
    pub fn daemon_staged(&self, field: &str) -> Option<StagedValue> {
        self.daemon
            .iter()
            .find(|(staged_field, _)| staged_field == field)
            .map(|(_, value)| *value)
    }

    #[must_use]
    pub fn local_staged(&self, pref: LocalPref) -> Option<StagedValue> {
        self.local
            .iter()
            .find(|(staged, _)| *staged == pref)
            .map(|(_, value)| *value)
    }

    /// `entry` as the panel shows it: the staged value when one is pending.
    #[must_use]
    pub fn entry_view(&self, entry: &DaemonFeatureEntry) -> DaemonFeatureEntry {
        let mut view = entry.clone();
        if let Some(staged) = self.daemon_staged(&entry.field) {
            match (&mut view.value, staged) {
                (DaemonFeatureValue::Bool(value), StagedValue::Bool(on)) => *value = on,
                (DaemonFeatureValue::Cycle { options, current }, StagedValue::Choice(index))
                    if index < options.len() =>
                {
                    *current = index;
                }
                _ => {}
            }
        }
        view
    }

    /// True when `entry` has a staged value that differs from its live one.
    #[must_use]
    pub fn daemon_dirty(&self, entry: &DaemonFeatureEntry) -> bool {
        let Some(staged) = self.daemon_staged(&entry.field) else {
            return false;
        };
        row_value_of_entry(entry).is_some_and(|live| live.staged() != staged)
    }

    /// Stage `value` for a daemon row whose live value is `live`; a value equal
    /// to the live one unstages the row.
    pub fn stage_daemon(&mut self, field: &str, live: StagedValue, value: StagedValue) {
        self.daemon
            .retain(|(staged_field, _)| staged_field != field);
        if value != live {
            self.daemon.push((field.to_string(), value));
        }
    }

    /// Live settings with a staged navigator preset applied: the baseline the
    /// navigator column and card rows are compared against.
    fn baseline_settings<'a>(&self, real: &'a UserSettings) -> Cow<'a, UserSettings> {
        match self.local_staged(LocalPref::NavigatorPreset) {
            Some(staged) => {
                let mut settings = real.clone();
                LocalPref::NavigatorPreset.write(&mut settings, staged);
                Cow::Owned(settings)
            }
            None => Cow::Borrowed(real),
        }
    }

    fn local_baseline(&self, pref: LocalPref, real: &UserSettings) -> StagedValue {
        if pref == LocalPref::NavigatorPreset {
            pref.read(real).staged()
        } else {
            pref.read(&self.baseline_settings(real)).staged()
        }
    }

    /// True when `pref` has a staged value that differs from the live one.
    #[must_use]
    pub fn local_dirty(&self, pref: LocalPref, real: &UserSettings) -> bool {
        self.local_staged(pref)
            .is_some_and(|staged| staged != self.local_baseline(pref, real))
    }

    /// Stage `value` for a local preference; a value equal to the live one
    /// unstages it. Choosing a navigator preset drops staged column edits, as
    /// choosing it always reset them.
    pub fn stage_local(&mut self, pref: LocalPref, value: StagedValue, real: &UserSettings) {
        if pref == LocalPref::NavigatorPreset {
            self.local
                .retain(|(staged, _)| !matches!(staged, LocalPref::NavigatorColumn(_)));
        }
        let live = self.local_baseline(pref, real);
        self.local.retain(|(staged, _)| *staged != pref);
        if value != live {
            self.local.push((pref, value));
        }
    }

    /// `real` with every staged local preference applied: what the local rows
    /// show while edits are pending.
    #[must_use]
    pub fn view_settings<'a>(&self, real: &'a UserSettings) -> Cow<'a, UserSettings> {
        if self.local.is_empty() {
            return Cow::Borrowed(real);
        }
        let mut settings = real.clone();
        for (pref, value) in self.ordered_local() {
            pref.write(&mut settings, value);
        }
        Cow::Owned(settings)
    }

    /// Staged local edits in apply order: the navigator preset first, since it
    /// resets the column overrides the other edits build on.
    fn ordered_local(&self) -> Vec<(LocalPref, StagedValue)> {
        let mut ordered = self.local.clone();
        ordered.sort_by_key(|(pref, _)| usize::from(*pref != LocalPref::NavigatorPreset));
        ordered
    }

    /// Number of rows whose staged value differs from the live one.
    #[must_use]
    pub fn count(&self, features: &[DaemonFeatureEntry], real: &UserSettings) -> usize {
        let daemon = self
            .daemon
            .iter()
            .filter(|(field, staged)| {
                features
                    .iter()
                    .find(|entry| &entry.field == field)
                    .and_then(row_value_of_entry)
                    .is_some_and(|live| live.staged() != *staged)
            })
            .count();
        let local = self
            .local
            .iter()
            .filter(|(pref, _)| self.local_dirty(*pref, real))
            .count();
        daemon + local
    }

    /// Daemon rows to send on apply: `(index into features, staged value)` for
    /// every staged row that differs from its live value, in row order.
    #[must_use]
    pub fn daemon_changes(&self, features: &[DaemonFeatureEntry]) -> Vec<(usize, StagedValue)> {
        let mut changes: Vec<(usize, StagedValue)> = self
            .daemon
            .iter()
            .filter_map(|(field, staged)| {
                let index = features.iter().position(|entry| &entry.field == field)?;
                let live = row_value_of_entry(&features[index])?;
                (live.staged() != *staged).then_some((index, *staged))
            })
            .collect();
        changes.sort_by_key(|(index, _)| *index);
        changes
    }

    /// Forget one daemon row once the daemon accepted it.
    pub fn settle_daemon(&mut self, field: &str) {
        self.daemon
            .retain(|(staged_field, _)| staged_field != field);
    }

    /// Drop every staged value that now equals its live one.
    pub fn prune_settled_daemon(&mut self, features: &[DaemonFeatureEntry]) {
        self.daemon.retain(|(field, staged)| {
            features
                .iter()
                .find(|entry| &entry.field == field)
                .and_then(row_value_of_entry)
                .is_some_and(|live| live.staged() != *staged)
        });
    }

    /// Take the staged local edits that differ from the live settings, in apply
    /// order, and forget all local staging.
    pub fn take_local_changes(&mut self, real: &UserSettings) -> Vec<(LocalPref, StagedValue)> {
        let changes: Vec<(LocalPref, StagedValue)> = self
            .ordered_local()
            .into_iter()
            .filter(|(pref, _)| self.local_dirty(*pref, real))
            .collect();
        self.local.clear();
        changes
    }
}

/// True when the registry row `spec` stages its edit and takes effect when the
/// page is applied (`w`), rather than acting the moment it is used. Daemon rows
/// stage when the page holds a switch or choice for them; TUI-owned rows stage
/// when the registry marks them [`SettingApply::OnApply`] (the local
/// preferences of [`LocalPref`]). A test pins this against the staging code, so
/// the labels the page shows ("on apply (w)") cannot drift from what it does.
#[must_use]
pub fn stages_edit(spec: &SettingSpec) -> bool {
    match spec.kind {
        SettingKind::Bool | SettingKind::Cycle | SettingKind::Edit => match spec.owner {
            SettingOwner::Daemon(_) | SettingOwner::DaemonState(_) => {
                crate::settings_keys::daemon_feature_field_for(spec.id).is_some()
            }
            SettingOwner::Tui => spec.apply == SettingApply::OnApply,
            SettingOwner::DaemonFields(_) | SettingOwner::File(_) => false,
        },
        SettingKind::DynamicList => spec.apply == SettingApply::OnApply,
        SettingKind::Action | SettingKind::ReadOnly => false,
    }
}

/// Pending rows in the open Settings panel.
#[must_use]
pub fn pending_count(app: &App) -> usize {
    app.settings_state
        .pending
        .count(&app.daemon_features, &app.settings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::DaemonFeatureEntry;

    fn numeric(options: &[&str], current: usize) -> DaemonFeatureEntry {
        DaemonFeatureEntry {
            field: "sandbox_max_source_roots".to_string(),
            label: "Maximum sandbox roots".to_string(),
            value: DaemonFeatureValue::Cycle {
                options: options.iter().map(|option| (*option).to_string()).collect(),
                current,
            },
        }
    }

    fn named(options: &[&str], current: usize) -> DaemonFeatureEntry {
        DaemonFeatureEntry {
            field: "model_control_mode".to_string(),
            label: "Model control mode".to_string(),
            value: DaemonFeatureValue::Cycle {
                options: options.iter().map(|option| (*option).to_string()).collect(),
                current,
            },
        }
    }

    fn step_n(mut value: RowValue, step: Step, times: usize) -> RowValue {
        for _ in 0..times {
            value = value.stepped(step);
        }
        value
    }

    #[test]
    fn numeric_rows_step_down_and_clamp_at_both_ends() {
        let entry = numeric(&["512", "1024", "2048"], 1);
        let value = row_value_of_entry(&entry).expect("cycle row");
        assert_eq!(
            value.stepped(Step::Previous).staged(),
            StagedValue::Choice(0)
        );
        assert_eq!(
            step_n(value, Step::Previous, 5).staged(),
            StagedValue::Choice(0),
            "the minimum holds; it never wraps to the maximum"
        );
        assert_eq!(
            step_n(value, Step::Next, 5).staged(),
            StagedValue::Choice(2),
            "the maximum holds; it never wraps back to 512"
        );
        assert_eq!(
            step_n(value, Step::Activate, 5).staged(),
            StagedValue::Choice(2),
            "Enter is the forward step and clamps like +"
        );
        assert!(step_n(value, Step::Next, 5).is_clamped_at(Step::Next));
        assert!(!value.is_clamped_at(Step::Next));
    }

    #[test]
    fn a_default_sentinel_does_not_make_a_numeric_row_wrap() {
        let mut options = vec!["Default".to_string()];
        options.extend((1..=99).map(|pct| pct.to_string()));
        assert_eq!(edge_for_options(&options), StepEdge::Clamp);
        let value = RowValue::Choice {
            index: 0,
            len: options.len(),
            edge: edge_for_options(&options),
        };
        assert_eq!(value.stepped(Step::Previous), value);
    }

    #[test]
    fn enum_rows_step_backwards_and_wrap_both_ways() {
        let entry = named(&["normal", "pause-background", "deny-paid"], 0);
        let value = row_value_of_entry(&entry).expect("cycle row");
        assert_eq!(
            value.stepped(Step::Previous).staged(),
            StagedValue::Choice(2),
            "backwards from the first option wraps to the last"
        );
        assert_eq!(
            step_n(value, Step::Next, 3).staged(),
            StagedValue::Choice(0),
            "forwards from the last option wraps to the first"
        );
        assert!(!step_n(value, Step::Next, 2).is_clamped_at(Step::Next));
    }

    #[test]
    fn switches_flip_on_activate_and_set_on_next_off_previous() {
        let off = RowValue::Bool(false);
        let on = RowValue::Bool(true);
        assert_eq!(off.stepped(Step::Activate), on);
        assert_eq!(on.stepped(Step::Activate), off);
        assert_eq!(off.stepped(Step::Next), on);
        assert_eq!(on.stepped(Step::Next), on);
        assert_eq!(on.stepped(Step::Previous), off);
        assert_eq!(off.stepped(Step::Previous), off);
    }

    #[test]
    fn step_index_enters_an_unknown_value_at_the_matching_end() {
        assert_eq!(step_index(9, 3, true, StepEdge::Clamp), 0);
        assert_eq!(step_index(9, 3, false, StepEdge::Wrap), 2);
        assert_eq!(step_index(0, 0, true, StepEdge::Wrap), 0);
    }

    #[test]
    fn staging_back_to_the_live_value_unstages_the_row() {
        let entry = numeric(&["512", "1024", "2048"], 1);
        let live = row_value_of_entry(&entry).expect("cycle row");
        let mut pending = PendingSettings::default();
        let up = live.stepped(Step::Next);
        pending.stage_daemon(&entry.field, live.staged(), up.staged());
        assert!(pending.daemon_dirty(&entry));
        assert_eq!(
            pending.count(std::slice::from_ref(&entry), &UserSettings::default()),
            1
        );
        pending.stage_daemon(
            &entry.field,
            live.staged(),
            up.stepped(Step::Previous).staged(),
        );
        assert!(!pending.daemon_dirty(&entry));
        assert!(pending.is_empty());
    }

    #[test]
    fn the_view_shows_the_staged_value_and_the_live_row_is_untouched() {
        let entry = numeric(&["512", "1024", "2048"], 1);
        let live = row_value_of_entry(&entry).expect("cycle row");
        let mut pending = PendingSettings::default();
        pending.stage_daemon(&entry.field, live.staged(), StagedValue::Choice(2));
        let view = pending.entry_view(&entry);
        assert!(matches!(
            view.value,
            DaemonFeatureValue::Cycle { current: 2, .. }
        ));
        assert!(matches!(
            entry.value,
            DaemonFeatureValue::Cycle { current: 1, .. }
        ));
    }

    #[test]
    fn a_refresh_that_lands_on_the_staged_value_settles_it() {
        let mut features = vec![numeric(&["512", "1024", "2048"], 1)];
        let mut pending = PendingSettings::default();
        pending.stage_daemon(
            "sandbox_max_source_roots",
            StagedValue::Choice(1),
            StagedValue::Choice(2),
        );
        assert_eq!(pending.count(&features, &UserSettings::default()), 1);
        features[0].value = DaemonFeatureValue::Cycle {
            options: vec!["512".into(), "1024".into(), "2048".into()],
            current: 2,
        };
        assert_eq!(pending.count(&features, &UserSettings::default()), 0);
        assert!(pending.daemon_changes(&features).is_empty());
        pending.prune_settled_daemon(&features);
        assert!(pending.is_empty());
    }

    #[test]
    fn local_prefs_stage_without_touching_the_live_settings() {
        let live = UserSettings::default();
        let mut pending = PendingSettings::default();
        let pref = LocalPref::SubmitOnEnter;
        let before = pref.read(&live);
        pending.stage_local(pref, before.stepped(Step::Activate).staged(), &live);
        assert!(pending.local_dirty(pref, &live));
        assert_eq!(pref.read(&live), before, "the live setting is unchanged");
        assert_ne!(pref.read(&pending.view_settings(&live)), before);
        assert_eq!(pending.count(&[], &live), 1);
        pending.stage_local(pref, before.staged(), &live);
        assert_eq!(pending.count(&[], &live), 0);
    }

    #[test]
    fn local_numeric_pref_clamps_and_enum_pref_wraps() {
        let mut live = UserSettings::default();
        live.formulation_anim_ms = FORMULATION_ANIM_DURATIONS[0];
        let speed = LocalPref::FormulationSpeed.read(&live);
        assert_eq!(
            speed.stepped(Step::Previous),
            speed,
            "formulation speed clamps"
        );
        live.activity_indicator_style = ACTIVITY_STYLES[0];
        let style = LocalPref::ActivityIndicator.read(&live);
        assert_eq!(
            style.stepped(Step::Previous).staged(),
            StagedValue::Choice(ACTIVITY_STYLES.len() - 1),
            "activity indicator wraps"
        );
    }

    /// The registry's claim that a row stages (and so shows "on apply (w)")
    /// is exactly what the staging code does with it.
    #[test]
    fn registry_rows_agree_with_the_staging_code() {
        use crate::settings_registry::{SETTINGS, SettingId};

        let entries = DaemonFeatureEntry::defaults();
        let settings = UserSettings::default();
        for spec in SETTINGS {
            if let Some(field) = crate::settings_keys::daemon_feature_field_for(spec.id) {
                let steppable = entries
                    .iter()
                    .find(|entry| entry.field == field)
                    .and_then(row_value_of_entry)
                    .is_some();
                assert_eq!(
                    stages_edit(spec),
                    steppable,
                    "{:?} ({field}): the registry and the page agree it stages",
                    spec.id
                );
            }
        }
        // TUI-owned preference rows: the first UI row of each registry row.
        for section in [
            SettingsSection::Screen,
            SettingsSection::InputPrompts,
            SettingsSection::TranscriptDefaults,
        ] {
            for (position, spec) in SETTINGS
                .iter()
                .filter(|spec| spec.section == section)
                .enumerate()
            {
                assert_eq!(
                    stages_edit(spec),
                    LocalPref::at(&settings, section, position).is_some(),
                    "{:?}: the registry and LocalPref agree it stages",
                    spec.id
                );
            }
        }
        for (id, ui_row) in [
            (SettingId::NavigatorPreset, 0),
            (SettingId::NavigatorColumns, 1),
            (SettingId::CardFields, NAVIGATOR_SETTINGS_ROW_COUNT),
        ] {
            let Some(spec) = SETTINGS.iter().find(|spec| spec.id == id) else {
                panic!("{id:?} is a registry row");
            };
            assert!(stages_edit(spec), "{id:?} stages");
            assert!(
                LocalPref::at(&settings, SettingsSection::SessionList, ui_row).is_some(),
                "{id:?} is a local preference"
            );
        }
        // An action runs when activated; it never stages.
        let Some(stop) = SETTINGS
            .iter()
            .find(|spec| spec.id == SettingId::EmergencyStop)
        else {
            panic!("emergency stop is a registry row");
        };
        assert!(!stages_edit(stop));
        assert_eq!(stop.apply_label(), "when activated");
    }

    #[test]
    fn choosing_a_navigator_preset_drops_staged_column_edits() {
        let live = UserSettings::default();
        let mut pending = PendingSettings::default();
        let column = LocalPref::NavigatorColumn(NavigatorOptionalColumn::Retry);
        let enabled = column.read(&live);
        pending.stage_local(column, enabled.stepped(Step::Activate).staged(), &live);
        assert_eq!(pending.count(&[], &live), 1);
        let preset = LocalPref::NavigatorPreset;
        let next = preset.read(&live).stepped(Step::Next).staged();
        pending.stage_local(preset, next, &live);
        assert_eq!(pending.local_staged(column), None);
        let changes = pending.take_local_changes(&live);
        assert_eq!(changes, vec![(preset, next)]);
        assert!(pending.is_empty());
    }
}
