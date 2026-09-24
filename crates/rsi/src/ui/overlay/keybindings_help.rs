//! Contextual action-discovery overlay rendering.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::action_registry::{
    ActionContext, AvailableAction, HelpOrigin, available_actions, overlay_help_for,
};
use crate::app::App;
use crate::overlay::keybindings_help::HelpView;
use crate::ui::theme;

use super::fixed_centered_rect;

fn binding_text(action: &AvailableAction, context: &ActionContext) -> String {
    crate::action_registry::bindings_for_action(action, context).join(", ")
}

/// One rendered help row, from either a registry action or the overlay
/// discovery catalog.
struct HelpRow {
    category: &'static str,
    keys: String,
    label: &'static str,
}

pub(crate) fn contextual_lines(app: &App, filter: &str) -> Vec<Line<'static>> {
    let context = ActionContext::from_app(app);
    let terms: Vec<_> = filter.split_whitespace().map(str::to_lowercase).collect();
    let matches = |haystack: String| {
        let haystack = haystack.to_lowercase();
        terms.iter().all(|term| haystack.contains(term))
    };

    let mut rows = Vec::new();
    for action in available_actions(&context) {
        let bindings = binding_text(&action, &context);
        if matches(format!(
            "{} {} {} {} {:?}",
            action.descriptor.category,
            bindings,
            action.descriptor.label,
            action.descriptor.command_aliases.join(" "),
            action.request.id,
        )) {
            rows.push(HelpRow {
                category: action.descriptor.category,
                keys: bindings,
                label: action.descriptor.label,
            });
        }
    }
    // Overlay-owned keys come from the shared discovery catalog. They are
    // rendered and searchable here but never dispatched through the registry.
    if let Some(route) = overlay_help_for(&context) {
        for entry in route.entries() {
            if matches(format!(
                "{} {} {} {:?}",
                route.title, entry.keys, entry.label, entry.role
            )) {
                rows.push(HelpRow {
                    category: route.title,
                    keys: entry.keys.to_string(),
                    label: entry.label,
                });
            }
        }
    }

    let mut lines = Vec::new();
    let mut category = None;
    for row in rows {
        if category != Some(row.category) {
            if !lines.is_empty() {
                lines.push(Line::from(""));
            }
            category = Some(row.category);
            lines.push(Line::from(Span::styled(
                row.category,
                Style::default()
                    .fg(theme::accent())
                    .add_modifier(Modifier::BOLD),
            )));
        }
        // Keep the 16-column key gutter; overflowing catalog keys still get
        // one separating space before the description.
        let key = if row.keys.len() >= 16 {
            format!("{} ", row.keys)
        } else {
            row.keys
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {key:<16}"),
                Style::default()
                    .fg(theme::semantic_color(
                        crate::ui::theme_roles::ThemeRole::Running,
                    ))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(row.label, Style::default().fg(theme::text())),
        ]));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "No actions match this context and filter.",
            Style::default().fg(theme::subtext1()),
        )));
    }
    lines
}

fn all_commands_lines(app: &App, filter: &str) -> Vec<Line<'static>> {
    let context = ActionContext::from_app(app);
    let terms: Vec<_> = filter.split_whitespace().map(str::to_lowercase).collect();
    let mut lines = Vec::new();
    for (title, rows) in crate::action_registry::all_commands_sections(&context) {
        let matching: Vec<_> = rows
            .into_iter()
            .filter(|row| {
                let row = row.to_lowercase();
                terms.iter().all(|term| row.contains(term))
            })
            .collect();
        if matching.is_empty() {
            continue;
        }
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            title,
            Style::default()
                .fg(theme::accent())
                .add_modifier(Modifier::BOLD),
        )));
        lines.extend(
            matching
                .into_iter()
                .map(|row| Line::from(format!("  {row}"))),
        );
    }
    if lines.is_empty() {
        lines.push(Line::from("No commands match this filter."));
    }
    lines
}

fn help_footer(filter: &str, search_active: bool) -> String {
    let navigation = if search_active {
        format!("/{filter}█")
    } else if filter.is_empty() {
        "/ filter · j/k · Esc/q/? return".to_string()
    } else {
        format!("filter: {filter} · / edit · Esc clear · q/? return")
    };
    format!("{navigation} · Tab all/context · m manual · M pager")
}

