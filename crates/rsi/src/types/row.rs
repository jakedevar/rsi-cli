//! Shared session list row view model (SVR-004).
//!
//! Pure data contract that backs every session list row across the Main,
//! TaskRabbit, Archive, and Jobs zones. SVR-005 (full list) and SVR-006
//! (sidebar list) consume this directly.
//!
//! Cells use small sum types whose `Missing` / `Unassigned` variants encode
//! the "no data, render em-dash" sentinel. Renderers read the cell once and
//! never have to ask the VM again whether data is present.
//!
//! `compute_session_row(app, session_id, settings)` is settings-aware
//! (applies `UserSettings::is_card_field_enabled` gating) and returns an
//! owned VM (not `Option<...>`). On unknown session id, returns
//! `SessionRowViewModel::placeholder(session_id)` so call sites consume a
//! single shape.
//!
//! Wall-clock dependency: `compute_session_row` reads `chrono::Utc::now()`
//! through the promoted `ui::session::compute_time_display` and
//! `compute_heat_color` helpers.
//!
//! Em-dash convention `"—"` styled `theme::overlay0()` for missing data is
//! owned by the renderer, not encoded in the VM (matches SVR-003).

use std::collections::HashMap;

use ratatui::style::Color;
use rsi_common::types::{Project, Session, SessionKind, SessionStatus};
use uuid::Uuid;

use crate::app::App;
use crate::settings::UserSettings;
use crate::types::{
    CardField, SessionFocusEntry, SessionState, compute_context_budget_view,
    compute_session_focus_index, resolve_session_display_identity,
};
use crate::ui::content;
use crate::ui::session::{
    compute_heat_color, compute_time_display, created_text, effort_bar_counts, floor_char_boundary,
    format_work_time_ms, kind_color, short_model_label, status_color, status_icon,
};
use crate::ui::theme;

// ---------------------------------------------------------------------------
// Cell sum types
// ---------------------------------------------------------------------------
//
// `PartialEq` is derived on every cell so tests can assert structural
// equality. `Eq` is intentionally NOT derived — `Color::Rgb(...)` is
// `PartialEq` only on its struct member; same constraint SVR-003's deck
// types cite for `f64`.

/// `MODEL` cell — abbreviated long-form model id paired with the short
/// label and the effort-bar counts. `Missing` is the canonical em-dash signal.
#[derive(Debug, Clone, PartialEq)]
pub enum ModelCell {
    Long {
        long: String,
        short: String,
        effort_total: usize,
        effort_filled: usize,
        /// Exact supported-ladder label resolved from the source model.
        /// Counts alone cannot distinguish Codex's four-level `xhigh` from
        /// Claude's four-level `max`.
        effort_label: String,
    },
    Missing,
}

/// `PROJECT` cell — `App.projects` lookup result. `Unassigned` is the
/// "no project_id on the session" sentinel; SVR-005's renderer draws an
/// em-dash for that state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectCell {
    Named(String),
    Unassigned,
}

/// `CTX` cell — daemon-pushed percentage plus compact usage/provenance markers.
#[derive(Debug, Clone, PartialEq)]
pub enum ContextCell {
    Display { label: String, pct: Option<f64> },
    Missing,
}

/// `COST` cell — three-way split: `Amount` carries the formatted string
/// (`"$0.42"`), `BelowThreshold` represents a non-zero cost below the
/// $0.01 render threshold (preserves the "we have a cost but it's tiny"
/// signal without forcing the renderer to re-check), `Missing` is the
/// daemon-side `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CostCell {
    Amount(String),
    BelowThreshold,
    Missing,
}

/// `TIME` cell — pre-formatted text and color from
/// `compute_time_display`. `Missing` is the placeholder sentinel; the
/// computation path always emits `Value`.
#[derive(Debug, Clone, PartialEq)]
pub enum TimeCell {
    Value { text: String, color: Color },
    Missing,
}

// ---------------------------------------------------------------------------
// View model
// ---------------------------------------------------------------------------

/// Running work inside a Group or Epic, computed by the memoized focus index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContainerCounts {
    /// Descendant Epics with at least one Starting or Running leaf.
    pub running_epics: usize,
    /// Starting or Running leaf sessions in the full subtree.
    pub running_agents: usize,
}

/// Pure data contract backing one session list row across every zone
/// (Main / TaskRabbit / Archive / Jobs — uniform shape).
///
/// Fields are grouped by display order: identity, status, title,
/// metadata columns, expanded-only body, then flags. Renderers read each
/// field exactly once.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionRowViewModel {
    // ---- Identity ----
    pub session_id: Uuid,
    pub session_kind: SessionKind,
    pub effective_epic_ordinal: Option<u32>,
    /// Set by the session-list renderer from the daemon-resolved manager roster.
    pub is_manager: bool,

    // ---- Status ----
    pub status: SessionStatus,
    pub status_icon: String,
    pub status_color: Color,
    /// Independent one-cell navigator attention signal and inspector reasons.
    pub attention_glyph: &'static str,
    pub attention_reasons: Vec<String>,

    // ---- Title group ----
    pub display_title: String,
    pub rotation_suffix: String,
    pub issue_identifier: Option<String>,
    pub rating: Option<i16>,

    // ---- Metadata columns ----
    pub model: ModelCell,
    pub project_name: ProjectCell,
    pub context_pct: ContextCell,
    pub cost: CostCell,
    pub turns_text: Option<String>,
    pub time: TimeCell,
    pub heat_color: Color,

    // ---- New metadata columns (TD1 work-time, created timestamp) ----
    /// TD1 accumulated active-work time, pre-formatted ("2h15m"/"45m"/"<1m").
    /// `None` when the setting is off OR `session.work_time_ms` is absent
    /// (pre-V70 row, or a Group/Epic container that never ran a subprocess —
    /// same Missing-collapse convention as every other optional cell).
    pub work_time_text: Option<String>,
    /// Absolute `created_at`, pre-formatted ("%m-%d %H:%M", local time).
    /// `None` only when the `CardField` setting is off (data is never absent —
    /// `created_at` is a required, non-Option `Session` field).
    pub created_text: Option<String>,

    // ---- Container rows (Group/Epic only) ----
    /// Running subtree totals for Group/Epic rows; absent for leaf rows.
    pub container_counts: Option<ContainerCounts>,

    // ---- New-message signal (D2) ----
    /// `state.events_generation > state.last_seen_events_generation` at
    /// computation time. Unconditional (not CardField-gated) — same
    /// treatment as `is_stalled`/`is_pending_archive`.
    pub is_new_message: bool,

    // ---- Expanded body ----
    pub description: Option<String>,
    pub short_summary: Option<String>,
    pub docregblock_label: Option<String>,
    pub kind_pill: Option<String>,
    pub kind_color: Option<Color>,
    pub has_pills: bool,
    pub is_expanded: bool,

    // ---- Boolean / counter flags ----
    pub is_pinned: bool,
    pub is_testing_needed: bool,
    pub is_rotation_disabled: bool,
    pub is_sandboxed: bool,
    pub is_lead: bool,
    pub is_pending_archive: bool,
    pub is_stalled: bool,
    pub retry_info: Option<(u8, u8)>,
}

