//! Operator-only satellite registry and cached remote-session browser.
//! All transport goes through the hub client; remote rows have no local actions.

use crate::app::App;
use crate::types::{
    OverlayState, SatelliteRegistryForm, SatelliteRegistryOverlayState, SatelliteRegistryTab,
};
use crossterm::event::{KeyCode, KeyEvent};
use rsi_common::satellite::{
    SatelliteHubSessionsRequestV1, SatelliteLinkConfigV1, SatelliteLinkDirectionV1,
    SatellitePeerConfigV1, SatellitePeerV1, SatelliteProbeLinkRequestV1, SatellitePutLinkRequestV1,
    SatellitePutPeerRequestV1, SatelliteUuidV1,
};
use rsi_common::satellite_dispatch::{
    SatelliteInboundPolicyV1, SatellitePutScopeRequestV1, format_satellite_scope_text,
    parse_satellite_scope_text,
};

fn state(app: &mut App) -> Option<&mut SatelliteRegistryOverlayState> {
    match &mut app.overlay {
        OverlayState::SatelliteRegistry(state) => Some(state),
        _ => None,
    }
}

pub async fn open(app: &mut App) {
    match app.client.list_satellite_peers().await {
        Ok(registry) => {
            let scope_status = app.client.get_satellite_scope_status().await.ok();
            app.overlay =
                OverlayState::SatelliteRegistry(Box::new(SatelliteRegistryOverlayState {
                    registry,
                    selected_peer: 0,
                    selected_link: 0,
                    selected_session: 0,
                    tab: SatelliteRegistryTab::Peers,
                    sessions: None,
                    form: None,
                    last_probe: None,
                    scope_status,
                    last_error: None,
                }));
        }
        Err(error) => app.notify_error(format!("Satellite registry load failed: {error}")),
    }
}

async fn refresh(app: &mut App) {
    let selected_id = state(app).and_then(|state| {
        state
            .registry
            .peers
            .get(state.selected_peer)
            .map(|peer| peer.config.peer_id)
    });
    match app.client.list_satellite_peers().await {
        Ok(registry) => {
            let scope_status = app.client.get_satellite_scope_status().await.ok();
            if let Some(state) = state(app) {
                state.scope_status = scope_status;
                state.selected_peer = selected_id
                    .and_then(|id| {
                        registry
                            .peers
                            .iter()
                            .position(|peer| peer.config.peer_id == id)
                    })
                    .unwrap_or(0)
                    .min(registry.peers.len().saturating_sub(1));
                state.registry = registry;
                state.selected_link = state.selected_link.min(
                    state
                        .registry
                        .peers
                        .get(state.selected_peer)
                        .map_or(0, |peer| peer.links.len().saturating_sub(1)),
                );
                state.sessions = None;
                state.last_error = None;
            }
            load_sessions(app, 0).await;
        }
        Err(error) => set_error(app, format!("Registry refresh failed: {error}")),
    }
}

async fn load_sessions(app: &mut App, offset: u32) {
    let selected = state(app).and_then(|state| {
        state
            .registry
            .peers
            .get(state.selected_peer)
            .map(|peer| peer.config.peer_id)
    });
    let Some(peer_id) = selected else { return };
    match app
        .client
        .list_hub_satellite_sessions(SatelliteHubSessionsRequestV1 {
            peer_id,
            offset,
            limit: 30,
        })
        .await
    {
        Ok(page) => {
            if let Some(state) = state(app)
                && state
                    .registry
                    .peers
                    .get(state.selected_peer)
                    .map(|peer| peer.config.peer_id)
                    == Some(peer_id)
            {
                state.sessions = Some(page);
                state.selected_session = 0;
                state.last_error = None;
            }
        }
        Err(error) => set_error(app, format!("Cached session read failed: {error}")),
    }
}

fn set_error(app: &mut App, message: String) {
    if let Some(state) = state(app) {
        state.last_error = Some(message);
    }
}