pub fn render_keybindings_help(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    scroll_offset: usize,
    filter: &str,
    search_active: bool,
    origin: HelpOrigin,
) {
    let view = match &app.overlay {
        crate::types::OverlayState::KeybindingsHelp { view, .. } => *view,
        _ => HelpView::Contextual,
    };
    let popup = fixed_centered_rect(area, 80, 30);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(match view {
            HelpView::Contextual => format!(" Context Help — {} ", origin.title()),
            HelpView::All => format!(
                " All Commands ({}) ",
                crate::action_registry::ACTION_DESCRIPTORS.len()
            ),
        })
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::focused_border()))
        .style(Style::default().bg(theme::semantic_color(
            crate::ui::theme_roles::ThemeRole::ElevatedSurface,
        )));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let content_height = inner.height.saturating_sub(2);
    let lines = match view {
        HelpView::Contextual => contextual_lines(app, filter),
        HelpView::All => all_commands_lines(app, filter),
    };
    let content_width = inner.width.saturating_sub(2);
    let logical_lines = lines.len();
    let paragraph = match view {
        HelpView::Contextual => Paragraph::new(lines),
        HelpView::All => Paragraph::new(lines).wrap(Wrap { trim: false }),
    };
    let visible_lines = match view {
        HelpView::Contextual => logical_lines,
        HelpView::All => paragraph.line_count(content_width),
    };
    let max_scroll = visible_lines.saturating_sub(content_height as usize);
    let scroll = scroll_offset.min(max_scroll);
    frame.render_widget(
        paragraph.scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0)),
        Rect::new(inner.x + 1, inner.y, content_width, content_height),
    );

    let search = help_footer(filter, search_active);
    frame.render_widget(
        Paragraph::new(search).style(Style::default().fg(theme::subtext1())),
        Rect::new(
            inner.x + 1,
            inner.y + inner.height.saturating_sub(1),
            inner.width.saturating_sub(2),
            1,
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::app_test_helpers::with_session_list;

    #[test]
    fn all_view_lists_every_descriptor() {
        let app = with_session_list(0);
        let lines = all_commands_lines(&app, "");
        let text = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let descriptor_rows = text
            .lines()
            .filter(|line| line.starts_with("  ● ") || line.starts_with("  ○ "))
            .count();
        assert_eq!(
            descriptor_rows,
            crate::action_registry::ACTION_DESCRIPTORS.len()
        );
        for descriptor in crate::action_registry::ACTION_DESCRIPTORS {
            assert!(
                text.contains(descriptor.label),
                "missing {:?}",
                descriptor.id
            );
        }
        assert!(text.contains("File viewer commands"));
        assert!(text.contains("Raw keys"));
        assert!(text.contains("Overlay ·"));
    }

    #[test]
    fn help_footer_shows_all_manual_pager_hints() {
        let footer = help_footer("", false);
        for hint in ["Tab all/context", "m manual", "M pager"] {
            assert!(footer.contains(hint));
        }
    }

    #[test]
    fn t32_list_help_actions_recheck_and_route_to_registered_executors() {
        let app = with_session_list(1);
        let context = ActionContext::from_app(&app);
        let available = available_actions(&context);
        assert!(!available.is_empty());
        for action in available {
            assert_eq!(
                crate::action_registry::recheck_request(&context, action.request),
                crate::action_registry::ActionAvailability::Available(action.request)
            );
        }
        assert!(contextual_lines(&app, "refresh").len() < contextual_lines(&app, "").len());
    }

    #[test]
    fn t31_list_help_includes_registered_copy_uuid_binding_without_placeholder() {
        let app = with_session_list(1);
        let context = ActionContext::from_app(&app);
        let actions = available_actions(&context);
        let ids: std::collections::BTreeSet<_> =
            actions.iter().map(|action| action.request.id).collect();

        for id in [
            crate::action_registry::ActionId::JumpTop,
            crate::action_registry::ActionId::JumpBottom,
            crate::action_registry::ActionId::OpenThemePicker,
            crate::action_registry::ActionId::OpenLegacyColors,
        ] {
            assert!(ids.contains(&id), "session help omitted {id:?}");
        }
        for action in actions {
            assert!(
                !binding_text(&action, &context).is_empty(),
                "help rendered a placeholder key for {:?}",
                action.request.id
            );
        }
    }

    #[test]
    fn manager_chords_appear_in_session_list_help() {
        // RSI #415: the three manager chords are registry metadata, so the
        // contextual help overlay renders their bindings next to the labels of
        // the exact actions `:manager policy` / `:manager board` /
        // `:manager decisions` dispatch.
        let app = with_session_list(1);
        let context = ActionContext::from_app(&app);
        let actions = available_actions(&context);

        for (id, expected_binding, expected_label) in [
            (
                crate::action_registry::ActionId::ManagerPolicy,
                "<Space>gp",
                "Edit manager policy",
            ),
            (
                crate::action_registry::ActionId::ManagerBoard,
                "<Space>gb",
                "Open manager board",
            ),
            (
                crate::action_registry::ActionId::ManagerDecisions,
                "<Space>gd",
                "Open manager decisions",
            ),
        ] {
            let action = actions
                .iter()
                .find(|action| action.request.id == id)
                .unwrap_or_else(|| panic!("session help omitted {id:?}"));
            assert_eq!(binding_text(action, &context), expected_binding);
            assert!(action.descriptor.label.contains(expected_label));
        }

        let rendered = contextual_lines(&app, "");
        for snippet in ["<Space>gp", "<Space>gb", "<Space>gd"] {
            assert!(
                rendered
                    .iter()
                    .any(|line| format!("{line}").contains(snippet)),
                "help renderer omitted {snippet}"
            );
        }
    }

    #[test]
    fn help_search_is_case_insensitive_and_ands_terms_across_descriptor_fields() {
        let app = with_session_list(1);
        let all = contextual_lines(&app, "");
        assert_eq!(
            contextual_lines(&app, "ReFrEsH"),
            contextual_lines(&app, "refresh")
        );
        assert!(contextual_lines(&app, "refresh navigation").len() < all.len());
        assert!(contextual_lines(&app, "manager policy").len() < all.len());
        assert!(contextual_lines(&app, "Ctrl-Alt-G").len() < all.len());
        assert!(contextual_lines(&app, "gg").len() < all.len());
        assert_eq!(contextual_lines(&app, "   "), all);
        assert_eq!(
            contextual_lines(&app, "refresh impossible").len(),
            1,
            "unmatched terms produce the explicit no-results row"
        );
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn session_detail_help_uses_normal_mode_bindings() {
        let mut app = with_session_list(1);
        let pane_id = app.active_tab().focused_pane;
        let session_id = app.selected_session_id().expect("selected session");
        *app.active_tab_mut()
            .layout
            .find_pane_mut(pane_id)
            .expect("focused pane") = crate::types::Pane::SessionDetail { session_id };
        let context = ActionContext::from_app(&app);
        assert_eq!(context.origin, HelpOrigin::SessionDetail);
        let help = available_actions(&context)
            .into_iter()
            .find(|action| action.request.id == crate::action_registry::ActionId::ContextHelp)
            .expect("help remains available in detail view");
        assert!(binding_text(&help, &context).contains('?'));
        assert!(
            contextual_lines(&app, "manager policy")
                .iter()
                .any(|line| { format!("{line}").contains("<Space>gp") })
        );

        app.sessions
            .get_mut(&session_id)
            .expect("detail session")
            .input_bar
            .surface
            .mode = crate::types::PopupMode::Insert;
        let input_context = ActionContext::from_app(&app);
        assert_eq!(input_context.mode, crate::action_registry::ActionMode::Text);
        let input_help = available_actions(&input_context)
            .into_iter()
            .find(|action| action.request.id == crate::action_registry::ActionId::ContextHelp)
            .expect("input-bar help remains available");
        assert_eq!(binding_text(&input_help, &input_context), "Ctrl-Alt-G");
        *app.active_tab_mut()
            .layout
            .find_pane_mut(pane_id)
            .expect("focused pane") = crate::types::Pane::PromptCreator;
        let prompt_context = ActionContext::from_app(&app);
        assert_eq!(prompt_context.origin, HelpOrigin::PromptCreator);
        let prompt_help = available_actions(&prompt_context)
            .into_iter()
            .find(|action| action.request.id == crate::action_registry::ActionId::ContextHelp)
            .expect("help remains available in Prompt Creator");
        assert!(binding_text(&prompt_help, &prompt_context).contains('?'));
    }
}
