//! Read-only cached satellite sessions and operator registry controls.

use crate::types::{SatelliteRegistryForm, SatelliteRegistryOverlayState, SatelliteRegistryTab};
use crate::ui::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph, Wrap};

use super::fixed_centered_rect;

fn safe(value: &str, max_chars: usize) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_control())
        .take(max_chars)
        .collect()
}

/// A form row: the active text field shows the Standard cursor.
fn field_row(
    label: String,
    value: &str,
    selected: bool,
    text_limit: Option<usize>,
) -> Line<'static> {
    let style = if selected {
        Style::default().fg(theme::text()).bg(theme::surface2())
    } else {
        Style::default().fg(theme::subtext0())
    };
    let editing = selected && text_limit.is_some() && crate::field_edit::standard_frame();
    let shown = match text_limit {
        Some(limit) if !editing => safe(value, limit),
        _ => value.to_string(),
    };
    Line::from(crate::field_edit::field_row(&label, &shown, style, editing))
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

pub(super) fn render(frame: &mut Frame, area: Rect, state: &SatelliteRegistryOverlayState) {
    let popup = fixed_centered_rect(
        area,
        area.width.saturating_sub(4).min(140),
        area.height.saturating_sub(4).min(46),
    );
    frame.render_widget(Clear, popup);
    let block = theme::overlay_block()
        .title(Line::from(Span::styled(
            " Satellite registry · hub operator view ",
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

    let mut lines = Vec::new();
    if let Some(form) = &state.form {
        lines.push(Line::from(
            "Tab next field · Space toggle · Enter save · Esc cancel",
        ));
        lines.push(Line::default());
        match form {
            SatelliteRegistryForm::Peer {
                peer_id,
                label,
                expected_installation_id,
                enabled,
                read_enabled,
                dispatch_enabled,
                dispatch_scope,
                repair_quarantine,
                field,
            } => {
                lines.push(Line::from(format!("Peer ID  {peer_id:?}")));
                let limits = [Some(120), Some(36), None, None, None, Some(37 * 64), None];
                for (idx, (name, value)) in [
                    ("Label", label.clone()),
                    ("Expected installation ID", expected_installation_id.clone()),
                    ("Enabled", enabled.to_string()),
                    ("Session reads", read_enabled.to_string()),
                    ("Hub message dispatch", dispatch_enabled.to_string()),
                    (
                        "Dispatch scope (session IDs, comma-separated)",
                        dispatch_scope.clone(),
                    ),
                    (
                        "Acknowledge quarantine repair",
                        repair_quarantine.to_string(),
                    ),
                ]
                .into_iter()
                .enumerate()
                {
                    lines.push(field_row(
                        format!("{} {name}: ", if *field == idx { "▸" } else { " " }),
                        &value,
                        *field == idx,
                        limits[idx],
                    ));
                }
                lines.push(Line::default());
                lines.push(Line::from(
                    "Pair with the probed installation ID before enabling reads.",
                ));
            }
            SatelliteRegistryForm::Inbound { hubs, roots, field } => {
                lines.push(Line::from(
                    "Inbound delivery policy for THIS host (empty refuses all hub messages)",
                ));
                let limits = [Some(37 * 64); 2];
                for (idx, (name, value)) in [
                    (
                        "Allowed hub installation IDs (comma-separated)",
                        hubs.clone(),
                    ),
                    ("Scope root session IDs (comma-separated)", roots.clone()),
                ]
                .into_iter()
                .enumerate()
                {
                    lines.push(field_row(
                        format!("{} {name}: ", if *field == idx { "▸" } else { " " }),
                        &value,
                        *field == idx,
                        limits[idx],
                    ));
                }
            }
            SatelliteRegistryForm::Link {
                link_id,
                socket_path,
                ssh_target,
                trust_reference,
                direction,
                enabled,
                priority,
                field,
                ..
            } => {
                lines.push(Line::from(format!("Link ID  {link_id:?}")));
                let limits = [Some(100), Some(100), Some(100), None, None, None];
                for (idx, (name, value)) in [
                    ("Local socket path", socket_path.clone()),
                    ("SSH target", ssh_target.clone()),
                    ("Trust reference", trust_reference.clone()),
                    ("Direction", format!("{direction:?}")),
                    ("Enabled", enabled.to_string()),
                    ("Priority", priority.to_string()),
                ]
                .into_iter()
                .enumerate()
                {
                    lines.push(field_row(
                        format!("{} {name}: ", if *field == idx { "▸" } else { " " }),
                        &value,
                        *field == idx,
                        limits[idx],
                    ));
                }
                lines.push(Line::default());
                lines.push(Line::from("The socket must be owned by this hub user and contained in the satellite socket root."));
            }
        }
    } else {
        lines.push(Line::from("Tab peers / links / sessions · j/k select · a add · e edit · d enable/disable · R repair · I inbound policy · p probe · r refresh · Esc close"));
        lines.push(Line::from("Remote sessions are display-only; no local edit, terminal, approval or launch action applies."));
        lines.push(Line::default());
        lines.push(Line::from(format!(
            "Registry revision {} · {} peer(s) · selected tab {:?}",
            state.registry.revision,
            state.registry.peers.len(),
            state.tab
        )));
        let peer = state.registry.peers.get(state.selected_peer);
        if let Some(peer) = peer {
            let observation = peer.observation.as_ref();
            let status = observation.map_or("unobserved", |observation| observation.state.as_str());
            let age = observation
                .and_then(|observation| observation.last_observed_at)
                .map(|time| format!("{}s", (chrono::Utc::now() - time).num_seconds().max(0)))
                .unwrap_or_else(|| "never".into());
            lines.push(Line::from(format!(
                "Peer {} · {} · {} · observed {} ago · {}",
                peer.config.peer_id.0,
                safe(&peer.config.label, 80),
                status,
                age,
                if observation.is_some_and(|observation| observation.stale) {
                    "STALE"
                } else {
                    "fresh"
                }
            )));
            if let Some(error) =
                observation.and_then(|observation| observation.last_error.as_deref())
            {
                lines.push(Line::from(format!("Last failure: {}", safe(error, 120))));
            }
        }
        lines.push(Line::default());
        match state.tab {
            SatelliteRegistryTab::Peers => {
                if state.registry.peers.is_empty() {
                    lines.push(Line::from(
                        "No peers registered. Press a to add a disabled peer.",
                    ));
                }
                let window = state.selected_peer.saturating_sub(8);
                for (idx, peer) in state
                    .registry
                    .peers
                    .iter()
                    .enumerate()
                    .skip(window)
                    .take(20)
                {
                    lines.push(row(
                        format!(
                            "{} {}  {}  [{}{}]  {} link(s)",
                            if idx == state.selected_peer {
                                "▸"
                            } else {
                                " "
                            },
                            peer.config.peer_id.0,
                            safe(&peer.config.label, 48),
                            if peer.config.enabled {
                                "enabled"
                            } else {
                                "disabled"
                            },
                            if peer.config.read_enabled {
                                ", reads"
                            } else {
                                ""
                            },
                            peer.links.len()
                        ),
                        idx == state.selected_peer,
                    ));
                }
            }
            SatelliteRegistryTab::Links => {
                if let Some(peer) = peer {
                    if peer.links.is_empty() {
                        lines.push(Line::from(
                            "No links. Press a to add a disabled local socket link.",
                        ));
                    }
                    for (idx, link) in peer
                        .links
                        .iter()
                        .enumerate()
                        .skip(state.selected_link.saturating_sub(8))
                        .take(20)
                    {
                        lines.push(row(
                            format!(
                                "{} {}  {:?}  {}  [{}] priority {}",
                                if idx == state.selected_link {
                                    "▸"
                                } else {
                                    " "
                                },
                                link.link_id.0,
                                link.direction,
                                safe(&link.socket_path, 70),
                                if link.enabled { "enabled" } else { "disabled" },
                                link.priority
                            ),
                            idx == state.selected_link,
                        ));
                    }
                }
                if let Some(probe) = &state.last_probe {
                    lines.push(Line::default());
                    lines.push(Line::from(format!(
                        "Probe: installation {} incarnation {} expected match {:?}",
                        probe.identity.installation_id.0,
                        probe.identity.daemon_incarnation_id.0,
                        probe.matches_expected
                    )));
                }
            }
            SatelliteRegistryTab::Sessions => {
                lines.push(Line::from("n next page · b previous page · r refresh"));
                if let Some(page) = &state.sessions {
                    lines.push(Line::from(format!(
                        "Cached remote sessions: offset {} · {} row(s) · next {:?} · {}",
                        page.offset,
                        page.sessions.len(),
                        page.next_offset,
                        if page.observation.stale {
                            "STALE"
                        } else {
                            "fresh"
                        }
                    )));
                    for (idx, session) in page
                        .sessions
                        .iter()
                        .enumerate()
                        .skip(state.selected_session.saturating_sub(8))
                        .take(20)
                    {
                        lines.push(row(
                            format!(
                                "{} ({}, {})  {:?}  {}",
                                if idx == state.selected_session {
                                    "▸"
                                } else {
                                    " "
                                },
                                session.key.peer_id.0,
                                session.key.remote_session_id.0,
                                session.summary.status,
                                safe(session.summary.title.as_deref().unwrap_or("(untitled)"), 70),
                            ),
                            idx == state.selected_session,
                        ));
                    }
                } else {
                    lines.push(Line::from("No cached page loaded."));
                }
            }
        }
    }
    if let Some(error) = &state.last_error {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            safe(error, 180),
            Style::default().fg(theme::error_status()),
        )));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::satellite::{
        SatelliteHubSessionV1, SatelliteHubSessionsPageV1, SatelliteObservationV1,
        SatellitePeerConfigV1, SatellitePeerV1, SatelliteRegistryV1, SatelliteSessionKeyV1,
        SatelliteSessionSummaryV1, SatelliteUuidV1,
    };
    use rsi_common::types::{SessionProvider, SessionStatus};

    #[test]
    fn stale_remote_session_shows_composite_identity_with_sanitized_label() {
        let peer_id = SatelliteUuidV1(uuid::Uuid::new_v4());
        let remote_id = SatelliteUuidV1(uuid::Uuid::new_v4());
        let now = chrono::Utc::now();
        let observation = SatelliteObservationV1 {
            state: "offline".into(),
            installation_id: None,
            incarnation_id: None,
            last_observed_at: Some(now - chrono::Duration::seconds(90)),
            last_error: None,
            cached_session_count: 1,
            stale: true,
        };
        let state = SatelliteRegistryOverlayState {
            registry: SatelliteRegistryV1 {
                revision: 2,
                peers: vec![SatellitePeerV1 {
                    config: SatellitePeerConfigV1 {
                        peer_id,
                        label: "bad\u{1b}[31mpeer".into(),
                        expected_installation_id: None,
                        enabled: true,
                        read_enabled: true,
                        dispatch_enabled: false,
                    },
                    row_version: 1,
                    links: Vec::new(),
                    observation: Some(observation.clone()),
                    dispatch_scope: Vec::new(),
                }],
            },
            selected_peer: 0,
            selected_link: 0,
            selected_session: 0,
            tab: SatelliteRegistryTab::Sessions,
            sessions: Some(SatelliteHubSessionsPageV1 {
                peer_id,
                observation,
                offset: 0,
                sessions: vec![SatelliteHubSessionV1 {
                    key: SatelliteSessionKeyV1 {
                        peer_id,
                        remote_session_id: remote_id,
                    },
                    summary: SatelliteSessionSummaryV1 {
                        session_id: remote_id,
                        title: Some("remote\u{1b}[31mtitle".into()),
                        provider: SessionProvider::Codex,
                        status: SessionStatus::Running,
                        working_dir: None,
                        created_at: now,
                        updated_at: now,
                    },
                }],
                next_offset: None,
            }),
            form: None,
            last_probe: None,
            last_error: None,
        };
        let backend = ratatui::backend::TestBackend::new(160, 55);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| render(frame, frame.area(), &state))
            .expect("draw browser");
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains(&peer_id.0.to_string()));
        assert!(text.contains(&remote_id.0.to_string()));
        assert!(text.contains("STALE"));
        assert!(text.contains("bad[31mpeer"));
        assert!(text.contains("remote[31mtitle"));
        assert!(!text.contains('\u{1b}'));
    }
}