pub async fn handle_key(app: &mut App, key: KeyEvent) {
    if state(app).is_some_and(|state| state.form.is_some()) {
        handle_form_key(app, key).await;
        return;
    }
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => app.overlay = OverlayState::None,
        KeyCode::Tab => {
            let mut entered_sessions = false;
            if let Some(state) = state(app) {
                state.tab = match state.tab {
                    SatelliteRegistryTab::Peers => SatelliteRegistryTab::Links,
                    SatelliteRegistryTab::Links => SatelliteRegistryTab::Sessions,
                    SatelliteRegistryTab::Sessions => SatelliteRegistryTab::Peers,
                };
                entered_sessions = state.tab == SatelliteRegistryTab::Sessions;
            }
            if entered_sessions {
                load_sessions(app, 0).await;
            }
        }
        KeyCode::Char('j') | KeyCode::Down => move_selection(app, 1).await,
        KeyCode::Char('k') | KeyCode::Up => move_selection(app, -1).await,
        KeyCode::Char('r') => refresh(app).await,
        KeyCode::Char('a') => open_add_form(app),
        KeyCode::Char('e') | KeyCode::Enter => open_edit_form(app),
        KeyCode::Char('d') => toggle_selected(app).await,
        KeyCode::Char('R') => open_repair_form(app),
        KeyCode::Char('I') => open_inbound_form(app).await,
        KeyCode::Char('p') => probe_selected(app).await,
        KeyCode::Char('n') | KeyCode::PageDown => {
            if let Some(offset) = state(app)
                .filter(|state| state.tab == SatelliteRegistryTab::Sessions)
                .and_then(|state| state.sessions.as_ref()?.next_offset)
            {
                load_sessions(app, offset).await;
            }
        }
        KeyCode::Char('b') | KeyCode::PageUp => {
            if let Some(offset) = state(app)
                .filter(|state| state.tab == SatelliteRegistryTab::Sessions)
                .and_then(|state| {
                    state
                        .sessions
                        .as_ref()
                        .map(|page| page.offset.saturating_sub(30))
                })
            {
                load_sessions(app, offset).await;
            }
        }
        _ => {}
    }
}

async fn move_selection(app: &mut App, step: isize) {
    let mut peer_changed = false;
    if let Some(state) = state(app) {
        let (index, len) = match state.tab {
            SatelliteRegistryTab::Peers => (&mut state.selected_peer, state.registry.peers.len()),
            SatelliteRegistryTab::Links => (
                &mut state.selected_link,
                state
                    .registry
                    .peers
                    .get(state.selected_peer)
                    .map_or(0, |peer| peer.links.len()),
            ),
            SatelliteRegistryTab::Sessions => (
                &mut state.selected_session,
                state
                    .sessions
                    .as_ref()
                    .map_or(0, |page| page.sessions.len()),
            ),
        };
        let before = *index;
        *index = index.saturating_add_signed(step).min(len.saturating_sub(1));
        peer_changed = state.tab == SatelliteRegistryTab::Peers && before != *index;
        if peer_changed {
            state.selected_link = 0;
            state.sessions = None;
        }
    }
    if peer_changed {
        load_sessions(app, 0).await;
    }
}

/// Open the satellite-side inbound policy form with the current policy.
async fn open_inbound_form(app: &mut App) {
    match app.client.get_satellite_inbound_policy().await {
        Ok(policy) => {
            if let Some(state) = state(app) {
                state.last_error = None;
                state.form = Some(SatelliteRegistryForm::Inbound {
                    hubs: format_satellite_scope_text(&policy.allowed_hub_installations, &[]),
                    roots: format_satellite_scope_text(&policy.scope_roots, &policy.scope_projects),
                    field: 0,
                });
            }
        }
        Err(error) => set_error(app, format!("Inbound policy read failed: {error}")),
    }
}

fn open_add_form(app: &mut App) {
    let Some(state) = state(app) else { return };
    state.last_error = None;
    state.form =
        match state.tab {
            SatelliteRegistryTab::Peers => Some(SatelliteRegistryForm::Peer {
                peer_id: SatelliteUuidV1(uuid::Uuid::new_v4()),
                label: String::new(),
                expected_installation_id: String::new(),
                enabled: false,
                read_enabled: false,
                dispatch_enabled: false,
                dispatch_scope: String::new(),
                repair_quarantine: false,
                field: 0,
            }),
            SatelliteRegistryTab::Links => {
                state.registry.peers.get(state.selected_peer).map(|peer| {
                    SatelliteRegistryForm::Link {
                        peer_id: peer.config.peer_id,
                        link_id: SatelliteUuidV1(uuid::Uuid::new_v4()),
                        socket_path: String::new(),
                        ssh_target: String::new(),
                        trust_reference: String::new(),
                        direction: SatelliteLinkDirectionV1::DialHomeReverse,
                        enabled: false,
                        priority: 0,
                        field: 0,
                    }
                })
            }
            SatelliteRegistryTab::Sessions => None,
        };
}

