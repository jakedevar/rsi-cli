//! RSI Remote settings page (#1096): live status, device and project pickers.

use crate::types::{RemoteFocus, RemoteOverlayState};
use crate::ui::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph, Wrap};
use rsi_common::remote_control::{MAX_REMOTE_PROJECTS, RemoteStatusV1};

use super::fixed_centered_rect;

fn safe(value: &str, max_chars: usize) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_control())
        .take(max_chars)
        .collect()
}

fn yes_no(flag: bool, yes: &'static str, no: &'static str) -> &'static str {
    if flag { yes } else { no }
}

/// Status block as plain text, one fact per line.
pub(crate) fn status_lines(status: &RemoteStatusV1) -> Vec<String> {
    let mut lines = vec![
        format!(
            "Remote: {}   (e: {})",
            yes_no(status.enabled, "ENABLED", "disabled"),
            yes_no(status.enabled, "disable", "enable")
        ),
        format!(
            "Gateway: {}   Serve route: {}   Funnel: {}",
            yes_no(status.gateway_running, "running", "stopped"),
            yes_no(status.serve_route_present, "present", "missing"),
            yes_no(status.funnel_off, "off", "ON (refused)")
        ),
        if status.tailscale.reachable {
            format!(
                "Tailscale: {} {} (qualified {}; {})",
                safe(&status.tailscale.version, 32),
                safe(&status.tailscale.backend_state, 24),
                safe(&status.tailscale.pinned_version, 16),
                yes_no(status.tailscale.version_qualified, "ok", "OUTSIDE line")
            )
        } else {
            "Tailscale: unreachable".to_string()
        },
        match &status.url {
            Some(url) => format!("Open on phone: {}", safe(url, 120)),
            None => "Open on phone: (host unknown until Tailscale is reachable)".to_string(),
        },
    ];
    if let Some(command) = &status.serve_pending_command {
        lines.push(format!(
            "One-time step pending: {}   then press e to enable",
            safe(command, 120)
        ));
    }
    lines
}

/// One picker row: `[x] name (os) online`.
pub(crate) fn device_row(peer: &rsi_common::remote_control::RemotePeerV1) -> String {
    format!(
        "[{}] {} {}{}",
        yes_no(peer.allowed, "x", " "),
        safe(&peer.name, 40),
        if peer.os.is_empty() {
            String::new()
        } else {
            format!("({}) ", safe(&peer.os, 16))
        },
        yes_no(peer.online, "online", "offline")
    )
}

fn row(text: String, selected: bool) -> Line<'static> {
    Line::from(Span::styled(
        text,
        if selected {
            Style::default().fg(theme::text()).bg(theme::surface2())
        } else {
            Style::default().fg(theme::subtext0())
        },
    ))
}

pub(super) fn render(frame: &mut Frame, area: Rect, state: &RemoteOverlayState) {
    let popup = fixed_centered_rect(
        area,
        area.width.saturating_sub(4).min(110),
        area.height.saturating_sub(4).min(40),
    );
    frame.render_widget(Clear, popup);
    let block = theme::overlay_block()
        .title(Line::from(Span::styled(
            " Remote · read-only phone view ",
            Style::default()
                .fg(theme::overlay_title())
                .add_modifier(Modifier::BOLD),
        )))
        .padding(Padding::horizontal(1));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.height == 0 {
        return;
    }
    let status = &state.status;
    let mut lines: Vec<Line> = status_lines(status)
        .into_iter()
        .map(|text| Line::from(Span::styled(text, Style::default().fg(theme::text()))))
        .collect();
    lines.push(Line::default());
    let devices_focused = state.focus == RemoteFocus::Devices;
    lines.push(Line::from(Span::styled(
        format!(
            "Devices ({} allowed){}",
            status.peers.iter().filter(|p| p.allowed).count(),
            yes_no(devices_focused, "  ◀", "")
        ),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    if status.peers.is_empty() {
        lines.push(Line::from("  (no devices on your tailnet yet)"));
    }
    for (index, peer) in status.peers.iter().enumerate() {
        lines.push(row(
            format!("  {}", device_row(peer)),
            devices_focused && index == state.selected_device,
        ));
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        format!(
            "Projects ({}/{} exposed){}",
            status.projects.iter().filter(|p| p.exposed).count(),
            MAX_REMOTE_PROJECTS,
            yes_no(!devices_focused, "  ◀", "")
        ),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    if status.projects.is_empty() {
        lines.push(Line::from("  (no projects)"));
    }
    for (index, project) in status.projects.iter().enumerate() {
        lines.push(row(
            format!(
                "  [{}] {}",
                yes_no(project.exposed, "x", " "),
                safe(&project.name, 60)
            ),
            !devices_focused && index == state.selected_project,
        ));
    }
    for message in status.errors.iter().chain(state.last_error.iter()) {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            safe(message, 200),
            Style::default().fg(theme::error_status()),
        )));
    }
    lines.push(Line::default());
    lines.push(Line::from(
        "Tab list · j/k move · Space toggle · e enable/disable · a apply · r refresh · Esc close",
    ));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::remote_control::{RemotePeerV1, RemoteTailscaleV1};

    #[test]
    fn status_block_shows_state_url_and_pending_step() {
        let status = RemoteStatusV1 {
            enabled: true,
            gateway_running: true,
            serve_route_present: false,
            funnel_off: true,
            url: Some("https://box.ts.net/".into()),
            serve_pending_command: Some("sudo tailscale set --operator=$USER".into()),
            tailscale: RemoteTailscaleV1 {
                reachable: true,
                backend_state: "Running".into(),
                version: "1.102.4".into(),
                pinned_version: "1.102.3+".into(),
                version_qualified: true,
            },
            ..Default::default()
        };
        let text = status_lines(&status).join("\n");
        assert!(text.contains("Remote: ENABLED"));
        assert!(text.contains("Gateway: running   Serve route: missing   Funnel: off"));
        assert!(text.contains("Open on phone: https://box.ts.net/"));
        assert!(text.contains("sudo tailscale set --operator=$USER"));
        assert!(text.contains("1.102.4 Running"));
    }

    #[test]
    fn device_row_marks_allowed_devices() {
        let peer = RemotePeerV1 {
            id: "nA".into(),
            name: "iPhone".into(),
            os: "iOS".into(),
            online: true,
            allowed: true,
        };
        assert_eq!(device_row(&peer), "[x] iPhone (iOS) online");
    }
}
