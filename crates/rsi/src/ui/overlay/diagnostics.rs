//! Diagnostics rendering.

use crate::app::App;
use crate::profiling;
use crate::ui::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph};

use super::tall_centered_rect;

pub(super) fn render_diagnostics(frame: &mut Frame, area: Rect, app: &App) {
    let popup_area = tall_centered_rect(area, 50);
    frame.render_widget(Clear, popup_area);

    let profiling_on = profiling::enabled();
    let m = &app.metrics;

    let fmt_ms = |v: Option<f64>| -> String {
        v.map_or_else(|| "\u{2014}".to_string(), |ms| format!("{ms:.1} ms"))
    };
    let fmt_pct = |v: Option<f64>| -> String {
        v.map_or_else(|| "\u{2014}".to_string(), |r| format!("{:.0}%", r * 100.0))
    };

    let label_style = Style::default().fg(theme::overlay_hint());
    let value_style = Style::default().fg(theme::overlay_title());
    let warn_style = Style::default().fg(theme::warning_status());

    let profile_status = if profiling_on {
        Span::styled("ON", Style::default().fg(theme::status_running()))
    } else {
        Span::styled("OFF  (set RSI_PROFILE=0 to disable)", warn_style)
    };

    let lines = vec![
        Line::from(vec![
            Span::styled("RSI_PROFILE : ", label_style),
            profile_status,
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("render time   : ", label_style),
            Span::styled(fmt_ms(m.last_render_ms), value_style),
        ]),
        Line::from(vec![
            Span::styled("poll time     : ", label_style),
            Span::styled(fmt_ms(m.last_poll_ms), value_style),
        ]),
        Line::from(vec![
            Span::styled("cache hit rate: ", label_style),
            Span::styled(fmt_pct(m.last_cache_hit_rate), value_style),
        ]),
    ]
    .into_iter()
    .chain(draw_stat_lines(app, label_style, value_style))
    .chain([
        Line::from(""),
        Line::from(vec![Span::styled("q / Esc  close", label_style)]),
    ])
    .collect::<Vec<_>>();

    let block = theme::overlay_block()
        .title(Span::styled(
            " diagnostics ",
            Style::default().fg(theme::overlay_title()),
        ))
        .padding(Padding::horizontal(1));

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Draw counts by dirty reason since the TUI started (#332). Reasons that never
/// fired are left out; `marks` can exceed `draws` because marks coalesce.
fn draw_stat_lines(app: &App, label_style: Style, value_style: Style) -> Vec<Line<'static>> {
    let snap = app.draw_stats.snapshot();
    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("frames        : ", label_style),
            Span::styled(
                format!(
                    "{} in {} s (animation {}, unattributed {})",
                    snap.frames, snap.window_secs, snap.animation_frames, snap.unattributed_frames
                ),
                value_style,
            ),
        ]),
    ];
    for (name, counts) in snap.by_reason.iter().filter(|(_, c)| c.marks > 0) {
        lines.push(Line::from(vec![
            Span::styled(format!("  {name:<12}: "), label_style),
            Span::styled(
                format!("{} draws / {} marks", counts.draws, counts.marks),
                value_style,
            ),
        ]));
    }
    lines
}