fn open_edit_form(app: &mut App) {
    let Some(state) = state(app) else { return };
    let Some(peer) = state.registry.peers.get(state.selected_peer) else {
        return;
    };
    state.last_error = None;
    state.form = match state.tab {
        SatelliteRegistryTab::Peers => Some(SatelliteRegistryForm::Peer {
            peer_id: peer.config.peer_id,
            label: peer.config.label.clone(),
            expected_installation_id: peer
                .config
                .expected_installation_id
                .map_or(String::new(), |id| id.0.to_string()),
            enabled: peer.config.enabled,
            read_enabled: peer.config.read_enabled,
            dispatch_enabled: peer.config.dispatch_enabled,
            dispatch_scope: format_satellite_scope_text(
                &peer.dispatch_scope,
                &peer.dispatch_project_scope,
            ),
            repair_quarantine: false,
            field: 0,
        }),
        SatelliteRegistryTab::Links => {
            peer.links
                .get(state.selected_link)
                .map(|link| SatelliteRegistryForm::Link {
                    peer_id: peer.config.peer_id,
                    link_id: link.link_id,
                    socket_path: link.socket_path.clone(),
                    ssh_target: link.ssh_target.clone().unwrap_or_default(),
                    trust_reference: link.trust_reference.clone(),
                    direction: link.direction,
                    enabled: link.enabled,
                    priority: link.priority,
                    field: 0,
                })
        }
        SatelliteRegistryTab::Sessions => None,
    };
}

fn open_repair_form(app: &mut App) {
    if state(app).is_none_or(|state| state.tab != SatelliteRegistryTab::Peers) {
        return;
    }
    open_edit_form(app);
    if let Some(state) = state(app)
        && let Some(SatelliteRegistryForm::Peer {
            repair_quarantine, ..
        }) = &mut state.form
    {
        *repair_quarantine = true;
    }
}

/// Parse the comma-separated scope field into `(session ids, project ids)`;
/// a `project:<id>` entry is a project whose current appointed manager is the
/// target (#1928).
fn parse_dispatch_scope(
    text: &str,
) -> Result<(Vec<SatelliteUuidV1>, Vec<SatelliteUuidV1>), String> {
    parse_satellite_scope_text(text)
}

/// A plain id list (hub installations): project entries make no sense there.
fn parse_id_list(text: &str) -> Result<Vec<SatelliteUuidV1>, String> {
    let (ids, projects) = parse_satellite_scope_text(text)?;
    if projects.is_empty() {
        Ok(ids)
    } else {
        Err("Hub installation IDs cannot be project: entries".into())
    }
}

/// Build the revision-fenced peer edit for the registry `d`/toggle key.
///
/// Disabling a peer also clears session reads: the daemon's
/// `RegistryPeer::validate` refuses a peer whose `read_enabled` is set
/// while it is not enabled. Without this the
/// submitted config is rejected and the toggle silently fails.
fn toggle_peer_request(peer: &SatellitePeerV1, revision: u64) -> SatellitePutPeerRequestV1 {
    let mut config = peer.config.clone();
    config.enabled = !config.enabled;
    if !config.enabled {
        config.read_enabled = false;
        config.dispatch_enabled = false;
    }
    SatellitePutPeerRequestV1 {
        expected_registry_revision: revision,
        peer: config,
        repair_quarantine: false,
    }
}

async fn toggle_selected(app: &mut App) {
    let Some(state) = state(app) else { return };
    let Some(peer) = state.registry.peers.get(state.selected_peer) else {
        return;
    };
    let revision = state.registry.revision;
    let result = match state.tab {
        SatelliteRegistryTab::Peers => Some((Some(toggle_peer_request(peer, revision)), None)),
        SatelliteRegistryTab::Links => peer.links.get(state.selected_link).map(|link| {
            let mut config = link.clone();
            config.enabled = !config.enabled;
            (
                None,
                Some(SatellitePutLinkRequestV1 {
                    expected_registry_revision: revision,
                    peer_id: peer.config.peer_id,
                    link: config,
                }),
            )
        }),
        SatelliteRegistryTab::Sessions => None,
    };
    let Some((peer, link)) = result else { return };
    let result = if let Some(peer) = peer {
        app.client.put_satellite_peer(peer).await
    } else if let Some(link) = link {
        app.client.put_satellite_link(link).await
    } else {
        return;
    };
    match result {
        Ok(_) => refresh(app).await,
        Err(error) => set_error(app, format!("Satellite edit failed: {error}")),
    }
}

