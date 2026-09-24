//! Budget policy form rendering (Settings -> Budgets add/edit overlay).
//!
//! Mirrors `hook_form::render_hook_form`'s layout (bordered popup, one row
//! per field, hint line at the bottom), generalized to the 9 fields exposed
//! by `OverlayState::BudgetPolicyForm`.

use crate::model_control_budgets::{MODEL_TIERS, SCOPE_KINDS};
use crate::ui::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph};

use super::fixed_centered_rect;

/// Render the budget policy add/edit form overlay.
#[allow(clippy::too_many_arguments)]
pub(super) fn render_budget_policy_form(
    frame: &mut Frame,
    area: Rect,
    focused_field: usize,
    scope_kind_idx: usize,
    scope_id: &str,
    purpose: &str,
    model_tier_idx: usize,
    max_total_tokens: &str,
    max_concurrency: &str,
    max_calls_per_window: &str,
    rate_window_seconds: &str,
    alert_threshold_ratio: &str,
    is_editing: bool,
) {
    let popup_height: u16 = 12;
    let popup_area = fixed_centered_rect(area, 70, popup_height);
    frame.render_widget(Clear, popup_area);

    let title = if is_editing {
        " Edit Budget Policy "
    } else {
        " New Budget Policy "
    };

    let block = theme::overlay_block()
        .title(Line::from(vec![Span::styled(
            title,
            Style::default()
                .fg(theme::overlay_title())
                .add_modifier(Modifier::BOLD),
        )]))
        .padding(Padding::horizontal(1));
    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    if inner.height < 10 {
        return;
    }

    let scope_kind_label = SCOPE_KINDS
        .get(scope_kind_idx)
        .map_or("?", |(_, label)| *label);
    let model_tier_label = MODEL_TIERS
        .get(model_tier_idx)
        .map_or("?", |(_, label)| *label);

    let fields: [(&str, String); 9] = [
        ("Scope", format!("{scope_kind_label}  ↑/↓ to cycle")),
        ("ScopeId", scope_id.to_string()),
        ("Purpose", format!("{purpose} (blank = all)")),
        ("Tier", format!("{model_tier_label}  ↑/↓ to cycle")),
        ("MaxTokens", max_total_tokens.to_string()),
        ("MaxConc", max_concurrency.to_string()),
        ("CallsPerWin", max_calls_per_window.to_string()),
        ("WindowSecs", rate_window_seconds.to_string()),
        ("AlertRatio", alert_threshold_ratio.to_string()),
    ];

    for (i, (label, value)) in fields.iter().enumerate() {
        let row_y = inner.y + i as u16;
        let is_focused = i == focused_field;

        let label_style = if is_focused {
            Style::default()
                .fg(theme::mauve())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::overlay_hint())
        };

        let cursor = if is_focused { "█" } else { "" };
        let line = Line::from(vec![
            Span::styled(format!("{label:>11}: "), label_style),
            Span::styled(value.clone(), Style::default().fg(theme::text())),
            Span::styled(cursor, Style::default().fg(theme::text())),
        ]);
        let row_area = Rect::new(inner.x, row_y, inner.width, 1);
        frame.render_widget(Paragraph::new(line), row_area);
    }

    let hint_y = inner.y + inner.height - 1;
    let hint_area = Rect::new(inner.x, hint_y, inner.width, 1);
    let hint = Line::from(vec![Span::styled(
        "Tab: next  ↑/↓: cycle  Enter: save  Esc: cancel",
        Style::default().fg(theme::overlay_hint()),
    )]);
    frame.render_widget(Paragraph::new(hint), hint_area);
}