impl SessionRowViewModel {
    /// The all-default shape used when a session id resolves to nothing.
    /// Cheap to construct — empty strings + sentinel cell variants.
    /// `session_id` is preserved (not `Uuid::nil()`) so hit-test still
    /// works on the placeholder row.
    pub fn placeholder(session_id: Uuid) -> Self {
        Self {
            session_id,
            session_kind: SessionKind::Standard,
            effective_epic_ordinal: None,
            is_manager: false,
            status: SessionStatus::Starting,
            status_icon: String::new(),
            status_color: theme::overlay0(),
            attention_glyph: "",
            attention_reasons: Vec::new(),
            display_title: String::new(),
            rotation_suffix: String::new(),
            issue_identifier: None,
            rating: None,
            model: ModelCell::Missing,
            project_name: ProjectCell::Unassigned,
            context_pct: ContextCell::Missing,
            cost: CostCell::Missing,
            turns_text: None,
            time: TimeCell::Missing,
            heat_color: theme::text(),
            work_time_text: None,
            created_text: None,
            container_counts: None,
            is_new_message: false,
            description: None,
            short_summary: None,
            docregblock_label: None,
            kind_pill: None,
            kind_color: None,
            has_pills: false,
            is_expanded: false,
            is_pinned: false,
            is_testing_needed: false,
            is_rotation_disabled: false,
            is_sandboxed: false,
            is_lead: false,
            is_pending_archive: false,
            is_stalled: false,
            retry_info: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Computation
// ---------------------------------------------------------------------------

/// Build the session row view model for `session_id` against the
/// current `App` snapshot, applying user-settings gating for every
/// `CardField`-tagged field. Pure: borrows `app` immutably, returns an
/// owned VM, mutates nothing.
///
/// When the session id misses, returns
/// `SessionRowViewModel::placeholder(session_id)` (D6) so call sites
/// always see a single shape.
pub fn compute_session_row(
    app: &App,
    session_id: Uuid,
    settings: &UserSettings,
) -> SessionRowViewModel {
    compute_session_row_parts(&app.sessions, &app.projects, session_id, settings)
}

/// Field-disjoint variant of `compute_session_row` — accepts the immutable App
/// fields it actually reads. Renderers use this when they already hold a
/// narrow mutable borrow on list geometry.
pub fn compute_session_row_parts(
    sessions: &HashMap<Uuid, SessionState>,
    projects: &[Project],
    session_id: Uuid,
    settings: &UserSettings,
) -> SessionRowViewModel {
    let Some(state) = sessions.get(&session_id) else {
        return SessionRowViewModel::placeholder(session_id);
    };
    compute_session_row_for_state(state, sessions, projects, settings)
}

/// State-anchored variant that builds a VM directly from a
/// `SessionState` reference, regardless of whether the session id is
/// present in the `sessions` map. Tests use this to build a `SessionState`
/// in-place without inserting it into a `HashMap`.
///
/// `sessions` is still consulted for the rotation-chain walk and the
/// hierarchical-parent lead lookup; missing parent ids resolve to "not
/// lead" / no extra rotation depth.
pub fn compute_session_row_for_state(
    state: &SessionState,
    sessions: &HashMap<Uuid, SessionState>,
    projects: &[Project],
    settings: &UserSettings,
) -> SessionRowViewModel {
    let focus_index = compute_session_focus_index(sessions, chrono::Utc::now());
    compute_session_row_for_state_with_focus(
        state,
        sessions,
        projects,
        settings,
        focus_index.get(&state.session.id),
    )
}

/// Focus-aware variant of [`compute_session_row_for_state`]. List renderers
/// pass their already-cached hierarchy projection so Group/Epic rows can show
/// scoped activity without walking the hierarchy once per rendered row.
pub fn compute_session_row_for_state_with_focus(
    state: &SessionState,
    sessions: &HashMap<Uuid, SessionState>,
    projects: &[Project],
    settings: &UserSettings,
    focus: Option<&SessionFocusEntry>,
) -> SessionRowViewModel {
    let session = &state.session;
    let is_active = matches!(
        session.status,
        SessionStatus::Running | SessionStatus::Starting
    );

    // ---- Lead derivation (one walk shared with the kind-pill / kind-color paths) ----
    let is_lead = compute_is_lead(session, sessions);

    // A Group/Epic with a Starting/Running leaf descendant shows ◉ in the
    // running color. This outranks its own lifecycle glyph, pending-archive
    // overlay color, and stalled color; the attention cell remains separate.
    let contains_running_work =
        matches!(session.session_kind, SessionKind::Group | SessionKind::Epic)
            && focus.is_some_and(|entry| entry.running_agent_count > 0);
    let status_icon_str = if contains_running_work {
        "◉"
    } else {
        navigator_lifecycle_icon(session.status)
    }
    .to_string();

    // ---- Status color (container running → pending archive → stalled → status) ----
    let status_color_value = if contains_running_work {
        theme::status_running()
    } else if session.pending_archive && is_active {
        theme::overlay1()
    } else if state.is_stalled {
        theme::status_stalled()
    } else {
        status_color(session.status)
    };

    // ---- Title + rotation depth ----
    let display_identity = resolve_session_display_identity(session, sessions);
    let display_title = display_identity.effective_title;
    let rotation_count = display_identity.rotation_depth;
    // Session-list rows are single-line cells. Ratatui does not reserve a
    // visual cell for embedded newlines in a `Span`, so normalize all title
    // whitespace before it reaches any list presentation. In particular,
    // `"first\nsecond"` must display as `"first second"`, not `"firstsecond"`.
    let display_title = display_title
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let rotation_suffix =
        if settings.is_card_field_enabled(CardField::RotationSuffix) && rotation_count > 0 {
            format!(" \u{21BB}{}", rotation_count)
        } else {
            String::new()
        };

    // ---- Time ----
    let (time_text, time_color) = compute_time_display(session);
    let time = TimeCell::Value {
        text: time_text,
        color: time_color,
    };

    // ---- Heat color ----
    let heat_color = if settings.is_card_field_enabled(CardField::HeatColor) {
        compute_heat_color(session.updated_at)
    } else {
        theme::text()
    };

    // ---- Context pct ----
    let context = compute_context_budget_view(state);
    let context_pct = match context.compact_label() {
        Some(label) => ContextCell::Display {
            label,
            pct: context.percent,
        },
        None => ContextCell::Missing,
    };

    // ---- Cost ----
    let cost = if settings.is_card_field_enabled(CardField::Cost) {
        match session.cost_usd {
            None => CostCell::Missing,
            Some(c) if c < 0.01 => CostCell::BelowThreshold,
            Some(c) => CostCell::Amount(format!("${:.2}", c)),
        }
    } else {
        CostCell::Missing
    };

    // ---- Turns ----
    // The navigator's `⇄` column header names the unit, so the value is bare.
    let turns_text = session.num_turns.map(|t| t.to_string());

    // ---- Description (gated by both setting and expanded state) ----
    let description =
        if state.list_card_expanded && settings.is_card_field_enabled(CardField::Description) {
            let raw = state
                .events
                .iter()
                .find(|e| {
                    e.event_type == rsi_common::types::EventType::Message
                        && e.role == Some(rsi_common::types::Role::User)
                })
                .map(|e| e.content.as_str())
                .unwrap_or(session.query.as_str());
            Some(truncate_description(raw, 200))
        } else {
            None
        };

    // ---- Short summary (gated only by expanded state) ----
    let short_summary = if state.list_card_expanded {
        session.short_summary.clone()
    } else {
        None
    };

    // ---- Docregblock label ----
    let docregblock_label =
        if state.list_card_expanded && settings.is_card_field_enabled(CardField::DocregblockPill) {
            content::docregblock_pill_label(&state.docregblock_contents)
        } else {
            None
        };

    // ---- Kind pill (with optional " ★" suffix when lead) ----
    let kind_pill =
        if state.list_card_expanded && settings.is_card_field_enabled(CardField::KindPill) {
            kind_pill_label(session.session_kind).map(|base| {
                if is_lead {
                    format!("{} \u{2605}", base) // ★
                } else {
                    base.to_string()
                }
            })
        } else {
            None
        };

    // ---- Kind color (lead override → epic_purple) ----
    let kind_color_value = if is_lead {
        Some(theme::epic_purple())
    } else {
        kind_color(session.session_kind)
    };

    let has_pills = docregblock_label.is_some() || kind_pill.is_some();

    // ---- Model ----
    let model = match session.model.as_deref() {
        Some(model_id) => {
            let (effort_total, effort_filled) =
                effort_bar_counts(session.effort.as_deref(), Some(model_id));
            let effort_label = rsi_common::model_utils::effort_ladder(model_id)
                .get(effort_filled.saturating_sub(1))
                .copied()
                .unwrap_or("—")
                .to_string();
            ModelCell::Long {
                long: rsi_common::model_utils::abbreviate_model(model_id),
                short: short_model_label(Some(model_id)).unwrap_or_default(),
                effort_total,
                effort_filled,
                effort_label,
            }
        }
        None => ModelCell::Missing,
    };

    // ---- Project name ----
    let project_name = match session.project_id {
        Some(pid) => projects
            .iter()
            .find(|p| p.id == pid)
            .map(|p| ProjectCell::Named(p.name.clone()))
            .unwrap_or(ProjectCell::Unassigned),
        None => ProjectCell::Unassigned,
    };

    // ---- Retry info (filter zero attempts) ----
    let retry_pending = session
        .retry_attempt
        .zip(session.max_retries)
        .filter(|(attempt, max)| *attempt > 0 && *max > 0);
    let retry_info = if settings.is_card_field_enabled(CardField::RetryInfo) {
        retry_pending
    } else {
        None
    };

    // ---- Container counts (Group/Epic only; scoped hierarchy activity) ----
    let container_counts = matches!(session.session_kind, SessionKind::Group | SessionKind::Epic)
        .then(|| {
            focus.map(|entry| ContainerCounts {
                running_epics: entry.running_epic_count,
                running_agents: entry.running_agent_count,
            })
        })
        .flatten();

    // ---- Work time / created (TD1 + F-027, gated by CardField — D3) ----
    let work_time_text = if settings.is_card_field_enabled(CardField::WorkTime) {
        session.work_time_ms.map(format_work_time_ms)
    } else {
        None
    };

    let created_text = if settings.is_card_field_enabled(CardField::CreatedTimestamp) {
        Some(created_text(session.created_at))
    } else {
        None
    };

    // ---- New-message signal (D2) ----
    let is_new_message = state.events_generation > state.last_seen_events_generation;

    // ---- Boolean flags ----
    let is_pinned = session.pinned_at.is_some();
    let is_testing_needed = session.testing_needed_at.is_some();
    let is_rotation_disabled = session.rotation_disabled_at.is_some();
    let is_sandboxed = session.sandbox_kind.is_some();
    let is_pending_archive = session.pending_archive;
    let is_stalled = state.is_stalled;

    let mut attention_reasons = Vec::new();
    if session.pending_question.is_some() || session.status == SessionStatus::WaitingApproval {
        attention_reasons.push("Waiting for input or approval".to_string());
    }
    if session.status == SessionStatus::Failed {
        attention_reasons.push("Session failed".to_string());
    }
    if retry_pending.is_some() {
        attention_reasons.push("Retry pending".to_string());
    }
    if state.is_stalled {
        attention_reasons.push("Session may be stalled".to_string());
    }
    if is_new_message {
        attention_reasons.push("Unread output".to_string());
    }
    let attention_glyph = if session.pending_question.is_some()
        || matches!(
            session.status,
            SessionStatus::WaitingApproval | SessionStatus::Failed
        ) {
        crate::ui::glyphs::NEEDS_YOU
    } else if retry_pending.is_some() {
        crate::ui::glyphs::RETRY
    } else if state.is_stalled {
        crate::ui::glyphs::STALLED
    } else if is_new_message {
        crate::ui::glyphs::UNREAD
    } else {
        ""
    };

    SessionRowViewModel {
        session_id: session.id,
        session_kind: session.session_kind,
        effective_epic_ordinal: display_identity.effective_epic_ordinal,
        is_manager: false,
        status: session.status,
        status_icon: status_icon_str,
        status_color: status_color_value,
        attention_glyph,
        attention_reasons,
        display_title,
        rotation_suffix,
        issue_identifier: session.issue_identifier.clone(),
        rating: session.rating,
        model,
        project_name,
        context_pct,
        cost,
        turns_text,
        time,
        heat_color,
        work_time_text,
        created_text,
        container_counts,
        is_new_message,
        description,
        short_summary,
        docregblock_label,
        kind_pill,
        kind_color: kind_color_value,
        has_pills,
        is_expanded: state.list_card_expanded,
        is_pinned,
        is_testing_needed,
        is_rotation_disabled,
        is_sandboxed,
        is_lead,
        is_pending_archive,
        is_stalled,
        retry_info,
    }
}

pub(crate) fn navigator_lifecycle_icon(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Starting => "◐",
        SessionStatus::Running => "●",
        SessionStatus::WaitingApproval => "?",
        SessionStatus::Completed => "✓",
        SessionStatus::Failed => "×",
        SessionStatus::Interrupted => "■",
        SessionStatus::Archived => "·",
        // Deleted is deliberately outside the seven-status navigator contract.
        SessionStatus::Deleted | _ => status_icon(status),
    }
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Whether `session` is the lead of its hierarchical parent (Group / Epic
/// container). Promotes the inline walk in `ui/session.rs` so the VM and
/// any future caller share one implementation.
fn compute_is_lead(session: &Session, all_sessions: &HashMap<Uuid, SessionState>) -> bool {
    session
        .parent_id
        .and_then(|pid| all_sessions.get(&pid))
        .and_then(|p| p.session.lead_session_id)
        .map(|lid| lid == session.id)
        .unwrap_or(false)
}

/// 3-letter kind pill label, or `None` for kinds that don't render a
/// pill (`Standard`, `TaskRabbit`, future variants).
fn kind_pill_label(kind: SessionKind) -> Option<&'static str> {
    match kind {
        SessionKind::Standard => None,
        SessionKind::TaskRabbit => None,
        SessionKind::Bug => Some("BUG"),
        SessionKind::Group => Some("GRP"),
        SessionKind::Epic => Some("EPC"),
        SessionKind::Story => Some("STY"),
        SessionKind::Task => Some("TSK"),
        SessionKind::Feature => Some("FEAT"),
        SessionKind::Refactor => Some("REF"),
        SessionKind::Research => Some("RES"),
        _ => None,
    }
}

/// Truncate `raw` to at most `limit` bytes at a UTF-8 char boundary.
/// Promotes the inline body from `build_card_view_model`'s description
/// path. The `limit` is in BYTES — `floor_char_boundary` honors char
/// boundaries so multibyte UTF-8 sequences are never split.
fn truncate_description(raw: &str, limit: usize) -> String {
    if raw.len() <= limit {
        raw.to_string()
    } else {
        let cut = floor_char_boundary(raw, limit);
        raw[..cut].to_string()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::app::app_test_helpers::{baseline_session, with_focused_kind};
    use crate::types::SessionState;
    use chrono::{Duration, Utc};
    use rsi_common::types::{
        ConversationEvent, EventType, Project, Role, Session, SessionKind, SessionStatus,
    };

    fn make_event(seq: i32, role: Option<Role>, content: String) -> ConversationEvent {
        ConversationEvent {
            id: 0,
            session_id: Uuid::nil(),
            sequence: seq,
            event_type: EventType::Message,
            role,
            content,
            created_at: Utc::now(),
            tool_name: None,
            tool_input: None,
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        }
    }

    fn one_session_app() -> (App, Uuid) {
        let app = with_focused_kind(SessionKind::Standard, None);
        let session_id = *app
            .sessions
            .keys()
            .next()
            .expect("with_focused_kind must populate one session");
        (app, session_id)
    }

    fn mutate_session<F>(app: &mut App, session_id: Uuid, mutate: F)
    where
        F: FnOnce(&mut Session),
    {
        let state = app
            .sessions
            .get_mut(&session_id)
            .expect("session must exist");
        mutate(&mut state.session);
    }

    // ---- placeholder + unknown-session ----

    #[test]
    fn placeholder_returns_empty_shape() {
        let id = Uuid::new_v4();
        let p = SessionRowViewModel::placeholder(id);
        assert_eq!(p.session_id, id);
        assert!(p.display_title.is_empty());
        assert!(p.rotation_suffix.is_empty());
        assert!(p.status_icon.is_empty());
        assert_eq!(p.model, ModelCell::Missing);
        assert_eq!(p.project_name, ProjectCell::Unassigned);
        assert_eq!(p.context_pct, ContextCell::Missing);
        assert_eq!(p.cost, CostCell::Missing);
        assert!(matches!(p.time, TimeCell::Missing));
        assert!(p.description.is_none());
        assert!(p.short_summary.is_none());
        assert!(p.docregblock_label.is_none());
        assert!(p.kind_pill.is_none());
        assert!(p.kind_color.is_none());
        assert!(!p.has_pills);
        assert!(!p.is_expanded);
        assert!(!p.is_lead);
        assert!(!p.is_pinned);
        assert!(!p.is_testing_needed);
        assert!(!p.is_rotation_disabled);
        assert!(!p.is_sandboxed);
        assert!(!p.is_pending_archive);
        assert!(!p.is_stalled);
        assert!(p.retry_info.is_none());
    }

    #[test]
    fn compute_session_row_unknown_session_returns_placeholder() {
        let (app, _real) = one_session_app();
        let unknown = Uuid::new_v4();
        let row = compute_session_row(&app, unknown, &app.settings);
        assert_eq!(row, SessionRowViewModel::placeholder(unknown));
    }

    // ---- populated session ----

    #[test]
    fn running_session_populates_every_visible_cell() {
        let (mut app, session_id) = one_session_app();
        mutate_session(&mut app, session_id, |s| {
            s.status = SessionStatus::Running;
            s.model = Some("claude-sonnet-5".to_string());
            s.cost_usd = Some(0.42);
            s.num_turns = Some(7);
        });
        if let Some(state) = app.sessions.get_mut(&session_id) {
            state.live_context_pct = Some(48.0);
            state
                .events
                .push(make_event(1, Some(Role::User), "hello world".to_string()));
        }
        let row = compute_session_row(&app, session_id, &app.settings);
        assert!(matches!(row.model, ModelCell::Long { .. }));
        assert_eq!(
            row.context_pct,
            ContextCell::Display {
                label: "48%?".to_string(),
                pct: Some(48.0),
            }
        );
        assert_eq!(row.cost, CostCell::Amount("$0.42".to_string()));
        assert_eq!(row.turns_text, Some("7".to_string()));
        assert!(matches!(row.time, TimeCell::Value { .. }));
        assert_eq!(row.status, SessionStatus::Running);
    }

    // ---- missing data ----

    #[test]
    fn missing_model_gives_model_missing() {
        let (app, session_id) = one_session_app();
        let row = compute_session_row(&app, session_id, &app.settings);
        assert_eq!(row.model, ModelCell::Missing);
    }

    #[test]
    fn display_title_replaces_newlines_with_a_space() {
        let (mut app, session_id) = one_session_app();
        mutate_session(&mut app, session_id, |s| {
            s.title = Some("PIPELINE MODE: true\nPIPELINE STAGE: review".to_string());
        });

        let row = compute_session_row(&app, session_id, &app.settings);

        assert_eq!(
            row.display_title,
            "PIPELINE MODE: true PIPELINE STAGE: review"
        );
    }

    #[test]
    fn missing_project_gives_project_unassigned() {
        let (app, session_id) = one_session_app();
        let row = compute_session_row(&app, session_id, &app.settings);
        assert_eq!(row.project_name, ProjectCell::Unassigned);
    }

    #[test]
    fn assigned_project_resolves_to_named() {
        let (mut app, session_id) = one_session_app();
        let pid = Uuid::new_v4();
        app.projects.push(Project {
            id: pid,
            name: "rsi".to_string(),
            path: Some("/tmp".into()),
            description: None,
            color: "#89b4fa".to_string(),
            context_files: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });
        mutate_session(&mut app, session_id, |s| {
            s.project_id = Some(pid);
        });
        let row = compute_session_row(&app, session_id, &app.settings);
        assert_eq!(row.project_name, ProjectCell::Named("rsi".to_string()));
    }

    // ---- archived ----

    #[test]
    fn archived_session_preserves_status_color() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        let (mut app, session_id) = one_session_app();
        mutate_session(&mut app, session_id, |s| {
            s.status = SessionStatus::Archived;
        });
        let row = compute_session_row(&app, session_id, &app.settings);
        assert_eq!(row.status_color, theme::status_archived());
        assert_eq!(row.status_icon, "·");
    }

    // ---- time-since-last-event (PI-5, F-019) ----

    #[test]
    fn time_cell_is_uniformly_relative_to_last_update() {
        // PI-5: `compute_time_display` dropped its Running/Starting special
        // case (previously elapsed-since-`created_at`) — `row.time` is now
        // uniformly relative-to-`updated_at` for EVERY status, matching
        // `format_relative_time`'s single "time since last event" meaning.
        let (mut app, session_id) = one_session_app();
        mutate_session(&mut app, session_id, |s| {
            s.status = SessionStatus::Running;
            s.created_at = Utc::now() - Duration::seconds(3600); // long-running
            s.updated_at = Utc::now() - Duration::seconds(125); // last event ~2m ago
        });
        let row = compute_session_row(&app, session_id, &app.settings);
        match row.time {
            TimeCell::Value { text, .. } => {
                assert!(
                    text.contains("ago"),
                    "row.time must be relative-to-last-update even for a \
                     Running session (uniform semantics post-PI-5); got {text:?}"
                );
                assert_eq!(
                    text, "2m ago",
                    "row.time must reflect updated_at (~2m ago), not the \
                     much-older created_at (1h ago)"
                );
            }
            TimeCell::Missing => panic!("expected TimeCell::Value"),
        }
    }

    // ---- cost thresholds ----

    #[test]
    fn cost_below_threshold_emits_below_threshold_cell() {
        let (mut app, session_id) = one_session_app();
        mutate_session(&mut app, session_id, |s| {
            s.cost_usd = Some(0.005);
        });
        let row = compute_session_row(&app, session_id, &app.settings);
        assert_eq!(row.cost, CostCell::BelowThreshold);
    }

    #[test]
    fn cost_zero_dot_zero_one_emits_amount() {
        let (mut app, session_id) = one_session_app();
        mutate_session(&mut app, session_id, |s| {
            s.cost_usd = Some(0.01);
        });
        let row = compute_session_row(&app, session_id, &app.settings);
        assert_eq!(row.cost, CostCell::Amount("$0.01".to_string()));
    }

    // ---- rotation suffix ----

    #[test]
    fn rotation_suffix_present_when_continued_from_set() {
        let (mut app, leaf_id) = one_session_app();
        // Build a 3-link rotation chain: leaf -> mid -> root.
        let mid_id = Uuid::new_v4();
        let root_id = Uuid::new_v4();
        let mut mid = baseline_session(mid_id, SessionKind::Standard);
        mid.continued_from = Some(root_id);
        let root = baseline_session(root_id, SessionKind::Standard);
        app.sessions.insert(mid_id, SessionState::new(mid));
        app.sessions.insert(root_id, SessionState::new(root));
        mutate_session(&mut app, leaf_id, |s| {
            s.continued_from = Some(mid_id);
        });
        let row = compute_session_row(&app, leaf_id, &app.settings);
        assert_eq!(row.rotation_suffix, " \u{21BB}2");
    }

    // ---- description truncation + gating ----

    #[test]
    fn description_respects_200_char_boundary() {
        let (mut app, session_id) = one_session_app();
        if let Some(state) = app.sessions.get_mut(&session_id) {
            state.list_card_expanded = true;
            // 250-byte ASCII string — should truncate to <= 200 bytes.
            state
                .events
                .push(make_event(1, Some(Role::User), "x".repeat(250)));
        }
        let row = compute_session_row(&app, session_id, &app.settings);
        let desc = row.description.expect("expanded + User Message → Some");
        assert!(
            desc.len() <= 200,
            "expected <= 200 bytes, got {}",
            desc.len()
        );
        // Ensure we kept char-boundary safety on a multibyte payload too.
        let mut app2 = with_focused_kind(SessionKind::Standard, None);
        let sid2 = *app2.sessions.keys().next().unwrap();
        if let Some(state) = app2.sessions.get_mut(&sid2) {
            state.list_card_expanded = true;
            state
                .events
                .push(make_event(1, Some(Role::User), "é".repeat(150)));
        }
        let row2 = compute_session_row(&app2, sid2, &app2.settings);
        let desc2 = row2.description.expect("multibyte expanded → Some");
        // 150 chars × 2 bytes = 300 bytes; truncated to <=200 bytes at a
        // valid char boundary. Char-boundary safety = string parses fine.
        assert!(desc2.len() <= 200);
        assert!(desc2.is_char_boundary(desc2.len()));
    }

    #[test]
    fn description_gated_by_settings() {
        let (mut app, session_id) = one_session_app();
        if let Some(state) = app.sessions.get_mut(&session_id) {
            state.list_card_expanded = true;
            state
                .events
                .push(make_event(1, Some(Role::User), "x".repeat(50)));
        }
        // Disable the Description card field.
        for entry in app.settings.card_fields.iter_mut() {
            if entry.field == CardField::Description {
                entry.enabled = false;
            }
        }
        let row = compute_session_row(&app, session_id, &app.settings);
        assert!(row.description.is_none());
    }

    #[test]
    fn description_gated_by_collapsed_state() {
        let (mut app, session_id) = one_session_app();
        if let Some(state) = app.sessions.get_mut(&session_id) {
            state.list_card_expanded = false; // collapsed
            state
                .events
                .push(make_event(1, Some(Role::User), "x".repeat(50)));
        }
        let row = compute_session_row(&app, session_id, &app.settings);
        assert!(row.description.is_none());
    }

    // ---- kind pill ----

    #[test]
    fn kind_pill_lead_appends_star() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        let (mut app, leaf_id) = one_session_app();
        // Set leaf kind=Epic, parent.lead_session_id = leaf.
        let parent_id = Uuid::new_v4();
        let mut parent = baseline_session(parent_id, SessionKind::Group);
        parent.lead_session_id = Some(leaf_id);
        app.sessions.insert(parent_id, SessionState::new(parent));
        mutate_session(&mut app, leaf_id, |s| {
            s.session_kind = SessionKind::Epic;
            s.parent_id = Some(parent_id);
        });
        if let Some(state) = app.sessions.get_mut(&leaf_id) {
            state.list_card_expanded = true;
        }
        let row = compute_session_row(&app, leaf_id, &app.settings);
        assert_eq!(row.kind_pill, Some("EPC \u{2605}".to_string()));
        assert_eq!(row.kind_color, Some(theme::epic_purple()));
        assert!(row.is_lead);
    }

    #[test]
    fn kind_pill_disabled_setting_yields_none() {
        let (mut app, session_id) = one_session_app();
        mutate_session(&mut app, session_id, |s| {
            s.session_kind = SessionKind::Epic;
        });
        if let Some(state) = app.sessions.get_mut(&session_id) {
            state.list_card_expanded = true;
        }
        for entry in app.settings.card_fields.iter_mut() {
            if entry.field == CardField::KindPill {
                entry.enabled = false;
            }
        }
        let row = compute_session_row(&app, session_id, &app.settings);
        assert!(row.kind_pill.is_none());
    }

    #[test]
    fn kind_pill_collapsed_yields_none() {
        let (mut app, session_id) = one_session_app();
        mutate_session(&mut app, session_id, |s| {
            s.session_kind = SessionKind::Epic;
        });
        // list_card_expanded defaults to false.
        let row = compute_session_row(&app, session_id, &app.settings);
        assert!(row.kind_pill.is_none());
    }

    // ---- stalled override ----

    #[test]
    fn stalled_session_overrides_status_color() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        let (mut app, session_id) = one_session_app();
        mutate_session(&mut app, session_id, |s| {
            s.status = SessionStatus::Running;
        });
        if let Some(state) = app.sessions.get_mut(&session_id) {
            state.is_stalled = true;
        }
        let row = compute_session_row(&app, session_id, &app.settings);
        assert_eq!(row.status_color, theme::status_stalled());
        assert!(row.is_stalled);
    }

    // ---- pending-archive ----

    #[test]
    fn pending_archive_active_dims_status_color() {
        let _pinned_theme = crate::ui::theme::pin_theme_state();
        let (mut app, session_id) = one_session_app();
        mutate_session(&mut app, session_id, |s| {
            s.status = SessionStatus::Running;
            s.pending_archive = true;
        });
        let row = compute_session_row(&app, session_id, &app.settings);
        assert_eq!(row.status_color, theme::overlay1());
        assert_eq!(row.status_icon, "●");
        assert!(row.is_pending_archive);
    }

    #[test]
    fn pending_archive_inactive_does_not_dim() {
        let (mut app, session_id) = one_session_app();
        mutate_session(&mut app, session_id, |s| {
            s.status = SessionStatus::Completed;
            s.pending_archive = true;
        });
        let row = compute_session_row(&app, session_id, &app.settings);
        // Completed status — pending_archive should NOT override the color.
        assert_eq!(row.status_color, status_color(SessionStatus::Completed));
        assert_eq!(row.status_icon, "✓");
        assert!(row.is_pending_archive);
    }

    // ---- narrow-width fallbacks ----

    #[test]
    fn narrow_width_fallbacks_keep_optional_fields_optional() {
        // Baseline session: model=None, cost_usd=None, num_turns=None,
        // no events, list_card_expanded=false. All optional cells must
        // be explicitly absent — never crashing or returning placeholders.
        let (app, session_id) = one_session_app();
        let row = compute_session_row(&app, session_id, &app.settings);
        assert_eq!(row.model, ModelCell::Missing);
        assert_eq!(row.cost, CostCell::Missing);
        assert!(row.turns_text.is_none());
        assert!(row.description.is_none());
        assert!(row.short_summary.is_none());
        assert!(row.docregblock_label.is_none());
        assert!(row.kind_pill.is_none());
        assert!(!row.has_pills);
    }

    // ---- retry info ----

    #[test]
    fn retry_info_filters_zero_attempts() {
        let (mut app, session_id) = one_session_app();
        mutate_session(&mut app, session_id, |s| {
            s.retry_attempt = Some(0);
            s.max_retries = Some(3);
        });
        let row = compute_session_row(&app, session_id, &app.settings);
        assert!(row.retry_info.is_none());

        mutate_session(&mut app, session_id, |s| {
            s.retry_attempt = Some(1);
            s.max_retries = Some(3);
        });
        let row2 = compute_session_row(&app, session_id, &app.settings);
        assert_eq!(row2.retry_info, Some((1, 3)));
    }

    // ---- pin indicator ----

    #[test]
    fn navigator_pin_remains_visible_when_legacy_card_field_is_disabled() {
        let (mut app, session_id) = one_session_app();
        mutate_session(&mut app, session_id, |s| {
            s.pinned_at = Some(Utc::now());
        });
        // Gate ON (default) → pinned reflected.
        let row = compute_session_row(&app, session_id, &app.settings);
        assert!(row.is_pinned);

        // Legacy state remains deserializable but cannot remove navigator-required pin state.
        for entry in app.settings.card_fields.iter_mut() {
            if entry.field == CardField::PinIndicator {
                entry.enabled = false;
            }
        }
        let row2 = compute_session_row(&app, session_id, &app.settings);
        assert!(row2.is_pinned);
    }

    // ---- effort bars ----

    #[test]
    fn effort_bars_zero_for_unsupported_model() {
        let (mut app, session_id) = one_session_app();
        mutate_session(&mut app, session_id, |s| {
            // Haiku never supports effort (unlike Sonnet/Opus, which support it
            // from 4.6 onward) — a stable "0 effort levels" fixture.
            s.model = Some("claude-haiku-4-5".to_string());
            s.effort = None;
        });
        let row = compute_session_row(&app, session_id, &app.settings);
        match row.model {
            ModelCell::Long {
                effort_total,
                effort_filled,
                ..
            } => {
                assert_eq!(effort_total, 0);
                assert_eq!(effort_filled, 0);
            }
            ModelCell::Missing => panic!("expected ModelCell::Long for haiku"),
        }
    }

    // ---- container scope counts ----

    #[test]
    fn group_counts_direct_running_or_starting_standard_leaf() {
        for leaf_status in [SessionStatus::Running, SessionStatus::Starting] {
            let (mut app, group_id) = one_session_app();
            mutate_session(&mut app, group_id, |s| {
                s.session_kind = SessionKind::Group;
                s.status = if leaf_status == SessionStatus::Starting {
                    SessionStatus::Completed
                } else {
                    SessionStatus::Running
                };
                s.pending_archive = leaf_status == SessionStatus::Running;
            });
            let leaf_id = Uuid::new_v4();
            let mut leaf = baseline_session(leaf_id, SessionKind::Standard);
            leaf.parent_id = Some(group_id);
            leaf.status = leaf_status;
            app.sessions.insert(leaf_id, SessionState::new(leaf));

            let group_row = compute_session_row(&app, group_id, &app.settings);
            assert_eq!(
                group_row.container_counts,
                Some(ContainerCounts {
                    running_epics: 0,
                    running_agents: 1,
                })
            );
            assert_eq!(group_row.status_icon, "◉");
            assert_eq!(group_row.status_color, theme::status_running());
            let leaf_row = compute_session_row(&app, leaf_id, &app.settings);
            assert_eq!(leaf_row.container_counts, None);
        }
    }

    #[test]
    fn container_counts_include_active_work_in_nested_epics() {
        let (mut app, group_id) = one_session_app();
        mutate_session(&mut app, group_id, |s| {
            s.session_kind = SessionKind::Group;
            s.status = SessionStatus::Completed;
        });

        let epic_id = Uuid::new_v4();
        let mut epic = baseline_session(epic_id, SessionKind::Epic);
        epic.parent_id = Some(group_id);
        epic.status = SessionStatus::Completed;
        app.sessions.insert(epic_id, SessionState::new(epic));

        let running_id = Uuid::new_v4();
        let mut running = baseline_session(running_id, SessionKind::Task);
        running.parent_id = Some(epic_id);
        running.status = SessionStatus::Running;
        app.sessions.insert(running_id, SessionState::new(running));

        let completed_id = Uuid::new_v4();
        let mut completed = baseline_session(completed_id, SessionKind::Task);
        completed.parent_id = Some(epic_id);
        completed.status = SessionStatus::Completed;
        app.sessions
            .insert(completed_id, SessionState::new(completed));

        let waiting_id = Uuid::new_v4();
        let mut waiting = baseline_session(waiting_id, SessionKind::Standard);
        waiting.parent_id = Some(group_id);
        waiting.status = SessionStatus::WaitingApproval;
        app.sessions.insert(waiting_id, SessionState::new(waiting));

        for status in [SessionStatus::Archived, SessionStatus::Deleted] {
            let id = Uuid::new_v4();
            let mut hidden = baseline_session(id, SessionKind::Standard);
            hidden.parent_id = Some(group_id);
            hidden.status = status;
            app.sessions.insert(id, SessionState::new(hidden));
        }

        let row = compute_session_row(&app, group_id, &app.settings);
        assert_eq!(
            row.container_counts,
            Some(ContainerCounts {
                running_epics: 1,
                running_agents: 1,
            })
        );
        assert_eq!(row.status_icon, "◉");
        assert_eq!(row.status_color, theme::status_running());

        let epic_row = compute_session_row(&app, epic_id, &app.settings);
        assert_eq!(
            epic_row.container_counts,
            Some(ContainerCounts {
                running_epics: 0,
                running_agents: 1,
            })
        );
        assert_eq!(epic_row.status_icon, "◉");

        mutate_session(&mut app, group_id, |s| s.status = SessionStatus::Running);
        let legacy_active_container = compute_session_row(&app, group_id, &app.settings);
        assert_eq!(
            legacy_active_container.container_counts,
            row.container_counts
        );

        mutate_session(&mut app, running_id, |s| {
            s.status = SessionStatus::Completed;
        });
        mutate_session(&mut app, group_id, |s| s.status = SessionStatus::Completed);
        let quiet_group = compute_session_row(&app, group_id, &app.settings);
        assert_eq!(quiet_group.status_icon, "✓");
        assert_eq!(
            quiet_group.container_counts,
            Some(ContainerCounts {
                running_epics: 0,
                running_agents: 0,
            })
        );
    }

    #[test]
    fn container_counts_none_for_leaf_kinds() {
        let (app, session_id) = one_session_app(); // Standard kind by default
        let row = compute_session_row(&app, session_id, &app.settings);
        assert_eq!(row.container_counts, None);
    }

    // ---- work time (TD1, PI-7, F-023..F-026) ----

    #[test]
    fn work_time_text_hides_gracefully_when_absent() {
        let (mut app, session_id) = one_session_app();
        // Absent: work_time_ms is None (baseline default).
        let row = compute_session_row(&app, session_id, &app.settings);
        assert_eq!(row.work_time_text, None);

        mutate_session(&mut app, session_id, |s| {
            s.work_time_ms = Some(9_125_000);
        });
        let row2 = compute_session_row(&app, session_id, &app.settings);
        assert_eq!(row2.work_time_text, Some("2h32m".to_string()));
    }

    #[test]
    fn work_time_gated_by_settings() {
        let (mut app, session_id) = one_session_app();
        // Force a clean baseline card-field layout regardless of ambient
        // `~/.rsi/state.json` contents: `App::new()` falls back to the real
        // on-disk `PersistedState` with no test-time isolation, so without
        // this the toggle loop below can be a silent no-op against whatever
        // `card_fields` array happens to be on this machine (review Finding
        // 1 — hermeticity fix).
        app.settings.card_fields = crate::settings::default_card_fields();
        mutate_session(&mut app, session_id, |s| {
            s.work_time_ms = Some(9_125_000);
        });
        // Gate ON (default) -> Some.
        let row = compute_session_row(&app, session_id, &app.settings);
        assert!(row.work_time_text.is_some());

        // Gate OFF -> None even though work_time_ms is Some.
        for entry in app.settings.card_fields.iter_mut() {
            if entry.field == CardField::WorkTime {
                entry.enabled = false;
            }
        }
        let row2 = compute_session_row(&app, session_id, &app.settings);
        assert!(row2.work_time_text.is_none());
    }

    // ---- created timestamp (F-027, D3) ----

    #[test]
    fn created_text_gated_by_settings() {
        let (mut app, session_id) = one_session_app();
        // Force a clean baseline card-field layout regardless of ambient
        // `~/.rsi/state.json` contents — same hermeticity fix as
        // `work_time_gated_by_settings` above (review Finding 1).
        app.settings.card_fields = crate::settings::default_card_fields();
        // Gate ON (default) -> Some (created_at is always present, never Option).
        let row = compute_session_row(&app, session_id, &app.settings);
        assert!(row.created_text.is_some());

        // Gate OFF -> None.
        for entry in app.settings.card_fields.iter_mut() {
            if entry.field == CardField::CreatedTimestamp {
                entry.enabled = false;
            }
        }
        let row2 = compute_session_row(&app, session_id, &app.settings);
        assert!(row2.created_text.is_none());
    }

    // ---- new-message signal (D2, F-029) ----

    #[test]
    fn is_new_message_true_when_generation_advances_past_last_seen() {
        let (mut app, session_id) = one_session_app();
        if let Some(state) = app.sessions.get_mut(&session_id) {
            state.events_generation = 3;
            state.last_seen_events_generation = 1;
        }
        let row = compute_session_row(&app, session_id, &app.settings);
        assert!(row.is_new_message);

        if let Some(state) = app.sessions.get_mut(&session_id) {
            state.last_seen_events_generation = state.events_generation;
        }
        let row2 = compute_session_row(&app, session_id, &app.settings);
        assert!(!row2.is_new_message);
    }
}