async fn probe_selected(app: &mut App) {
    let selected = state(app)
        .filter(|state| state.tab == SatelliteRegistryTab::Links)
        .and_then(|state| {
            let peer = state.registry.peers.get(state.selected_peer)?;
            let link = peer.links.get(state.selected_link)?;
            Some((peer.config.peer_id, link.link_id))
        });
    let Some((peer_id, link_id)) = selected else {
        return;
    };
    match app
        .client
        .probe_satellite_link(SatelliteProbeLinkRequestV1 { peer_id, link_id })
        .await
    {
        Ok(probe) => {
            if let Some(state) = state(app) {
                state.last_probe = Some(probe);
                state.last_error = None;
            }
        }
        Err(error) => set_error(app, format!("Link probe failed: {error}")),
    }
}

/// The characters a text field accepts.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FieldAlphabet {
    /// Any printable character.
    Free,
    /// Hub id lists: hex digits, `-` and `,` only.
    Ids,
    /// Dispatch scopes and inbound roots: ids plus `project:<uuid>` entries,
    /// so additionally the letters of `project` and `:`.
    Scope,
}

impl FieldAlphabet {
    fn accepts(self, c: char) -> bool {
        if c.is_control() {
            return false;
        }
        match self {
            Self::Free => true,
            Self::Ids => c.is_ascii_hexdigit() || c == '-' || c == ',',
            Self::Scope => {
                c.is_ascii_hexdigit() || matches!(c, '-' | ',' | ':' | ' ') || "project".contains(c)
            }
        }
    }
}

/// Longest id-list field: a `project:<uuid>` entry plus its comma.
const ID_LIST_LIMIT: usize = 46 * 64;

/// The active text field's character limit and alphabet (`None` on toggle and
/// numeric fields).
fn active_text_limit(form: &SatelliteRegistryForm) -> Option<(usize, FieldAlphabet)> {
    match form {
        SatelliteRegistryForm::Peer { field, .. } => match *field {
            0 => Some((120, FieldAlphabet::Free)),
            1 => Some((36, FieldAlphabet::Free)),
            5 => Some((ID_LIST_LIMIT, FieldAlphabet::Scope)),
            _ => None,
        },
        SatelliteRegistryForm::Inbound { field, .. } => Some((
            ID_LIST_LIMIT,
            if *field == 0 {
                FieldAlphabet::Ids
            } else {
                FieldAlphabet::Scope
            },
        )),
        SatelliteRegistryForm::Link { field, .. } => match *field {
            0 => Some((512, FieldAlphabet::Free)),
            1 | 2 => Some((256, FieldAlphabet::Free)),
            _ => None,
        },
    }
}

/// The active text field of the form (see [`active_text_limit`]).
fn active_text_mut(form: &mut SatelliteRegistryForm) -> Option<&mut String> {
    match form {
        SatelliteRegistryForm::Peer {
            label,
            expected_installation_id,
            dispatch_scope,
            field,
            ..
        } => match *field {
            0 => Some(label),
            1 => Some(expected_installation_id),
            5 => Some(dispatch_scope),
            _ => None,
        },
        SatelliteRegistryForm::Inbound { hubs, roots, field } => {
            Some(if *field == 0 { hubs } else { roots })
        }
        SatelliteRegistryForm::Link {
            socket_path,
            ssh_target,
            trust_reference,
            field,
            ..
        } => match *field {
            0 => Some(socket_path),
            1 => Some(ssh_target),
            2 => Some(trust_reference),
            _ => None,
        },
    }
}

