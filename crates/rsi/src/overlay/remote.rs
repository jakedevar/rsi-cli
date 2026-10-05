//! Operator RSI Remote settings page (#1096). All state lives in the daemon
//! (`RemoteGetStatus` / `RemoteSetConfig`); this page only edits selections
//! and renders what the daemon returns. Operator-only: agents have no verb.

use crate::app::App;
use crate::types::{OverlayState, RemoteFocus, RemoteOverlayState};
use crossterm::event::{KeyCode, KeyEvent};
use rsi_common::remote_control::{RemoteSetConfigRequestV1, RemoteStatusV1};

fn state(app: &mut App) -> Option<&mut RemoteOverlayState> {
    match &mut app.overlay {
        OverlayState::Remote(state) => Some(state),
        _ => None,
    }
}

pub async fn open(app: &mut App) {
    match app.client.remote_get_status().await {
        Ok(status) => {
            app.overlay = OverlayState::Remote(Box::new(RemoteOverlayState {
                status,
                focus: RemoteFocus::Devices,
                selected_device: 0,
                selected_project: 0,
                last_error: None,
            }));
        }
        Err(error) => app.notify_error(format!("Remote status load failed: {error}")),
    }
}

fn install(app: &mut App, status: RemoteStatusV1) {
    if let Some(state) = state(app) {
        state.selected_device = state
            .selected_device
            .min(status.peers.len().saturating_sub(1));
        state.selected_project = state
            .selected_project
            .min(status.projects.len().saturating_sub(1));
        state.status = status;
        state.last_error = None;
    }
}

async fn refresh(app: &mut App) {
    match app.client.remote_get_status().await {
        Ok(status) => install(app, status),
        Err(error) => set_error(app, format!("Refresh failed: {error}")),
    }
}

async fn apply(app: &mut App, request: RemoteSetConfigRequestV1) {
    match app.client.remote_set_config(request).await {
        Ok(status) => install(app, status),
        Err(error) => set_error(app, error.to_string()),
    }
}

fn set_error(app: &mut App, message: String) {
    if let Some(state) = state(app) {
        state.last_error = Some(message);
    }
}

/// The edit that flips the highlighted device.
pub(crate) fn toggled_devices(status: &RemoteStatusV1, index: usize) -> Option<Vec<String>> {
    let target = status.peers.get(index)?;
    Some(
        status
            .peers
            .iter()
            .filter(|peer| {
                (peer.allowed && peer.id != target.id) || (!peer.allowed && peer.id == target.id)
            })
            .map(|peer| peer.id.clone())
            .collect(),
    )
}

/// The edit that flips the highlighted project.
pub(crate) fn toggled_projects(status: &RemoteStatusV1, index: usize) -> Option<Vec<String>> {
    let target = status.projects.get(index)?;
    Some(
        status
            .projects
            .iter()
            .filter(|project| {
                (project.exposed && project.id != target.id)
                    || (!project.exposed && project.id == target.id)
            })
            .map(|project| project.id.clone())
            .collect(),
    )
}

pub async fn handle_key(app: &mut App, key: KeyEvent) {
    let Some(current) = state(app) else { return };
    let focus = current.focus;
    let len = match focus {
        RemoteFocus::Devices => current.status.peers.len(),
        RemoteFocus::Projects => current.status.projects.len(),
    };
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => app.overlay = OverlayState::None,
        KeyCode::Tab | KeyCode::BackTab => {
            current.focus = match focus {
                RemoteFocus::Devices => RemoteFocus::Projects,
                RemoteFocus::Projects => RemoteFocus::Devices,
            };
        }
        KeyCode::Char('j') | KeyCode::Down => {
            let index = match focus {
                RemoteFocus::Devices => &mut current.selected_device,
                RemoteFocus::Projects => &mut current.selected_project,
            };
            *index = (*index + 1).min(len.saturating_sub(1));
        }
        KeyCode::Char('k') | KeyCode::Up => {
            let index = match focus {
                RemoteFocus::Devices => &mut current.selected_device,
                RemoteFocus::Projects => &mut current.selected_project,
            };
            *index = index.saturating_sub(1);
        }
        KeyCode::Char(' ') | KeyCode::Enter => {
            let request = match focus {
                RemoteFocus::Devices => toggled_devices(&current.status, current.selected_device)
                    .map(|ids| RemoteSetConfigRequestV1 {
                        allowed_node_ids: Some(ids),
                        ..Default::default()
                    }),
                RemoteFocus::Projects => {
                    toggled_projects(&current.status, current.selected_project).map(|ids| {
                        RemoteSetConfigRequestV1 {
                            project_ids: Some(ids),
                            ..Default::default()
                        }
                    })
                }
            };
            if let Some(request) = request {
                apply(app, request).await;
            }
        }
        // Enable or disable. Disable takes effect on the gateway's next request.
        KeyCode::Char('e') => {
            let enabled = !current.status.enabled;
            apply(
                app,
                RemoteSetConfigRequestV1 {
                    enabled: Some(enabled),
                    ..Default::default()
                },
            )
            .await;
        }
        // Re-converge gateway and serve route (after the one-time privilege step).
        KeyCode::Char('a') => apply(app, RemoteSetConfigRequestV1::default()).await,
        KeyCode::Char('r') => refresh(app).await,
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::remote_control::{RemotePeerV1, RemoteProjectV1};

    fn status() -> RemoteStatusV1 {
        RemoteStatusV1 {
            peers: vec![
                RemotePeerV1 {
                    id: "nA".into(),
                    allowed: true,
                    ..Default::default()
                },
                RemotePeerV1 {
                    id: "nB".into(),
                    allowed: false,
                    ..Default::default()
                },
            ],
            projects: vec![
                RemoteProjectV1 {
                    id: "p1".into(),
                    exposed: true,
                    ..Default::default()
                },
                RemoteProjectV1 {
                    id: "p2".into(),
                    exposed: false,
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn toggling_a_device_adds_or_removes_exactly_that_id() {
        let status = status();
        assert_eq!(
            toggled_devices(&status, 1),
            Some(vec!["nA".to_string(), "nB".to_string()])
        );
        assert_eq!(toggled_devices(&status, 0), Some(Vec::new()));
        assert_eq!(toggled_devices(&status, 9), None);
    }

    #[test]
    fn toggling_a_project_adds_or_removes_exactly_that_id() {
        let status = status();
        assert_eq!(
            toggled_projects(&status, 1),
            Some(vec!["p1".to_string(), "p2".to_string()])
        );
        assert_eq!(toggled_projects(&status, 0), Some(Vec::new()));
    }
}
