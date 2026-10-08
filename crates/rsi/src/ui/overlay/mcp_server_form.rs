//! MCP server definition and credential form rendering.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph};

use crate::ui::theme;

use super::fixed_centered_rect;

fn mask_secret(secret: &str) -> String {
    "*".repeat(secret.chars().count())
}

pub(super) fn render_mcp_server_form(
    frame: &mut Frame,
    area: Rect,
    focused_field: usize,
    id: &str,
    command: &str,
    args: &str,
    secret_env_names: &str,
    working_dir: &str,
    enabled: bool,
) {
    let area = super::scoped_popup_rect(area, area);
    frame.render_widget(Clear, area);
    let block = theme::overlay_block()
        .title(" MCP Server ")
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height < 8 {
        return;
    }

    let fields = [
        ("ID", id),
        ("Command", command),
        ("Args (comma-separated)", args),
        ("Secret env names", secret_env_names),
        ("Working dir", working_dir),
        ("Enabled", if enabled { "on" } else { "off" }),
    ];
    for (index, (label, value)) in fields.iter().enumerate() {
        let value_style = Style::default().fg(theme::text());
        let mut spans = vec![Span::styled(
            format!("{label}: "),
            Style::default()
                .fg(theme::mauve())
                .add_modifier(Modifier::BOLD),
        )];
        if focused_field == index && index < 5 {
            spans.extend(crate::field_edit::draw(value, value_style));
        } else {
            spans.push(Span::styled(value.to_string(), value_style));
            spans.push(Span::styled(
                if focused_field == index { "█" } else { "" },
                value_style,
            ));
        }
        let row = Line::from(spans);
        frame.render_widget(
            Paragraph::new(row),
            Rect::new(inner.x, inner.y + index as u16, inner.width, 1),
        );
    }
}

pub(super) fn render_mcp_server_secret_form(
    frame: &mut Frame,
    area: Rect,
    id: &str,
    rotate: bool,
    secret: &str,
) {
    let popup_area = fixed_centered_rect(area, 60, 7);
    frame.render_widget(Clear, popup_area);
    let title = if rotate {
        format!(" Rotate {id} MCP Credential ")
    } else {
        format!(" Set {id} MCP Credential ")
    };
    let block = theme::overlay_block()
        .title(title)
        .padding(Padding::horizontal(1));
    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);
    if inner.height < 3 {
        return;
    }

    let row = Line::from(vec![
        Span::styled(
            "Secret: ",
            Style::default()
                .fg(theme::mauve())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(mask_secret(secret), Style::default().fg(theme::text())),
        Span::styled("█", Style::default().fg(theme::text())),
    ]);
    frame.render_widget(
        Paragraph::new(row),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn mask_secret_replaces_every_character() {
        let masked = mask_secret("mcp-secret-canary");
        assert_eq!(masked.chars().count(), "mcp-secret-canary".chars().count());
        assert!(masked.chars().all(|character| character == '*'));
    }

    #[test]
    fn mcp_secret_form_renders_masked_input_only() {
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).expect("test terminal");
        terminal
            .draw(|frame| {
                render_mcp_server_secret_form(
                    frame,
                    frame.area(),
                    "docs",
                    false,
                    "mcp-secret-canary",
                );
            })
            .expect("render MCP secret form");
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol().to_string())
            .collect::<String>();
        assert!(rendered.contains("docs"));
        assert_eq!(rendered.matches('*').count(), "mcp-secret-canary".len());
        assert!(!rendered.contains("mcp-secret-canary"));
    }
}