async fn handle_form_key(app: &mut App, key: KeyEvent) {
    // Standard editing: a real cursor and selection in the active text field
    // (Enter, Esc, Tab, toggles and the numeric priority keep their handling).
    if let Some((cap, alphabet)) = state(app)
        .and_then(|state| state.form.as_ref())
        .and_then(active_text_limit)
        && app.edit_field_capped(
            key,
            |overlay| match overlay {
                OverlayState::SatelliteRegistry(state) => {
                    state.form.as_mut().and_then(active_text_mut)
                }
                _ => None,
            },
            move |c| alphabet.accepts(c),
            cap,
        ) != crate::field_edit::FieldKey::Ignored
    {
        return;
    }
    match key.code {
        KeyCode::Esc => {
            if let Some(state) = state(app) {
                state.form = None;
            }
        }
        KeyCode::Enter => save_form(app).await,
        KeyCode::Tab | KeyCode::BackTab => {
            if let Some(state) = state(app)
                && let Some(form) = &mut state.form
            {
                let (field, count) = match form {
                    SatelliteRegistryForm::Peer { field, .. } => (field, 7),
                    SatelliteRegistryForm::Link { field, .. } => (field, 6),
                    SatelliteRegistryForm::Inbound { field, .. } => (field, 2),
                };
                *field = if key.code == KeyCode::BackTab {
                    (*field + count - 1) % count
                } else {
                    (*field + 1) % count
                };
            }
        }
        KeyCode::Char(' ') => edit_form(app, ' '),
        KeyCode::Char(ch) => edit_form(app, ch),
        KeyCode::Backspace => {
            if let Some(state) = state(app)
                && let Some(form) = &mut state.form
            {
                match form {
                    SatelliteRegistryForm::Peer {
                        label,
                        expected_installation_id,
                        dispatch_scope,
                        field,
                        ..
                    } => match *field {
                        0 => {
                            label.pop();
                        }
                        1 => {
                            expected_installation_id.pop();
                        }
                        5 => {
                            dispatch_scope.pop();
                        }
                        _ => {}
                    },
                    SatelliteRegistryForm::Inbound { hubs, roots, field } => {
                        if *field == 0 {
                            hubs.pop();
                        } else {
                            roots.pop();
                        }
                    }
                    SatelliteRegistryForm::Link {
                        socket_path,
                        ssh_target,
                        trust_reference,
                        priority,
                        field,
                        ..
                    } => match *field {
                        0 => {
                            socket_path.pop();
                        }
                        1 => {
                            ssh_target.pop();
                        }
                        2 => {
                            trust_reference.pop();
                        }
                        5 => *priority /= 10,
                        _ => {}
                    },
                }
            }
        }
        _ => {}
    }
}

fn edit_form(app: &mut App, ch: char) {
    if ch.is_control() {
        return;
    }
    let Some(state) = state(app) else { return };
    let Some(form) = &mut state.form else { return };
    match form {
        SatelliteRegistryForm::Peer {
            label,
            expected_installation_id,
            enabled,
            read_enabled,
            dispatch_enabled,
            dispatch_scope,
            repair_quarantine,
            field,
            ..
        } => match *field {
            0 if label.len() < 120 => label.push(ch),
            1 if expected_installation_id.len() < 36 => expected_installation_id.push(ch),
            2 if ch == ' ' => *enabled = !*enabled,
            3 if ch == ' ' => *read_enabled = !*read_enabled,
            4 if ch == ' ' => *dispatch_enabled = !*dispatch_enabled,
            5 if FieldAlphabet::Scope.accepts(ch) && dispatch_scope.len() < ID_LIST_LIMIT => {
                dispatch_scope.push(ch);
            }
            6 if ch == ' ' => *repair_quarantine = !*repair_quarantine,
            _ => {}
        },
        SatelliteRegistryForm::Inbound { hubs, roots, field } => {
            let alphabet = if *field == 0 {
                FieldAlphabet::Ids
            } else {
                FieldAlphabet::Scope
            };
            let target = if *field == 0 { hubs } else { roots };
            if alphabet.accepts(ch) && target.len() < ID_LIST_LIMIT {
                target.push(ch);
            }
        }
        SatelliteRegistryForm::Link {
            socket_path,
            ssh_target,
            trust_reference,
            direction,
            enabled,
            priority,
            field,
            ..
        } => match *field {
            0 if socket_path.len() < 512 => socket_path.push(ch),
            1 if ssh_target.len() < 256 => ssh_target.push(ch),
            2 if trust_reference.len() < 256 => trust_reference.push(ch),
            3 if ch == ' ' => {
                *direction = match direction {
                    SatelliteLinkDirectionV1::DialHomeReverse => {
                        SatelliteLinkDirectionV1::DirectLocalForward
                    }
                    SatelliteLinkDirectionV1::DirectLocalForward => {
                        SatelliteLinkDirectionV1::DialHomeReverse
                    }
                }
            }
            4 if ch == ' ' => *enabled = !*enabled,
            5 if ch.is_ascii_digit() => {
                *priority = u8::try_from(u32::from(*priority) * 10 + ch.to_digit(10).unwrap_or(0))
                    .unwrap_or(u8::MAX);
            }
            _ => {}
        },
    }
}

pub(super) fn paste_text(app: &mut App, text: &str) -> bool {
    if state(app).is_none_or(|state| state.form.is_none()) {
        return false;
    }
    for ch in text.chars() {
        edit_form(app, ch);
    }
    true
}

async fn save_form(app: &mut App) {
    let (form, revision) = {
        let Some(state) = state(app) else { return };
        let Some(form) = state.form.take() else {
            return;
        };
        (form, state.registry.revision)
    };
    let result = match &form {
        SatelliteRegistryForm::Peer {
            peer_id,
            label,
            expected_installation_id,
            enabled,
            read_enabled,
            dispatch_enabled,
            dispatch_scope,
            repair_quarantine,
            ..
        } => {
            let expected = if expected_installation_id.is_empty() {
                Ok(None)
            } else {
                serde_json::from_value::<SatelliteUuidV1>(serde_json::Value::String(
                    expected_installation_id.clone(),
                ))
                .map(Some)
                .map_err(|error| error.to_string())
            };
            let scope = parse_dispatch_scope(dispatch_scope);
            match (expected, scope) {
                (Ok(expected_installation_id), Ok((scope_ids, scope_projects))) => {
                    let saved = app
                        .client
                        .put_satellite_peer(SatellitePutPeerRequestV1 {
                            expected_registry_revision: revision,
                            peer: SatellitePeerConfigV1 {
                                peer_id: *peer_id,
                                label: label.clone(),
                                expected_installation_id,
                                enabled: *enabled,
                                read_enabled: *read_enabled,
                                dispatch_enabled: *dispatch_enabled,
                            },
                            repair_quarantine: *repair_quarantine,
                        })
                        .await
                        .map_err(|error| error.to_string());
                    match saved {
                        Ok(new_revision) => app
                            .client
                            .put_satellite_peer_scope(SatellitePutScopeRequestV1 {
                                expected_registry_revision: new_revision,
                                peer_id: *peer_id,
                                remote_session_ids: scope_ids,
                                remote_project_ids: scope_projects,
                            })
                            .await
                            .map_err(|error| error.to_string()),
                        Err(error) => Err(error),
                    }
                }
                (Err(error), _) => Err(format!("Invalid installation ID: {error}")),
                (_, Err(error)) => Err(error),
            }
        }
        SatelliteRegistryForm::Inbound { hubs, roots, .. } => {
            match (parse_id_list(hubs), parse_dispatch_scope(roots)) {
                (Ok(allowed_hub_installations), Ok((scope_roots, scope_projects))) => app
                    .client
                    .put_satellite_inbound_policy(SatelliteInboundPolicyV1 {
                        allowed_hub_installations,
                        scope_roots,
                        scope_projects,
                    })
                    .await
                    .map(|()| revision)
                    .map_err(|error| error.to_string()),
                (Err(error), _) | (_, Err(error)) => Err(error),
            }
        }
        SatelliteRegistryForm::Link {
            peer_id,
            link_id,
            socket_path,
            ssh_target,
            trust_reference,
            direction,
            enabled,
            priority,
            ..
        } => app
            .client
            .put_satellite_link(SatellitePutLinkRequestV1 {
                expected_registry_revision: revision,
                peer_id: *peer_id,
                link: SatelliteLinkConfigV1 {
                    link_id: *link_id,
                    direction: *direction,
                    socket_path: socket_path.clone(),
                    ssh_target: (!ssh_target.is_empty()).then(|| ssh_target.clone()),
                    trust_reference: trust_reference.clone(),
                    enabled: *enabled,
                    priority: *priority,
                },
            })
            .await
            .map_err(|error| error.to_string()),
    };
    match result {
        Ok(_) => refresh(app).await,
        Err(error) => {
            if let Some(state) = state(app) {
                state.form = Some(form);
                state.last_error = Some(error);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use rsi_common::satellite::{
        SatelliteHubSessionV1, SatelliteHubSessionsPageV1, SatelliteObservationV1,
        SatelliteRegistryV1, SatelliteSessionKeyV1, SatelliteSessionSummaryV1,
    };
    use rsi_common::types::{SessionProvider, SessionStatus};

    #[test]
    fn dispatch_scope_field_parses_ids_and_rejects_garbage() {
        let a = uuid::Uuid::new_v4();
        let b = uuid::Uuid::new_v4();
        let (parsed, projects) = parse_dispatch_scope(&format!("{a}, {b},")).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[1].0, b);
        assert!(projects.is_empty());
        assert!(parse_dispatch_scope("").unwrap().0.is_empty());
        assert!(parse_dispatch_scope("not-a-uuid").is_err());
    }

    #[test]
    fn dispatch_scope_field_accepts_project_entries_but_hub_lists_do_not() {
        let session = uuid::Uuid::new_v4();
        let project = uuid::Uuid::new_v4();
        let (sessions, projects) =
            parse_dispatch_scope(&format!("{session},project:{project}")).unwrap();
        assert_eq!(sessions, vec![SatelliteUuidV1(session)]);
        assert_eq!(projects, vec![SatelliteUuidV1(project)]);
        assert!(parse_id_list(&format!("project:{project}")).is_err());
        assert_eq!(parse_id_list(&session.to_string()).unwrap().len(), 1);
    }

    #[test]
    fn disabling_a_peer_also_switches_dispatch_off() {
        let peer = SatellitePeerV1 {
            config: SatellitePeerConfigV1 {
                peer_id: SatelliteUuidV1(uuid::Uuid::new_v4()),
                label: "laptop".into(),
                expected_installation_id: Some(SatelliteUuidV1(uuid::Uuid::new_v4())),
                enabled: true,
                read_enabled: true,
                dispatch_enabled: true,
            },
            row_version: 1,
            links: Vec::new(),
            observation: None,
            dispatch_scope: Vec::new(),
            dispatch_project_scope: Vec::new(),
        };
        let request = toggle_peer_request(&peer, 3);
        assert!(!request.peer.enabled);
        assert!(!request.peer.dispatch_enabled);
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Pressing `d` on an enabled, read-enabled peer must submit a config the
    /// daemon accepts: disabled with session reads cleared.
    #[test]
    fn disabling_a_read_enabled_peer_clears_session_reads() {
        let peer = SatellitePeerV1 {
            config: SatellitePeerConfigV1 {
                peer_id: SatelliteUuidV1(uuid::Uuid::new_v4()),
                label: "build box".into(),
                expected_installation_id: Some(SatelliteUuidV1(uuid::Uuid::new_v4())),
                enabled: true,
                read_enabled: true,
                dispatch_enabled: false,
            },
            row_version: 1,
            links: vec![],
            observation: None,
            dispatch_scope: Vec::new(),
            dispatch_project_scope: Vec::new(),
        };
        let request = toggle_peer_request(&peer, 7);
        assert_eq!(request.expected_registry_revision, 7);
        assert!(!request.peer.enabled, "the peer is disabled");
        assert!(!request.peer.read_enabled, "session reads are cleared");
        // Mirrors `RegistryPeer::validate`: session reads require an
        // enabled, paired peer, so the submitted config is accepted.
        assert!(!request.peer.read_enabled || request.peer.enabled);
    }

    #[tokio::test]
    async fn cached_remote_rows_never_enter_local_session_actions() {
        let mut app = crate::app::app_test_helpers::with_session_list(1);
        let peer_id = SatelliteUuidV1(uuid::Uuid::new_v4());
        let remote_id = SatelliteUuidV1(uuid::Uuid::new_v4());
        let now = chrono::Utc::now();
        let observation = SatelliteObservationV1 {
            sessions_running: None,
            workers_parked: None,
            open_jobs: None,
            platform: None,
            state: "healthy".into(),
            installation_id: None,
            incarnation_id: None,
            last_observed_at: Some(now),
            last_error: None,
            cached_session_count: 1,
            stale: false,
            manager_launches: Vec::new(),
        };
        app.overlay = OverlayState::SatelliteRegistry(Box::new(SatelliteRegistryOverlayState {
            registry: SatelliteRegistryV1 {
                revision: 1,
                peers: vec![rsi_common::satellite::SatellitePeerV1 {
                    config: SatellitePeerConfigV1 {
                        peer_id,
                        label: "test".into(),
                        expected_installation_id: None,
                        enabled: true,
                        read_enabled: true,
                        dispatch_enabled: false,
                    },
                    row_version: 1,
                    links: vec![],
                    observation: Some(observation.clone()),
                    dispatch_scope: Vec::new(),
                    dispatch_project_scope: Vec::new(),
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
                        title: Some("remote".into()),
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
            scope_status: None,
            last_error: None,
        }));
        let before = app.pending_lc_actions.len();
        for code in [
            KeyCode::Enter,
            KeyCode::Char('e'),
            KeyCode::Char('d'),
            KeyCode::Char('p'),
        ] {
            handle_key(&mut app, key(code)).await;
        }
        assert_eq!(app.pending_lc_actions.len(), before);
        assert!(matches!(
            app.overlay,
            OverlayState::SatelliteRegistry(ref state)
                if state.tab == SatelliteRegistryTab::Sessions
                    && state.sessions.as_ref().is_some_and(|page| page.sessions[0].key.remote_session_id == remote_id)
        ));
    }

    fn form_overlay(form: SatelliteRegistryForm) -> OverlayState {
        OverlayState::SatelliteRegistry(Box::new(SatelliteRegistryOverlayState {
            registry: SatelliteRegistryV1 {
                revision: 1,
                peers: Vec::new(),
            },
            selected_peer: 0,
            selected_link: 0,
            selected_session: 0,
            tab: SatelliteRegistryTab::Peers,
            sessions: None,
            form: Some(form),
            last_probe: None,
            scope_status: None,
            last_error: None,
        }))
    }

    fn peer_form(field: usize) -> SatelliteRegistryForm {
        SatelliteRegistryForm::Peer {
            peer_id: SatelliteUuidV1(uuid::Uuid::new_v4()),
            label: String::new(),
            expected_installation_id: String::new(),
            enabled: false,
            read_enabled: false,
            dispatch_enabled: true,
            dispatch_scope: String::new(),
            repair_quarantine: false,
            field,
        }
    }

    fn peer_scope_text(app: &mut App) -> String {
        match state(app).and_then(|state| state.form.as_ref()) {
            Some(SatelliteRegistryForm::Peer { dispatch_scope, .. }) => dispatch_scope.clone(),
            _ => panic!("peer form is open"),
        }
    }

    /// Pasting `<session>,project:<uuid>` into the dispatch-scope field keeps
    /// the whole text, and the saved scope parses to both entries (#2829).
    #[test]
    fn pasted_project_scope_entry_survives_the_dispatch_scope_field() {
        let mut app = crate::app::app_test_helpers::with_session_list(1);
        let session = uuid::Uuid::new_v4();
        let project = uuid::Uuid::new_v4();
        app.overlay = form_overlay(peer_form(5));
        let pasted = format!("{session},project:{project}");
        assert!(paste_text(&mut app, &pasted));
        let text = peer_scope_text(&mut app);
        assert_eq!(text, pasted);
        let (sessions, projects) = parse_dispatch_scope(&text).unwrap();
        assert_eq!(sessions, vec![SatelliteUuidV1(session)]);
        assert_eq!(projects, vec![SatelliteUuidV1(project)]);
    }

    /// Typing `project:<uuid>` key by key reaches the field unchanged.
    #[tokio::test]
    async fn typed_project_scope_entry_reaches_the_dispatch_scope_field() {
        let mut app = crate::app::app_test_helpers::with_session_list(1);
        let project = uuid::Uuid::new_v4();
        app.overlay = form_overlay(peer_form(5));
        let typed = format!("project:{project}");
        for ch in typed.chars() {
            handle_key(&mut app, key(KeyCode::Char(ch))).await;
        }
        let text = peer_scope_text(&mut app);
        assert_eq!(text, typed);
        let (sessions, projects) = parse_dispatch_scope(&text).unwrap();
        assert!(sessions.is_empty());
        assert_eq!(projects, vec![SatelliteUuidV1(project)]);
    }

    /// The inbound roots field takes project entries; the hub list keeps the
    /// narrow id alphabet.
    #[test]
    fn inbound_roots_accept_project_entries_and_hubs_keep_the_id_alphabet() {
        let mut app = crate::app::app_test_helpers::with_session_list(1);
        let project = uuid::Uuid::new_v4();
        app.overlay = form_overlay(SatelliteRegistryForm::Inbound {
            hubs: String::new(),
            roots: String::new(),
            field: 1,
        });
        let pasted = format!("project:{project}");
        assert!(paste_text(&mut app, &pasted));
        let roots = match state(&mut app).and_then(|state| state.form.as_ref()) {
            Some(SatelliteRegistryForm::Inbound { roots, .. }) => roots.clone(),
            _ => panic!("inbound form is open"),
        };
        assert_eq!(roots, pasted);
        assert_eq!(
            parse_dispatch_scope(&roots).unwrap().1,
            vec![SatelliteUuidV1(project)]
        );

        let hub = uuid::Uuid::new_v4();
        let hub_form = |field| SatelliteRegistryForm::Inbound {
            hubs: String::new(),
            roots: String::new(),
            field,
        };
        assert!(matches!(
            active_text_limit(&hub_form(0)),
            Some((_, FieldAlphabet::Ids))
        ));
        assert!(matches!(
            active_text_limit(&hub_form(1)),
            Some((_, FieldAlphabet::Scope))
        ));
        app.overlay = form_overlay(hub_form(0));
        assert!(paste_text(&mut app, &hub.to_string()));
        let hubs = match state(&mut app).and_then(|state| state.form.as_ref()) {
            Some(SatelliteRegistryForm::Inbound { hubs, .. }) => hubs.clone(),
            _ => panic!("inbound form is open"),
        };
        assert_eq!(parse_id_list(&hubs).unwrap(), vec![SatelliteUuidV1(hub)]);
    }
}
