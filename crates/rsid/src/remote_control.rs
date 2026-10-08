//! Operator-side lifecycle for RSI Remote (#1096).
//!
//! The settings page drives two operator-only RPCs (`RemoteGetStatus`,
//! `RemoteSetConfig`). This module owns what they do: detect owner and host
//! from Tailscale, write the gateway's policy file (the single source of truth
//! the gateway re-reads per request), manage a systemd *user* unit for the
//! gateway (never a session child), and add or remove the `tailscale serve`
//! route. Funnel is never enabled; a Funnel-on host is reported, not fixed.
//!
//! Every system effect sits behind [`RemoteSystem`] so the policy logic is
//! tested with a fake host.

use rsi_common::remote_control::{
    MAX_REMOTE_PROJECTS, QUALIFIED_TAILSCALE_LINE, QUALIFIED_TAILSCALE_MIN_PATCH, RemotePeerV1,
    RemoteProjectV1, RemoteSetConfigRequestV1, RemoteStatusV1, RemoteTailscaleV1,
    tailscale_version_qualified,
};
use rsi_remote::config::{self, Config};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant, SystemTime},
};

pub const UNIT_NAME: &str = "rsi-remote.service";
/// The one-time step that lets `tailscale serve` run without sudo.
pub const OPERATOR_COMMAND: &str = "sudo tailscale set --operator=$USER";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UnitState {
    pub installed: bool,
    pub active: bool,
}

/// Every effect on the host. Blocking; callers use `spawn_blocking`.
pub trait RemoteSystem: Send + Sync {
    fn policy_path(&self) -> PathBuf;
    fn socket_path(&self) -> PathBuf;
    fn gateway_binary(&self) -> Option<PathBuf>;
    fn tailscale_status_json(&self) -> Result<String, String>;
    fn tailscale_serve_status_json(&self) -> Result<String, String>;
    fn tailscale_serve_apply(&self, socket: &Path) -> Result<(), String>;
    /// Remove only the `/` mount on https 443 (never other mounts).
    fn tailscale_serve_remove_root(&self) -> Result<(), String>;
    fn unit_state(&self) -> UnitState;
    /// Whether a persistent unit file of our name exists on disk.
    fn unit_file_exists(&self) -> bool;
    fn unit_install_and_start(&self, unit_text: &str) -> Result<(), String>;
    fn unit_stop_and_disable(&self) -> Result<(), String>;
    /// Delete our unit file (only if it carries the rsid marker).
    fn unit_remove_file(&self) -> Result<(), String>;
    /// When the unit's main process started; `None` when unknown or inactive.
    fn unit_started_at(&self) -> Option<SystemTime>;
    /// Modification time of the gateway binary (symlinks followed).
    fn binary_modified(&self, binary: &Path) -> Option<SystemTime>;
    fn unit_restart(&self) -> Result<(), String>;
}

/// The running gateway predates its binary: a deploy or install replaced the
/// file after the process started. Whole seconds on both sides (systemd
/// reports seconds), so a start in the same second as the write never counts
/// and a restart cannot repeat. Unknown times never count.
fn gateway_outdated(started: Option<SystemTime>, modified: Option<SystemTime>) -> bool {
    let secs = |time: SystemTime| {
        time.duration_since(SystemTime::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .ok()
    };
    matches!(
        (started.and_then(secs), modified.and_then(secs)),
        (Some(started), Some(modified)) if modified > started
    )
}

struct TsPeer {
    id: String,
    name: String,
    os: String,
    online: bool,
    user_id: u64,
}

struct TsInfo {
    backend_state: String,
    version: String,
    self_id: String,
    user_id: u64,
    host: String,
    peers: Vec<TsPeer>,
}

fn parse_tailscale_status(json: &str) -> Result<TsInfo, String> {
    let root: Value =
        serde_json::from_str(json).map_err(|_| "tailscale status was not JSON".to_string())?;
    let text = |value: Option<&Value>| value.and_then(Value::as_str).unwrap_or("").to_string();
    let me = root.get("Self").cloned().unwrap_or(Value::Null);
    let peers = root
        .get("Peer")
        .and_then(Value::as_object)
        .map(|map| {
            map.values()
                .filter_map(|peer| {
                    let id = text(peer.get("ID"));
                    (!id.is_empty()).then(|| TsPeer {
                        id,
                        name: {
                            let host = text(peer.get("HostName"));
                            if host.is_empty() {
                                text(peer.get("DNSName"))
                            } else {
                                host
                            }
                        },
                        os: text(peer.get("OS")),
                        online: peer.get("Online").and_then(Value::as_bool).unwrap_or(false),
                        user_id: peer.get("UserID").and_then(Value::as_u64).unwrap_or(0),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(TsInfo {
        backend_state: text(root.get("BackendState")),
        version: text(root.get("Version")),
        self_id: text(me.get("ID")),
        user_id: me.get("UserID").and_then(Value::as_u64).unwrap_or(0),
        host: text(me.get("DNSName"))
            .trim_end_matches('.')
            .to_ascii_lowercase(),
        peers,
    })
}

fn load_policy(system: &dyn RemoteSystem, errors: &mut Vec<String>) -> Config {
    let path = system.policy_path();
    if std::fs::symlink_metadata(&path).is_err() {
        return Config::default();
    }
    match config::read(&path) {
        Ok(policy) => policy,
        Err(error) => {
            errors.push(format!("policy file {} unusable: {error}", path.display()));
            Config::default()
        }
    }
}

/// True when the serve config has Funnel switched on for `host:443`.
fn funnel_on(serve_json: &str, host: &str) -> bool {
    serde_json::from_str::<Value>(serve_json)
        .ok()
        .and_then(|root| root.get("AllowFunnel").cloned())
        .and_then(|funnel| funnel.get(format!("{host}:443")).cloned())
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

/// Build the status the settings page renders. Read-only.
pub fn status(system: &dyn RemoteSystem, projects: &[(String, String)]) -> RemoteStatusV1 {
    let mut errors = Vec::new();
    let policy = load_policy(system, &mut errors);
    let ts = match system
        .tailscale_status_json()
        .and_then(|json| parse_tailscale_status(&json))
    {
        Ok(info) => Some(info),
        Err(error) => {
            errors.push(format!("Tailscale unavailable: {error}"));
            None
        }
    };
    let canonical_host = if policy.canonical_host.is_empty() {
        ts.as_ref().map(|t| t.host.clone()).unwrap_or_default()
    } else {
        policy.canonical_host.clone()
    };
    let allowed: BTreeSet<&str> = policy.allowed_node_ids.iter().map(String::as_str).collect();
    let mut peers: Vec<RemotePeerV1> = ts
        .as_ref()
        .map(|t| {
            t.peers
                .iter()
                // The gateway only admits devices owned by the same Tailscale
                // user as this host; offering others would be a dead choice.
                .filter(|p| p.user_id == t.user_id)
                .map(|p| RemotePeerV1 {
                    id: p.id.clone(),
                    name: p.name.clone(),
                    os: p.os.clone(),
                    online: p.online,
                    allowed: allowed.contains(p.id.as_str()),
                })
                .collect()
        })
        .unwrap_or_default();
    for id in &policy.allowed_node_ids {
        if !peers.iter().any(|p| &p.id == id) {
            peers.push(RemotePeerV1 {
                id: id.clone(),
                name: "(not on this tailnet)".into(),
                os: String::new(),
                online: false,
                allowed: true,
            });
        }
    }
    let exposed: BTreeSet<&str> = policy.project_ids.iter().map(String::as_str).collect();
    let mut project_rows: Vec<RemoteProjectV1> = projects
        .iter()
        .map(|(id, name)| RemoteProjectV1 {
            id: id.clone(),
            name: name.clone(),
            exposed: exposed.contains(id.as_str()),
        })
        .collect();
    for id in &policy.project_ids {
        if !project_rows.iter().any(|p| &p.id == id) {
            project_rows.push(RemoteProjectV1 {
                id: id.clone(),
                name: "(unknown project)".into(),
                exposed: true,
            });
        }
    }
    let socket = system.socket_path();
    let (serve_route_present, funnel_off) = match system.tailscale_serve_status_json() {
        Ok(json) => (
            !canonical_host.is_empty()
                && rsi_remote::localapi::serve_route_ok(json.as_bytes(), &canonical_host, &socket)
                    .is_ok(),
            !funnel_on(&json, &canonical_host),
        ),
        // No readable serve config means no route; Funnel cannot be proven on.
        Err(_) => (false, true),
    };
    let unit = system.unit_state();
    let tailscale = RemoteTailscaleV1 {
        reachable: ts.is_some(),
        backend_state: ts
            .as_ref()
            .map(|t| t.backend_state.clone())
            .unwrap_or_default(),
        version: ts.as_ref().map(|t| t.version.clone()).unwrap_or_default(),
        pinned_version: format!("{QUALIFIED_TAILSCALE_LINE}.{QUALIFIED_TAILSCALE_MIN_PATCH}+"),
        version_qualified: ts
            .as_ref()
            .is_some_and(|t| tailscale_version_qualified(&t.version)),
    };
    if ts.is_some() && !tailscale.version_qualified {
        errors.push(format!(
            "Tailscale {} is outside the qualified {} line",
            tailscale.version, tailscale.pinned_version
        ));
    }
    if !funnel_off {
        errors.push(
            "Funnel is ON for this host; RSI Remote refuses to serve. Run `tailscale funnel --https=443 off`."
                .into(),
        );
    }
    RemoteStatusV1 {
        enabled: policy.enabled,
        url: (!canonical_host.is_empty()).then(|| format!("https://{canonical_host}/")),
        canonical_host,
        unit_installed: unit.installed,
        gateway_running: unit.active,
        serve_route_present,
        funnel_off,
        serve_pending_command: (policy.enabled && !serve_route_present)
            .then(|| OPERATOR_COMMAND.to_string()),
        tailscale,
        peers,
        projects: project_rows,
        errors,
    }
}

fn canonical_uuid(s: &str) -> bool {
    uuid::Uuid::parse_str(s).is_ok_and(|id| id.to_string() == s)
}

/// First line of every unit we write. A unit file without it is foreign and
/// is never replaced, stopped or removed.
const UNIT_MARKER: &str = "# Managed by rsid: RSI Remote gateway (#1096). Edits are overwritten.";

/// The unit text. Relative, non-UTF-8, whitespace, quote, backslash and
/// control characters are refused; `%` and `$` are escaped for systemd.
///
/// No `PrivateTmp=`/`PrivateUsers=` (#1639): in a systemd *user* manager they
/// put the gateway in a private user namespace that maps only this user, so
/// root `tailscaled` shows up as the overflow uid and fails the gateway's
/// uid-0 peer check on every request. `Restart=always` because a clean
/// SIGTERM (an operator killing processes by name) is not a failure, yet it
/// left the gateway down.
fn unit_text(binary: &Path, policy: &Path) -> Result<String, String> {
    let clean = |path: &Path| {
        let text = path
            .to_str()
            .ok_or_else(|| format!("path {} is not valid UTF-8", path.display()))?;
        if !path.is_absolute()
            || text
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '"' | '\'' | '\\'))
        {
            return Err(format!("path {text} is unsupported in a systemd unit"));
        }
        Ok(text.replace('%', "%%").replace('$', "$$"))
    };
    let (binary, policy) = (clean(binary)?, clean(policy)?);
    Ok(format!(
        "{UNIT_MARKER}\n\
[Unit]\n\
Description=RSI Remote read-only gateway (managed by rsid)\n\
After=network-online.target\n\
\n\
[Service]\n\
Type=simple\n\
ExecStartPre=-/usr/bin/rm -f %t/rsi-remote/ingress.sock\n\
ExecStart={binary} run {policy} %t/rsi-remote/ingress.sock\n\
RuntimeDirectory=rsi-remote\n\
RuntimeDirectoryMode=0700\n\
Restart=always\n\
RestartSec=3\n\
NoNewPrivileges=yes\n\
\n\
[Install]\n\
WantedBy=default.target\n"
    ))
}

/// Most devices one policy may allow; keeps the file well under the reader's
/// size limit (the writer also enforces the byte bound).
pub const MAX_REMOTE_DEVICES: usize = 64;

fn validate_selection(
    nodes: &[String],
    project_ids: &[String],
    known_projects: &BTreeSet<&str>,
) -> Result<(), String> {
    if nodes.iter().any(|id| id.is_empty() || id.len() > 128) {
        return Err("invalid device id".into());
    }
    if nodes.len() > MAX_REMOTE_DEVICES {
        return Err(format!(
            "at most {MAX_REMOTE_DEVICES} devices can be allowed"
        ));
    }
    if project_ids.len() > MAX_REMOTE_PROJECTS {
        return Err(format!(
            "at most {MAX_REMOTE_PROJECTS} projects can be exposed"
        ));
    }
    for id in project_ids {
        if !canonical_uuid(id) {
            return Err(format!("invalid project id {id}"));
        }
        if !known_projects.contains(id.as_str()) {
            return Err(format!("unknown project {id}"));
        }
    }
    Ok(())
}

fn dedup(ids: &[String]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    ids.iter()
        .filter(|id| seen.insert((*id).clone()))
        .cloned()
        .collect()
}

fn write_policy(system: &dyn RemoteSystem, policy: &Config) -> Result<(), String> {
    let path = system.policy_path();
    ensure_policy_dir(&path)?;
    config::write(&path, policy, false).map_err(|e| format!("cannot write policy: {e}"))
}

fn ensure_policy_dir(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && std::fs::symlink_metadata(parent).is_err()
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    Ok(())
}

/// What currently holds `host:443` in the Tailscale serve config.
#[derive(Debug, PartialEq, Eq)]
enum Route {
    /// Nothing at all on 443 for this host.
    Empty,
    /// The `/` mount proxies to our socket. `exclusive` is false when other
    /// handlers share the listener; `funnel` mirrors `AllowFunnel`.
    Owned { exclusive: bool, funnel: bool },
    /// Something else owns it; the message says what.
    Foreign(String),
}

/// Classify the 443 listener independent of whether the gateway would accept
/// it: ownership is decided only by the `/` handler's proxy target.
fn classify_route(json: &str, host: &str, socket: &Path) -> Result<Route, String> {
    let trimmed = json.trim();
    let root: Value = if trimmed.is_empty() || trimmed == "null" {
        serde_json::json!({})
    } else {
        serde_json::from_str(trimmed).map_err(|_| "serve config was not JSON".to_string())?
    };
    let Some(object) = root.as_object() else {
        return Err("serve config was not an object".into());
    };
    let key = format!("{host}:443");
    let funnel = object
        .get("AllowFunnel")
        .and_then(|funnel| funnel.get(&key))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let live = |value: Option<&Value>| value.filter(|v| !v.is_null()).cloned();
    let tcp = live(object.get("TCP").and_then(|tcp| tcp.get("443")));
    let web = live(object.get("Web").and_then(|web| web.get(&key)));
    let (Some(tcp), Some(web)) = (tcp.clone(), web.clone()) else {
        return Ok(if tcp.is_none() && web.is_none() {
            Route::Empty
        } else {
            Route::Foreign("port 443 is already configured for another purpose".into())
        });
    };
    if tcp.get("HTTPS").and_then(Value::as_bool) != Some(true) {
        return Ok(Route::Foreign(
            "port 443 is not an HTTPS listener we created".into(),
        ));
    }
    let Some(handlers) = web.get("Handlers").and_then(Value::as_object) else {
        return Ok(Route::Foreign("port 443 has no readable handlers".into()));
    };
    let expected = format!("unix:{}", socket.display());
    Ok(match handlers.get("/") {
        Some(root_handler)
            if root_handler.get("Proxy").and_then(Value::as_str) == Some(expected.as_str()) =>
        {
            Route::Owned {
                exclusive: handlers.len() == 1,
                funnel,
            }
        }
        Some(_) => Route::Foreign("https://<host>/ is already served by another service".into()),
        None => Route::Foreign("port 443 already has other handlers".into()),
    })
}

/// Remove the route only when this socket owns the `/` mount, whatever else
/// (Funnel, extra handlers) surrounds it. Never touches unrelated mounts.
fn remove_owned_route(system: &dyn RemoteSystem, hosts: &[String], notes: &mut Vec<String>) {
    let json = match system.tailscale_serve_status_json() {
        Ok(json) => json,
        Err(error) => {
            notes.push(format!("cannot read serve config to remove route: {error}"));
            return;
        }
    };
    let socket = system.socket_path();
    let owned = hosts.iter().filter(|host| !host.is_empty()).any(|host| {
        matches!(
            classify_route(&json, host, &socket),
            Ok(Route::Owned { .. })
        )
    });
    if owned && let Err(error) = system.tailscale_serve_remove_root() {
        notes.push(format!("tailscale serve remove: {error}"));
    }
}

fn hosts_for_cleanup(system: &dyn RemoteSystem, policy: &Config) -> Vec<String> {
    let mut hosts = vec![policy.canonical_host.clone()];
    if let Ok(info) = system
        .tailscale_status_json()
        .and_then(|json| parse_tailscale_status(&json))
        && !hosts.contains(&info.host)
    {
        hosts.push(info.host);
    }
    hosts
}

/// Undo what a failed enable newly created. The policy is disabled first so
/// access is closed before any slower step; then newly created effects are
/// removed in reverse order. Every failure is reported, none is swallowed.
fn roll_back_enable(
    system: &dyn RemoteSystem,
    policy: &Config,
    prior_unit: UnitState,
    unit_file_existed: bool,
    created_route: bool,
    hosts: &[String],
    notes: &mut Vec<String>,
) {
    let mut disabled = policy.clone();
    disabled.enabled = false;
    if let Err(error) = write_policy(system, &disabled).or_else(|first| {
        config::write_replacing_unreadable(&system.policy_path(), &disabled)
            .map_err(|e| format!("{first}; {e}"))
    }) {
        notes.push(format!("ROLLBACK FAILED to disable the policy: {error}"));
    }
    if created_route {
        remove_owned_route(system, hosts, notes);
    }
    if !prior_unit.active
        && let Err(error) = system.unit_stop_and_disable()
    {
        notes.push(format!("rollback: stopping gateway unit: {error}"));
    }
    if !unit_file_existed
        && system.unit_file_exists()
        && let Err(error) = system.unit_remove_file()
    {
        notes.push(format!("rollback: removing gateway unit file: {error}"));
    }
}

/// Apply an operator edit, then converge the gateway and serve route to the
/// resulting policy. Validation and preflight refusals are `Err` with no host
/// change. A failure after effects began rolls back to a disabled policy and
/// is reported in the returned status' `errors`.
pub fn set_config(
    system: &dyn RemoteSystem,
    projects: &[(String, String)],
    request: &RemoteSetConfigRequestV1,
) -> Result<RemoteStatusV1, String> {
    let policy_path = system.policy_path();
    let disable_only = request.enabled == Some(false)
        && request.allowed_node_ids.is_none()
        && request.project_ids.is_none();
    let mut recovering = false;
    let mut policy = if std::fs::symlink_metadata(&policy_path).is_err() {
        Config::default()
    } else {
        match config::read(&policy_path) {
            Ok(policy) => policy,
            // A bad file must never block turning Remote off.
            Err(_) if disable_only => {
                recovering = true;
                Config::default()
            }
            Err(error) => return Err(format!("existing policy unusable: {error}")),
        }
    };
    let known: BTreeSet<&str> = projects.iter().map(|(id, _)| id.as_str()).collect();
    if let Some(nodes) = &request.allowed_node_ids {
        policy.allowed_node_ids = dedup(nodes);
    }
    if let Some(ids) = &request.project_ids {
        policy.project_ids = dedup(ids);
    }
    let checked_projects: &[String] = if request.project_ids.is_some() {
        &policy.project_ids
    } else {
        &[]
    };
    validate_selection(&policy.allowed_node_ids, checked_projects, &known)?;
    let want_enabled = request.enabled.unwrap_or(policy.enabled);
    if want_enabled {
        enable(system, projects, policy)
    } else {
        disable(system, projects, policy, recovering)
    }
}

fn enable(
    system: &dyn RemoteSystem,
    projects: &[(String, String)],
    mut policy: Config,
) -> Result<RemoteStatusV1, String> {
    if policy.allowed_node_ids.is_empty() {
        return Err("pick at least one device before enabling Remote".into());
    }
    if policy.project_ids.is_empty() {
        return Err("pick at least one project before enabling Remote".into());
    }
    let ts = system
        .tailscale_status_json()
        .and_then(|json| parse_tailscale_status(&json))
        .map_err(|e| format!("Tailscale unavailable: {e}"))?;
    if ts.backend_state != "Running" || ts.self_id.is_empty() || ts.user_id == 0 {
        return Err(format!(
            "Tailscale is not running (state {:?})",
            ts.backend_state
        ));
    }
    if ts.host.split('.').count() < 2 {
        return Err("Tailscale host name is unknown; enable MagicDNS".into());
    }
    let binary = system.gateway_binary().ok_or_else(|| {
        "rsi-remote binary not found; run `make release-install` (or cargo install --path crates/rsi-remote --locked)".to_string()
    })?;
    let unit = unit_text(&binary, &system.policy_path())?;
    policy.enabled = true;
    policy.owner_user_id = ts.user_id;
    policy.canonical_host = ts.host.clone();
    config::encode(&policy).map_err(|e| format!("policy cannot be saved: {e}"))?;

    // Never replace a route we do not own, and never act on an unreadable one.
    let socket = system.socket_path();
    let serve_json = system.tailscale_serve_status_json().map_err(|e| {
        format!("cannot read the Tailscale serve config ({e}); refusing to change it")
    })?;
    if funnel_on(&serve_json, &policy.canonical_host) {
        return Err(
            "Funnel is ON for this host; RSI Remote refuses to serve. Run `tailscale funnel --https=443 off` first."
                .into(),
        );
    }
    let created_route = match classify_route(&serve_json, &policy.canonical_host, &socket)? {
        Route::Empty => true,
        Route::Owned {
            exclusive: true, ..
        } => false,
        Route::Owned {
            exclusive: false, ..
        } => {
            return Err(
                "other handlers share https port 443; the gateway needs the only handler there. Remove them first."
                    .into(),
            );
        }
        Route::Foreign(what) => {
            return Err(format!(
                "refusing to enable: {what}. RSI Remote will not replace it; free it first (tailscale serve --https=443 off)."
            ));
        }
    };
    let prior_unit = system.unit_state();
    let unit_file_existed = system.unit_file_exists();

    write_policy(system, &policy)?;
    let attempt = (|| -> Result<(), (String, bool)> {
        system
            .unit_install_and_start(&unit)
            .map_err(|e| (format!("gateway unit: {e}"), false))?;
        if !system.unit_state().active {
            return Err(("gateway unit did not become active".into(), false));
        }
        if created_route {
            system
                .tailscale_serve_apply(&socket)
                .map_err(|e| (format!("tailscale serve: {e}"), true))?;
        }
        let json = system
            .tailscale_serve_status_json()
            .map_err(|e| (format!("cannot verify the serve route: {e}"), false))?;
        match classify_route(&json, &policy.canonical_host, &socket) {
            Ok(Route::Owned {
                exclusive: true,
                funnel: false,
            }) => Ok(()),
            other => Err((format!("serve route verification failed: {other:?}"), false)),
        }
    })();
    match attempt {
        Ok(()) => Ok(status(system, projects)),
        Err((message, pending)) => {
            let mut notes = vec![message];
            let hosts = hosts_for_cleanup(system, &policy);
            roll_back_enable(
                system,
                &policy,
                prior_unit,
                unit_file_existed,
                created_route,
                &hosts,
                &mut notes,
            );
            let mut out = status(system, projects);
            out.errors.extend(notes);
            if pending {
                out.serve_pending_command = Some(OPERATOR_COMMAND.to_string());
            }
            Ok(out)
        }
    }
}

fn disable(
    system: &dyn RemoteSystem,
    projects: &[(String, String)],
    mut policy: Config,
    recovering: bool,
) -> Result<RemoteStatusV1, String> {
    // Persist first: the gateway re-reads the policy per request, so access
    // stops before any slower step runs.
    let was_enabled = policy.enabled;
    policy.enabled = false;
    if recovering {
        let path = system.policy_path();
        ensure_policy_dir(&path)?;
        config::write_replacing_unreadable(&path, &policy)
            .map_err(|e| format!("cannot replace unreadable policy: {e}"))?;
    } else {
        write_policy(system, &policy)?;
    }
    let mut notes = Vec::new();
    if (was_enabled || system.unit_state().installed)
        && let Err(error) = system.unit_stop_and_disable()
    {
        notes.push(format!("gateway unit: {error}"));
    }
    let hosts = hosts_for_cleanup(system, &policy);
    remove_owned_route(system, &hosts, &mut notes);
    let mut out = status(system, projects);
    out.errors.extend(notes);
    Ok(out)
}

/// What [`converge`] found and did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Converged {
    /// Remote is off or has no policy file: nothing was touched.
    Disabled,
    /// The gateway was already active (its unit file was refreshed, and the
    /// unit restarted, only if the text had drifted).
    AlreadyActive,
    /// The gateway was not active and has been started.
    Started,
    /// The gateway was active on a binary older than the installed one and
    /// has been restarted onto it.
    Restarted,
}

/// Bring the gateway unit back in line with the policy after an rsid start,
/// which also follows every deploy restart and `make release-install` (#1639).
/// When Remote is enabled the managed unit is rewritten if its text drifted,
/// started if it is not active, and restarted if its process is older than
/// the installed binary (a deploy replaced the file under it). It never
/// enables Remote, never stops or disables the unit and never touches the
/// `tailscale serve` route.
pub fn converge(system: &dyn RemoteSystem) -> Result<Converged, String> {
    let path = system.policy_path();
    if std::fs::symlink_metadata(&path).is_err() {
        return Ok(Converged::Disabled);
    }
    let policy =
        config::read(&path).map_err(|e| format!("policy file {} unusable: {e}", path.display()))?;
    if !policy.enabled {
        return Ok(Converged::Disabled);
    }
    let binary = system
        .gateway_binary()
        .ok_or_else(|| "rsi-remote binary not found".to_string())?;
    let unit = unit_text(&binary, &path)?;
    let was_active = system.unit_state().active;
    system
        .unit_install_and_start(&unit)
        .map_err(|e| format!("gateway unit: {e}"))?;
    // Checked after the install: a drifted unit was already restarted there.
    let restarted =
        was_active && gateway_outdated(system.unit_started_at(), system.binary_modified(&binary));
    if restarted {
        system
            .unit_restart()
            .map_err(|e| format!("gateway restart: {e}"))?;
    }
    if !system.unit_state().active {
        return Err("gateway unit did not become active".into());
    }
    Ok(match (was_active, restarted) {
        (_, true) => Converged::Restarted,
        (true, false) => Converged::AlreadyActive,
        (false, false) => Converged::Started,
    })
}

/// Serializes every status read and policy+lifecycle mutation: one operation
/// runs to completion before the next starts, so a stale read can never undo
/// a later disable or revocation. The lock is owned by the blocking task, so a
/// disconnecting client cannot release it while host effects are in flight.
pub struct RemoteController {
    system: std::sync::Arc<dyn RemoteSystem>,
    lock: std::sync::Arc<tokio::sync::Mutex<()>>,
}

impl RemoteController {
    pub fn new(system: std::sync::Arc<dyn RemoteSystem>) -> Self {
        Self {
            system,
            lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    pub async fn status(&self, projects: Vec<(String, String)>) -> Result<RemoteStatusV1, String> {
        let guard = std::sync::Arc::clone(&self.lock).lock_owned().await;
        let system = std::sync::Arc::clone(&self.system);
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            status(system.as_ref(), &projects)
        })
        .await
        .map_err(|e| format!("remote status task failed: {e}"))
    }

    /// [`converge`] under the same lock as every operator edit, so a
    /// concurrent disable cannot be undone by a stale startup read.
    pub async fn converge(&self) -> Result<Converged, String> {
        let guard = std::sync::Arc::clone(&self.lock).lock_owned().await;
        let system = std::sync::Arc::clone(&self.system);
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            converge(system.as_ref())
        })
        .await
        .map_err(|e| format!("remote converge task failed: {e}"))?
    }

    pub async fn set_config(
        &self,
        projects: Vec<(String, String)>,
        request: RemoteSetConfigRequestV1,
    ) -> Result<RemoteStatusV1, String> {
        let guard = std::sync::Arc::clone(&self.lock).lock_owned().await;
        let system = std::sync::Arc::clone(&self.system);
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            set_config(system.as_ref(), &projects, &request)
        })
        .await
        .map_err(|e| format!("remote config task failed: {e}"))?
    }
}

// ---- real host --------------------------------------------------------

pub struct HostSystem;

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(".config"))
}

const STDOUT_LIMIT: usize = 4 * 1024 * 1024;
const STDERR_LIMIT: usize = 16 * 1024;

/// Read a pipe to EOF, keeping at most `limit` bytes. Always drains, so the
/// child can never block on a full pipe.
fn drain<R: std::io::Read + Send + 'static>(
    mut reader: R,
    limit: usize,
) -> std::sync::mpsc::Receiver<(Vec<u8>, bool)> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let (mut kept, mut over) = (Vec::new(), false);
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let room = limit.saturating_sub(kept.len());
                    kept.extend_from_slice(&buf[..n.min(room)]);
                    over |= n > room;
                }
            }
        }
        let _ = tx.send((kept, over));
    });
    rx
}

fn run(program: &str, args: &[&str], timeout: Duration) -> Result<String, String> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        command.env(
            "XDG_RUNTIME_DIR",
            format!("/run/user/{}", nix::unistd::getuid()),
        );
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot run {program}: {e}"))?;
    let stdout = child.stdout.take().map(|pipe| drain(pipe, STDOUT_LIMIT));
    let stderr = child.stderr.take().map(|pipe| drain(pipe, STDERR_LIMIT));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{program} timed out"));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => return Err(format!("{program}: {e}")),
        }
    };
    let collect = |rx: Option<std::sync::mpsc::Receiver<(Vec<u8>, bool)>>| {
        rx.and_then(|rx| rx.recv_timeout(Duration::from_secs(2)).ok())
            .unwrap_or_default()
    };
    let (out, out_over) = collect(stdout);
    let (err, _) = collect(stderr);
    if out_over {
        return Err(format!("{program} output exceeded {STDOUT_LIMIT} bytes"));
    }
    if status.success() {
        Ok(String::from_utf8_lossy(&out).into_owned())
    } else {
        let stderr = String::from_utf8_lossy(&err);
        let detail = stderr
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("failed");
        Err(format!("{program} {}: {detail}", args.join(" ")))
    }
}

fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Create or replace the managed unit file: user-owned directory, never a
/// symlink or foreign file, same-directory temp file with explicit 0644 mode,
/// synced, then renamed. Returns whether the file changed.
fn install_unit_file(dir: &Path, text: &str) -> Result<bool, String> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
    let uid = nix::unistd::getuid().as_raw();
    if std::fs::symlink_metadata(dir).is_err() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    // `metadata` follows a symlinked directory (dotfile managers do this).
    let meta = std::fs::metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o022 != 0 {
        return Err(format!("unsafe unit directory {}", dir.display()));
    }
    let path = dir.join(UNIT_NAME);
    match std::fs::symlink_metadata(&path) {
        Ok(existing) => {
            if !existing.file_type().is_file() || existing.uid() != uid {
                return Err(format!(
                    "refusing to replace {}: not a regular file we own",
                    path.display()
                ));
            }
            let content =
                std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            if !content.contains(UNIT_MARKER) {
                return Err(format!(
                    "{} exists and is not managed by rsid; refusing to replace it",
                    path.display()
                ));
            }
            if content == text && existing.mode() & 0o777 == 0o644 {
                return Ok(false);
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{}: {e}", path.display())),
    }
    let temp = dir.join(format!(".{UNIT_NAME}.{}.tmp", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&temp)
        .map_err(|e| format!("cannot create {}: {e}", temp.display()))?;
    let result = (|| -> std::io::Result<()> {
        file.set_permissions(std::fs::Permissions::from_mode(0o644))?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temp, &path)
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&temp);
        return Err(format!("cannot write {}: {e}", path.display()));
    }
    Ok(true)
}

/// Remove the unit file only when it is a regular file of ours with the marker.
fn remove_unit_file(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let path = dir.join(UNIT_NAME);
    let uid = nix::unistd::getuid().as_raw();
    let Ok(existing) = std::fs::symlink_metadata(&path) else {
        return Ok(());
    };
    if !existing.file_type().is_file()
        || existing.uid() != uid
        || !std::fs::read_to_string(&path).is_ok_and(|text| text.contains(UNIT_MARKER))
    {
        return Err(format!(
            "{} is not managed by rsid; left alone",
            path.display()
        ));
    }
    std::fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))
}

fn user_unit_dir() -> PathBuf {
    config_home().join("systemd/user")
}

/// A persistent unit by our name that is not ours must not be stopped.
fn foreign_persistent_unit() -> bool {
    let path = user_unit_dir().join(UNIT_NAME);
    std::fs::symlink_metadata(&path).is_ok()
        && !std::fs::read_to_string(&path).is_ok_and(|text| text.contains(UNIT_MARKER))
}

impl RemoteSystem for HostSystem {
    fn policy_path(&self) -> PathBuf {
        config_home().join("rsi-remote/policy.toml")
    }

    fn socket_path(&self) -> PathBuf {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", nix::unistd::getuid())));
        runtime.join("rsi-remote/ingress.sock")
    }

    fn gateway_binary(&self) -> Option<PathBuf> {
        let mut candidates = vec![home().join(".local/bin/rsi-remote")];
        if let Ok(exe) = std::env::current_exe()
            && let Some(dir) = exe.parent()
        {
            candidates.push(dir.join("rsi-remote"));
        }
        candidates.push(home().join(".cargo/bin/rsi-remote"));
        if let Ok(found) = which::which("rsi-remote") {
            candidates.push(found);
        }
        candidates.into_iter().find(|p| executable(p))
    }

    fn tailscale_status_json(&self) -> Result<String, String> {
        run("tailscale", &["status", "--json"], Duration::from_secs(10))
    }

    fn tailscale_serve_status_json(&self) -> Result<String, String> {
        run(
            "tailscale",
            &["serve", "status", "--json"],
            Duration::from_secs(10),
        )
    }

    fn tailscale_serve_apply(&self, socket: &Path) -> Result<(), String> {
        let target = format!("unix:{}", socket.display());
        run(
            "tailscale",
            &["serve", "--yes", "--bg", "--https=443", &target],
            Duration::from_secs(20),
        )
        .map(|_| ())
        .map_err(|error| format!("{error}. One-time setup, then enable again: {OPERATOR_COMMAND}"))
    }

    fn tailscale_serve_remove_root(&self) -> Result<(), String> {
        // Only the `/` mount: unrelated mounts on 443 survive.
        run(
            "tailscale",
            &["serve", "--yes", "--https=443", "--set-path=/", "off"],
            Duration::from_secs(20),
        )
        .map(|_| ())
    }

    fn unit_state(&self) -> UnitState {
        let Ok(out) = run(
            "systemctl",
            &[
                "--user",
                "show",
                UNIT_NAME,
                "-p",
                "LoadState",
                "-p",
                "ActiveState",
            ],
            Duration::from_secs(5),
        ) else {
            return UnitState::default();
        };
        UnitState {
            installed: out.lines().any(|l| l == "LoadState=loaded"),
            active: out.lines().any(|l| l == "ActiveState=active"),
        }
    }

    fn unit_file_exists(&self) -> bool {
        std::fs::symlink_metadata(user_unit_dir().join(UNIT_NAME)).is_ok()
    }

    fn unit_install_and_start(&self, unit_text: &str) -> Result<(), String> {
        // A hand-started transient unit of the same name would shadow ours;
        // stop it only when it is plainly the rsi-remote gateway.
        let transient = run(
            "systemctl",
            &["--user", "show", UNIT_NAME, "-p", "Transient", "--value"],
            Duration::from_secs(5),
        )
        .is_ok_and(|v| v.trim() == "yes");
        if transient {
            let exec = run(
                "systemctl",
                &["--user", "show", UNIT_NAME, "-p", "ExecStart", "--value"],
                Duration::from_secs(5),
            )
            .unwrap_or_default();
            if !exec.contains("rsi-remote") {
                return Err(format!(
                    "a transient {UNIT_NAME} that is not rsi-remote exists; refusing to replace it"
                ));
            }
        }
        let changed = install_unit_file(&user_unit_dir(), unit_text)?;
        if transient {
            run(
                "systemctl",
                &["--user", "stop", UNIT_NAME],
                Duration::from_secs(20),
            )?;
        }
        if changed {
            run(
                "systemctl",
                &["--user", "daemon-reload"],
                Duration::from_secs(10),
            )?;
        }
        run(
            "systemctl",
            &["--user", "enable", "--now", UNIT_NAME],
            Duration::from_secs(20),
        )?;
        if changed {
            run(
                "systemctl",
                &["--user", "restart", UNIT_NAME],
                Duration::from_secs(20),
            )?;
        }
        Ok(())
    }

    fn unit_stop_and_disable(&self) -> Result<(), String> {
        if foreign_persistent_unit() {
            return Err(format!(
                "{UNIT_NAME} is not managed by rsid; refusing to stop it"
            ));
        }
        run(
            "systemctl",
            &["--user", "disable", "--now", UNIT_NAME],
            Duration::from_secs(20),
        )
        .map(|_| ())
        .or_else(|error| {
            // Transient (hand-started) units cannot be disabled; stop them.
            run(
                "systemctl",
                &["--user", "stop", UNIT_NAME],
                Duration::from_secs(20),
            )
            .map(|_| ())
            .map_err(|_| error)
        })
    }

    fn unit_remove_file(&self) -> Result<(), String> {
        remove_unit_file(&user_unit_dir())?;
        run(
            "systemctl",
            &["--user", "daemon-reload"],
            Duration::from_secs(10),
        )
        .map(|_| ())
    }

    fn unit_started_at(&self) -> Option<SystemTime> {
        // `@<unix seconds>`; empty when the unit never ran. An older systemd
        // without `--timestamp=unix` fails here and yields `None`.
        let out = run(
            "systemctl",
            &[
                "--user",
                "show",
                UNIT_NAME,
                "--timestamp=unix",
                "-p",
                "ExecMainStartTimestamp",
                "--value",
            ],
            Duration::from_secs(5),
        )
        .ok()?;
        let secs: u64 = out.trim().strip_prefix('@')?.parse().ok()?;
        (secs > 0).then(|| SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
    }

    fn binary_modified(&self, binary: &Path) -> Option<SystemTime> {
        std::fs::metadata(binary).and_then(|m| m.modified()).ok()
    }

    fn unit_restart(&self) -> Result<(), String> {
        run(
            "systemctl",
            &["--user", "restart", UNIT_NAME],
            Duration::from_secs(20),
        )
        .map(|_| ())
    }
}

/// Convenience for the RPC layer: projects as `(id, name)` rows.
pub fn project_rows(projects: &[rsi_common::types::Project]) -> Vec<(String, String)> {
    projects
        .iter()
        .map(|p| (p.id.to_string(), p.name.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Condvar, Mutex};

    const HOST: &str = "box.example.ts.net";
    const OWNER: u64 = 4242;
    const PHONE: &str = "nPHONE";
    const LAPTOP: &str = "nLAPTOP";
    const STRANGER: &str = "nSTRANGER";
    const PROJECT_A: &str = "550e8400-e29b-41d4-a716-446655440000";
    const PROJECT_B: &str = "550e8400-e29b-41d4-a716-446655440001";

    #[derive(Default)]
    struct FakeState {
        serve_json: Option<String>,
        serve_apply_error: Option<String>,
        /// The apply "times out" after already changing the host.
        serve_apply_partial: bool,
        unit_start_error: Option<String>,
        /// The unit starts without error but never becomes active.
        unit_never_active: bool,
        unit: UnitState,
        unit_file: bool,
        installed_unit_text: Option<String>,
        calls: Vec<&'static str>,
        funnel: bool,
        version: String,
        /// The first `tailscale status` call parks until released.
        park_status: bool,
        unit_started_at: Option<SystemTime>,
        binary_modified: Option<SystemTime>,
    }

    struct Fake {
        dir: tempfile::TempDir,
        state: Mutex<FakeState>,
        gate: (Mutex<bool>, Condvar),
    }

    impl Fake {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let config_dir = dir.path().join("cfg");
            std::fs::create_dir(&config_dir).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&config_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            Self {
                dir,
                state: Mutex::new(FakeState {
                    version: "1.102.4".into(),
                    serve_json: Some("{}".into()),
                    ..FakeState::default()
                }),
                gate: (Mutex::new(false), Condvar::new()),
            }
        }

        fn route_json(&self, funnel: bool) -> String {
            let socket = self.socket_path();
            serde_json::json!({
                "TCP": {"443": {"HTTPS": true}},
                "Web": {format!("{HOST}:443"): {"Handlers": {"/": {"Proxy": format!("unix:{}", socket.display())}}}},
                "AllowFunnel": {format!("{HOST}:443"): funnel},
            })
            .to_string()
        }

        fn set_serve(&self, json: &str) {
            self.state.lock().unwrap().serve_json = Some(json.to_string());
        }

        fn calls(&self) -> Vec<&'static str> {
            self.state.lock().unwrap().calls.clone()
        }

        fn release(&self) {
            *self.gate.0.lock().unwrap() = true;
            self.gate.1.notify_all();
        }
    }

    impl RemoteSystem for Fake {
        fn policy_path(&self) -> PathBuf {
            self.dir.path().join("cfg/policy.toml")
        }
        fn socket_path(&self) -> PathBuf {
            self.dir.path().join("rt/ingress.sock")
        }
        fn gateway_binary(&self) -> Option<PathBuf> {
            Some(PathBuf::from("/opt/rsi/bin/rsi-remote"))
        }
        fn tailscale_status_json(&self) -> Result<String, String> {
            let park = std::mem::take(&mut self.state.lock().unwrap().park_status);
            if park {
                let mut released = self.gate.0.lock().unwrap();
                while !*released {
                    released = self.gate.1.wait(released).unwrap();
                }
            }
            let state = self.state.lock().unwrap();
            Ok(serde_json::json!({
                "Version": state.version,
                "BackendState": "Running",
                "Self": {"ID": "nSELF", "UserID": OWNER, "DNSName": format!("{HOST}.")},
                "Peer": {
                    "k1": {"ID": PHONE, "HostName": "iPhone", "OS": "iOS", "Online": true, "UserID": OWNER},
                    "k2": {"ID": LAPTOP, "HostName": "laptop", "OS": "linux", "Online": false, "UserID": OWNER},
                    "k3": {"ID": STRANGER, "HostName": "shared-in", "OS": "linux", "Online": true, "UserID": 7},
                }
            })
            .to_string())
        }
        fn tailscale_serve_status_json(&self) -> Result<String, String> {
            let state = self.state.lock().unwrap();
            state
                .serve_json
                .clone()
                .ok_or_else(|| "no serve config".into())
        }
        fn tailscale_serve_apply(&self, _socket: &Path) -> Result<(), String> {
            let (funnel, error, partial) = {
                let mut state = self.state.lock().unwrap();
                state.calls.push("serve_apply");
                (
                    state.funnel,
                    state.serve_apply_error.clone(),
                    state.serve_apply_partial,
                )
            };
            if let Some(error) = error {
                if partial {
                    self.state.lock().unwrap().serve_json = Some(self.route_json(funnel));
                }
                return Err(error);
            }
            self.state.lock().unwrap().serve_json = Some(self.route_json(funnel));
            Ok(())
        }
        fn tailscale_serve_remove_root(&self) -> Result<(), String> {
            let mut state = self.state.lock().unwrap();
            state.calls.push("serve_remove");
            state.serve_json = Some("{}".into());
            Ok(())
        }
        fn unit_state(&self) -> UnitState {
            self.state.lock().unwrap().unit
        }
        fn unit_file_exists(&self) -> bool {
            self.state.lock().unwrap().unit_file
        }
        fn unit_install_and_start(&self, unit_text: &str) -> Result<(), String> {
            let mut state = self.state.lock().unwrap();
            state.calls.push("unit_start");
            if let Some(error) = state.unit_start_error.clone() {
                return Err(error);
            }
            state.unit_file = true;
            state.unit = UnitState {
                installed: true,
                active: !state.unit_never_active,
            };
            state.installed_unit_text = Some(unit_text.to_string());
            Ok(())
        }
        fn unit_stop_and_disable(&self) -> Result<(), String> {
            let mut state = self.state.lock().unwrap();
            state.calls.push("unit_stop");
            state.unit = UnitState {
                installed: state.unit_file,
                active: false,
            };
            Ok(())
        }
        fn unit_remove_file(&self) -> Result<(), String> {
            let mut state = self.state.lock().unwrap();
            state.calls.push("unit_remove");
            state.unit_file = false;
            state.unit = UnitState::default();
            Ok(())
        }
        fn unit_started_at(&self) -> Option<SystemTime> {
            self.state.lock().unwrap().unit_started_at
        }
        fn binary_modified(&self, _binary: &Path) -> Option<SystemTime> {
            self.state.lock().unwrap().binary_modified
        }
        fn unit_restart(&self) -> Result<(), String> {
            let mut state = self.state.lock().unwrap();
            state.calls.push("unit_restart");
            state.unit.active = !state.unit_never_active;
            state.unit_started_at = state.binary_modified.map(|t| t + Duration::from_secs(1));
            Ok(())
        }
    }

    fn projects() -> Vec<(String, String)> {
        vec![
            (PROJECT_A.into(), "alpha".into()),
            (PROJECT_B.into(), "beta".into()),
        ]
    }

    fn enable_request() -> RemoteSetConfigRequestV1 {
        RemoteSetConfigRequestV1 {
            enabled: Some(true),
            allowed_node_ids: Some(vec![PHONE.into()]),
            project_ids: Some(vec![PROJECT_A.into()]),
        }
    }

    fn disable_request() -> RemoteSetConfigRequestV1 {
        RemoteSetConfigRequestV1 {
            enabled: Some(false),
            ..Default::default()
        }
    }

    /// The policy on disk is disabled and still holds the chosen selections.
    fn assert_rolled_back(fake: &Fake, out: &RemoteStatusV1) {
        assert!(!out.enabled && !out.gateway_running && !out.serve_route_present);
        let policy = config::read(&fake.policy_path()).unwrap();
        assert!(!policy.enabled);
        assert_eq!(policy.allowed_node_ids, vec![PHONE.to_string()]);
        assert_eq!(policy.project_ids, vec![PROJECT_A.to_string()]);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn enabling_detects_identity_starts_the_gateway_and_serves() {
        let fake = Fake::new();
        let out = set_config(&fake, &projects(), &enable_request()).unwrap();
        assert!(out.enabled && out.gateway_running && out.serve_route_present && out.funnel_off);
        assert_eq!(out.url.as_deref(), Some("https://box.example.ts.net/"));
        assert!(out.serve_pending_command.is_none());
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        let policy = config::read(&fake.policy_path()).unwrap();
        assert_eq!(policy.owner_user_id, OWNER);
        assert_eq!(policy.canonical_host, HOST);
        assert_eq!(policy.allowed_node_ids, vec![PHONE.to_string()]);
        assert_eq!(policy.project_ids, vec![PROJECT_A.to_string()]);
        let unit = fake
            .state
            .lock()
            .unwrap()
            .installed_unit_text
            .clone()
            .unwrap();
        for needle in [
            "Restart=always",
            "WantedBy=default.target",
            "RuntimeDirectoryMode=0700",
            "ExecStart=/opt/rsi/bin/rsi-remote run ",
            "%t/rsi-remote/ingress.sock",
            UNIT_MARKER,
        ] {
            assert!(unit.contains(needle), "{needle}");
        }
        let phone = out.peers.iter().find(|p| p.id == PHONE).unwrap();
        assert!(phone.allowed);
        let ids: Vec<_> = out.peers.iter().map(|p| p.id.as_str()).collect();
        assert!(ids.contains(&LAPTOP) && !ids.contains(&STRANGER));
        let alpha = out.projects.iter().find(|p| p.id == PROJECT_A).unwrap();
        assert!(alpha.exposed);
        assert_eq!(fake.calls(), ["unit_start", "serve_apply"]);
    }

    /// #1639: the unit must share the host user namespace so tailscaled is
    /// seen as uid 0, and must come back after a clean SIGTERM.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn gateway_unit_keeps_the_host_user_namespace_and_always_restarts() {
        let text = unit_text(Path::new("/opt/rsi/bin/rsi-remote"), Path::new("/p")).unwrap();
        let directives: Vec<&str> = text
            .lines()
            .filter_map(|line| line.split_once('=').map(|(key, _)| key))
            .collect();
        assert_eq!(
            directives,
            [
                "Description",
                "After",
                "Type",
                "ExecStartPre",
                "ExecStart",
                "RuntimeDirectory",
                "RuntimeDirectoryMode",
                "Restart",
                "RestartSec",
                "NoNewPrivileges",
                "WantedBy",
            ]
        );
        assert!(text.contains("\nRestart=always\n"));
    }

    /// #1639: a daemon start re-starts an enabled gateway that is down, and
    /// touches nothing when Remote is off.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn converge_starts_an_enabled_gateway_that_is_down() {
        let fake = Fake::new();
        assert_eq!(converge(&fake), Ok(Converged::Disabled));
        assert!(fake.calls().is_empty());

        set_config(&fake, &projects(), &enable_request()).unwrap();
        fake.state.lock().unwrap().calls.clear();
        // The gateway died (a kill by name, a reboot, a deploy).
        fake.state.lock().unwrap().unit.active = false;
        assert_eq!(converge(&fake), Ok(Converged::Started));
        assert!(fake.state.lock().unwrap().unit.active);
        assert_eq!(fake.calls(), ["unit_start"]);
        let unit = fake.state.lock().unwrap().installed_unit_text.clone();
        assert_eq!(
            unit,
            Some(unit_text(Path::new("/opt/rsi/bin/rsi-remote"), &fake.policy_path()).unwrap())
        );

        assert_eq!(converge(&fake), Ok(Converged::AlreadyActive));

        fake.state.lock().unwrap().unit.active = false;
        fake.state.lock().unwrap().unit_never_active = true;
        assert_eq!(
            converge(&fake),
            Err("gateway unit did not become active".into())
        );

        set_config(&fake, &projects(), &disable_request()).unwrap();
        fake.state.lock().unwrap().calls.clear();
        assert_eq!(converge(&fake), Ok(Converged::Disabled));
        assert!(fake.calls().is_empty());
    }

    /// Review #1645: a deploy that replaces the binary under an unchanged
    /// unit restarts the running gateway onto it, once; a disabled policy
    /// is never touched however stale the binary looks.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn converge_restarts_a_gateway_older_than_its_binary() {
        let at = |secs: u64| Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs));
        let fake = Fake::new();
        set_config(&fake, &projects(), &enable_request()).unwrap();
        {
            let mut state = fake.state.lock().unwrap();
            state.calls.clear();
            state.unit_started_at = at(1_000);
            state.binary_modified = at(2_000);
        }
        assert_eq!(converge(&fake), Ok(Converged::Restarted));
        assert_eq!(fake.calls(), ["unit_start", "unit_restart"]);
        // The restarted process is now newer than the binary: no second restart.
        assert_eq!(converge(&fake), Ok(Converged::AlreadyActive));
        assert_eq!(fake.calls(), ["unit_start", "unit_restart", "unit_start"]);

        set_config(&fake, &projects(), &disable_request()).unwrap();
        {
            let mut state = fake.state.lock().unwrap();
            state.calls.clear();
            state.binary_modified = at(9_000);
        }
        assert_eq!(converge(&fake), Ok(Converged::Disabled));
        assert!(fake.calls().is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn gateway_is_outdated_only_when_the_binary_is_newer_by_whole_seconds() {
        let at = |secs: u64, nanos: u32| Some(SystemTime::UNIX_EPOCH + Duration::new(secs, nanos));
        assert!(gateway_outdated(at(100, 0), at(101, 0)));
        assert!(!gateway_outdated(at(100, 0), at(100, 900_000_000)));
        assert!(!gateway_outdated(at(101, 0), at(100, 0)));
        assert!(!gateway_outdated(None, at(101, 0)));
        assert!(!gateway_outdated(at(100, 0), None));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn enabling_needs_a_device_and_a_project_and_known_projects() {
        let fake = Fake::new();
        let mut request = enable_request();
        request.allowed_node_ids = Some(Vec::new());
        assert!(
            set_config(&fake, &projects(), &request)
                .unwrap_err()
                .contains("device")
        );
        let mut request = enable_request();
        request.project_ids = Some(Vec::new());
        assert!(
            set_config(&fake, &projects(), &request)
                .unwrap_err()
                .contains("project")
        );
        let mut request = enable_request();
        request.project_ids = Some(vec!["550e8400-e29b-41d4-a716-446655449999".into()]);
        assert!(
            set_config(&fake, &projects(), &request)
                .unwrap_err()
                .contains("unknown project")
        );
        let mut request = enable_request();
        request.project_ids = Some(vec!["not-a-uuid".into()]);
        assert!(set_config(&fake, &projects(), &request).is_err());
        assert!(
            fake.calls().is_empty(),
            "nothing started on a rejected edit"
        );
        assert!(!fake.policy_path().exists());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn project_and_device_caps_are_enforced() {
        let fake = Fake::new();
        let many: Vec<String> = (0..33)
            .map(|n| format!("550e8400-e29b-41d4-a716-4466554400{n:02}"))
            .collect();
        let rows: Vec<(String, String)> = many.iter().map(|id| (id.clone(), "p".into())).collect();
        let mut request = enable_request();
        request.project_ids = Some(many);
        assert!(
            set_config(&fake, &rows, &request)
                .unwrap_err()
                .contains("at most 32")
        );
        let mut request = enable_request();
        request.allowed_node_ids = Some((0..65).map(|n| format!("n{n}")).collect());
        assert!(
            set_config(&fake, &projects(), &request)
                .unwrap_err()
                .contains("at most 64 devices")
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_largest_accepted_device_list_still_fits_the_gateways_reader() {
        let fake = Fake::new();
        set_config(&fake, &projects(), &enable_request()).unwrap();
        // The device cap times the id bound stays under the reader's size
        // limit, so an accepted edit can never brick later edits (the byte
        // bound itself is enforced and tested in rsi-remote's writer).
        let mut request = enable_request();
        request.allowed_node_ids = Some(
            (0..MAX_REMOTE_DEVICES)
                .map(|n| format!("{n:0>128}"))
                .collect(),
        );
        set_config(&fake, &projects(), &request).unwrap();
        let policy = config::read(&fake.policy_path()).unwrap();
        assert_eq!(policy.allowed_node_ids.len(), MAX_REMOTE_DEVICES);
        assert!(std::fs::metadata(fake.policy_path()).unwrap().len() < 16 * 1024);
        // A later edit and a disable both still work.
        set_config(&fake, &projects(), &disable_request()).unwrap();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn disabling_writes_policy_first_then_stops_gateway_and_removes_route() {
        let fake = Fake::new();
        set_config(&fake, &projects(), &enable_request()).unwrap();
        let out = set_config(&fake, &projects(), &disable_request()).unwrap();
        assert!(!out.enabled && !out.gateway_running && !out.serve_route_present);
        assert!(config::read(&fake.policy_path()).is_ok_and(|p| !p.enabled));
        let calls = fake.calls();
        assert_eq!(&calls[calls.len() - 2..], ["unit_stop", "serve_remove"]);
        // Selections survive a disable so re-enabling needs no re-picking.
        assert!(out.peers.iter().any(|p| p.id == PHONE && p.allowed));
        assert!(out.projects.iter().any(|p| p.id == PROJECT_A && p.exposed));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn disable_removes_our_route_even_with_funnel_on_and_extra_handlers_but_not_a_foreign_one() {
        let fake = Fake::new();
        set_config(&fake, &projects(), &enable_request()).unwrap();
        // Our `/` plus an unrelated mount, with Funnel on: readiness would
        // reject this, ownership must not.
        let socket = fake.socket_path();
        fake.set_serve(
            &serde_json::json!({
                "TCP": {"443": {"HTTPS": true}},
                "Web": {format!("{HOST}:443"): {"Handlers": {
                    "/": {"Proxy": format!("unix:{}", socket.display())},
                    "/other": {"Proxy": "http://127.0.0.1:9000"},
                }}},
                "AllowFunnel": {format!("{HOST}:443"): true},
            })
            .to_string(),
        );
        set_config(&fake, &projects(), &disable_request()).unwrap();
        assert!(fake.calls().contains(&"serve_remove"));

        let fake = Fake::new();
        set_config(&fake, &projects(), &enable_request()).unwrap();
        fake.set_serve(
            &serde_json::json!({
                "TCP": {"443": {"HTTPS": true}},
                "Web": {format!("{HOST}:443"): {"Handlers": {"/": {"Proxy": "http://127.0.0.1:3000"}}}},
            })
            .to_string(),
        );
        let before = fake.calls().len();
        set_config(&fake, &projects(), &disable_request()).unwrap();
        assert!(
            !fake.calls()[before..].contains(&"serve_remove"),
            "a route we do not own is left alone"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn disable_recovers_an_unreadable_policy_file() {
        let fake = Fake::new();
        std::fs::write(fake.policy_path(), "x".repeat(config::MAX_POLICY_BYTES + 1)).unwrap();
        // An edit still refuses to overwrite it blindly.
        assert!(set_config(&fake, &projects(), &enable_request()).is_err());
        let out = set_config(&fake, &projects(), &disable_request()).unwrap();
        assert!(!out.enabled);
        assert!(config::read(&fake.policy_path()).is_ok_and(|p| !p.enabled));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn selections_persist_while_disabled_without_starting_anything() {
        let fake = Fake::new();
        let out = set_config(
            &fake,
            &projects(),
            &RemoteSetConfigRequestV1 {
                enabled: None,
                allowed_node_ids: Some(vec![PHONE.into(), LAPTOP.into()]),
                project_ids: Some(vec![PROJECT_B.into()]),
            },
        )
        .unwrap();
        assert!(!out.enabled);
        assert!(fake.calls().is_empty());
        assert_eq!(out.peers.iter().filter(|p| p.allowed).count(), 2);
        assert!(out.projects.iter().any(|p| p.id == PROJECT_B && p.exposed));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn unit_failure_aborts_before_serve_and_rolls_back_to_a_disabled_policy() {
        let fake = Fake::new();
        fake.state.lock().unwrap().unit_start_error = Some("systemctl exploded".into());
        let out = set_config(&fake, &projects(), &enable_request()).unwrap();
        assert_rolled_back(&fake, &out);
        assert!(out.errors.iter().any(|e| e.contains("systemctl exploded")));
        let calls = fake.calls();
        assert!(!calls.contains(&"serve_apply"), "{calls:?}");
        assert!(calls.contains(&"unit_stop"));
        // The unit file did not exist before, so nothing of ours is left.
        assert!(!fake.state.lock().unwrap().unit_file);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_unit_that_never_becomes_active_rolls_back_without_serving() {
        let fake = Fake::new();
        fake.state.lock().unwrap().unit_never_active = true;
        let out = set_config(&fake, &projects(), &enable_request()).unwrap();
        assert_rolled_back(&fake, &out);
        assert!(
            out.errors
                .iter()
                .any(|e| e.contains("did not become active"))
        );
        assert!(!fake.calls().contains(&"serve_apply"));
        assert!(fake.calls().contains(&"unit_remove"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn serve_privilege_failure_rolls_back_and_names_the_one_time_command() {
        let fake = Fake::new();
        fake.state.lock().unwrap().serve_apply_error = Some(format!(
            "Access denied. One-time setup, then enable again: {OPERATOR_COMMAND}"
        ));
        let out = set_config(&fake, &projects(), &enable_request()).unwrap();
        assert_rolled_back(&fake, &out);
        assert_eq!(out.serve_pending_command.as_deref(), Some(OPERATOR_COMMAND));
        assert!(out.errors.iter().any(|e| e.contains("Access denied")));
        assert!(!fake.state.lock().unwrap().unit_file);
        // After the one-time step, enabling again converges.
        fake.state.lock().unwrap().serve_apply_error = None;
        let out = set_config(&fake, &projects(), &enable_request()).unwrap();
        assert!(out.enabled && out.gateway_running && out.serve_route_present);
        assert!(out.serve_pending_command.is_none());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_serve_command_that_changed_the_host_before_failing_is_undone() {
        let fake = Fake::new();
        {
            let mut state = fake.state.lock().unwrap();
            state.serve_apply_error = Some("timed out".into());
            state.serve_apply_partial = true;
        }
        let out = set_config(&fake, &projects(), &enable_request()).unwrap();
        assert_rolled_back(&fake, &out);
        let calls = fake.calls();
        let apply = calls.iter().position(|c| *c == "serve_apply").unwrap();
        let remove = calls.iter().position(|c| *c == "serve_remove").unwrap();
        let stop = calls.iter().position(|c| *c == "unit_stop").unwrap();
        assert!(apply < remove && remove < stop, "reverse order: {calls:?}");
        assert!(!out.serve_route_present);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn enable_refuses_to_replace_a_foreign_root_route_or_an_unreadable_config() {
        let fake = Fake::new();
        let foreign = serde_json::json!({
            "TCP": {"443": {"HTTPS": true}},
            "Web": {format!("{HOST}:443"): {"Handlers": {"/": {"Proxy": "http://127.0.0.1:3000"}}}},
        })
        .to_string();
        fake.set_serve(&foreign);
        let error = set_config(&fake, &projects(), &enable_request()).unwrap_err();
        assert!(
            error.contains("already served by another service"),
            "{error}"
        );
        assert!(fake.calls().is_empty());
        assert!(!fake.policy_path().exists(), "nothing persisted");
        assert_eq!(
            fake.state.lock().unwrap().serve_json.as_deref(),
            Some(foreign.as_str())
        );

        // A non-HTTPS listener and extra handlers on 443 are also refused.
        fake.set_serve(r#"{"TCP":{"443":{"TCPForward":"127.0.0.1:22"}}}"#);
        assert!(set_config(&fake, &projects(), &enable_request()).is_err());
        let socket = fake.socket_path();
        fake.set_serve(
            &serde_json::json!({
                "TCP": {"443": {"HTTPS": true}},
                "Web": {format!("{HOST}:443"): {"Handlers": {
                    "/": {"Proxy": format!("unix:{}", socket.display())},
                    "/x": {"Proxy": "http://127.0.0.1:1"},
                }}},
            })
            .to_string(),
        );
        assert!(
            set_config(&fake, &projects(), &enable_request())
                .unwrap_err()
                .contains("other handlers")
        );

        // Unreadable serve config: cannot prove ownership, so refuse.
        fake.state.lock().unwrap().serve_json = None;
        let error = set_config(&fake, &projects(), &enable_request()).unwrap_err();
        assert!(
            error.contains("cannot read the Tailscale serve config"),
            "{error}"
        );
        assert!(fake.calls().is_empty());
        assert!(!fake.policy_path().exists());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn enabling_over_our_own_existing_route_does_not_reapply_it() {
        let fake = Fake::new();
        fake.set_serve(&fake.route_json(false));
        let out = set_config(&fake, &projects(), &enable_request()).unwrap();
        assert!(out.enabled && out.serve_route_present);
        assert!(!fake.calls().contains(&"serve_apply"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn funnel_on_refuses_enable_and_is_reported_in_status() {
        let fake = Fake::new();
        fake.set_serve(&fake.route_json(true));
        let error = set_config(&fake, &projects(), &enable_request()).unwrap_err();
        assert!(error.contains("Funnel"), "{error}");
        assert!(fake.calls().is_empty());
        let out = status(&fake, &projects());
        assert!(!out.funnel_off);
        assert!(out.errors.iter().any(|e| e.contains("Funnel")));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn status_reports_tailscale_version_qualification() {
        let fake = Fake::new();
        let out = status(&fake, &projects());
        assert!(out.tailscale.reachable && out.tailscale.version_qualified);
        assert_eq!(out.tailscale.version, "1.102.4");
        fake.state.lock().unwrap().version = "1.90.0".into();
        let out = status(&fake, &projects());
        assert!(!out.tailscale.version_qualified);
        assert!(out.errors.iter().any(|e| e.contains("qualified")));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn unit_text_escapes_percent_and_dollar_and_refuses_unsafe_paths() {
        assert!(unit_text(Path::new("/opt/a b/rsi-remote"), Path::new("/p")).is_err());
        assert!(unit_text(Path::new("relative/rsi-remote"), Path::new("/p")).is_err());
        assert!(unit_text(Path::new("/opt/it's/rsi-remote"), Path::new("/p")).is_err());
        let text = unit_text(
            Path::new("/opt/100%/rsi-remote"),
            Path::new("/home/${USER}/$HOME/policy.toml"),
        )
        .unwrap();
        assert!(text.contains("/opt/100%%/rsi-remote"));
        assert!(text.contains("/home/$${USER}/$$HOME/policy.toml"));
        use std::os::unix::ffi::OsStrExt;
        let bad = Path::new(std::ffi::OsStr::from_bytes(b"/opt/\xff/rsi-remote"));
        assert!(unit_text(bad, Path::new("/p")).is_err());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn unit_file_install_is_atomic_0644_and_refuses_symlinks_and_foreign_units() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        let units = dir.path().join("systemd/user");
        let text = format!("{UNIT_MARKER}\n[Service]\nExecStart=/bin/true\n");
        assert!(install_unit_file(&units, &text).unwrap());
        let path = units.join(UNIT_NAME);
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o644);
        // Same content and mode: no rewrite.
        assert!(!install_unit_file(&units, &text).unwrap());
        // A group-writable managed unit is replaced with safe permissions.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o664)).unwrap();
        assert!(install_unit_file(&units, &text).unwrap());
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o644);
        // No temp files linger.
        assert_eq!(std::fs::read_dir(&units).unwrap().count(), 1);

        // A symlink at the unit path is never followed or replaced.
        std::fs::remove_file(&path).unwrap();
        let target = dir.path().join("operator-file");
        std::fs::write(&target, "precious").unwrap();
        symlink(&target, &path).unwrap();
        assert!(install_unit_file(&units, &text).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "precious");
        assert!(remove_unit_file(&units).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "precious");

        // A foreign regular unit (no marker) is left alone.
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, "[Service]\nExecStart=/bin/false\n").unwrap();
        assert!(install_unit_file(&units, &text).is_err());
        assert!(remove_unit_file(&units).is_err());
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("/bin/false")
        );

        // A group-writable unit directory is refused.
        let open = dir.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o775)).unwrap();
        assert!(install_unit_file(&open, &text).is_err());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn run_drains_large_output_and_reports_failures_without_deadlock() {
        // 3 MB on stdout is more than a pipe buffer: waiting before reading
        // would hang until the timeout.
        let out = run(
            "sh",
            &["-c", "head -c 3000000 /dev/zero"],
            Duration::from_secs(20),
        )
        .unwrap();
        assert_eq!(out.len(), 3_000_000);
        let started = Instant::now();
        let error = run(
            "sh",
            &["-c", "head -c 6000000 /dev/zero"],
            Duration::from_secs(20),
        )
        .unwrap_err();
        assert!(error.contains("exceeded"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(15));
        // Heavy stderr plus a failing exit still returns the first line.
        let error = run(
            "sh",
            &["-c", "echo boom >&2; head -c 2000000 /dev/zero >&2; exit 3"],
            Duration::from_secs(20),
        )
        .unwrap_err();
        assert!(error.contains("boom"), "{error}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn run_kills_a_command_that_outlives_its_timeout() {
        let started = Instant::now();
        let error = run("sh", &["-c", "sleep 30"], Duration::from_millis(300)).unwrap_err();
        assert!(error.contains("timed out"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));
        // A flood that never ends is cut off by the same deadline.
        let error = run("sh", &["-c", "yes"], Duration::from_millis(500)).unwrap_err();
        assert!(
            error.contains("timed out") || error.contains("exceeded"),
            "{error}"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_disable_cannot_be_undone_by_an_earlier_apply() {
        let fake = Arc::new(Fake::new());
        let controller = Arc::new(RemoteController::new(fake.clone()));
        controller
            .set_config(projects(), enable_request())
            .await
            .unwrap();
        // The apply reads the (enabled) policy, then parks inside Tailscale.
        fake.state.lock().unwrap().park_status = true;
        let apply = {
            let controller = Arc::clone(&controller);
            tokio::spawn(async move {
                controller
                    .set_config(projects(), RemoteSetConfigRequestV1::default())
                    .await
            })
        };
        // Wait until the apply is parked, then race a disable against it.
        for _ in 0..200 {
            if !fake.state.lock().unwrap().park_status {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let disable = {
            let controller = Arc::clone(&controller);
            tokio::spawn(async move { controller.set_config(projects(), disable_request()).await })
        };
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !disable.is_finished(),
            "the disable waits for the in-flight operation"
        );
        assert!(
            config::read(&fake.policy_path()).is_ok_and(|p| p.enabled),
            "the disable has not touched the policy yet"
        );
        fake.release();
        apply.await.unwrap().unwrap();
        let out = disable.await.unwrap().unwrap();
        assert!(!out.enabled && !out.gateway_running && !out.serve_route_present);
        assert!(config::read(&fake.policy_path()).is_ok_and(|p| !p.enabled));
    }

    /// Read-only smoke against this machine's real Tailscale and systemd.
    /// Run by hand: `cargo test -p rsid --lib real_host_status_smoke -- --ignored --nocapture`.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    #[ignore = "reads the real host's Tailscale and systemd state"]
    fn real_host_status_smoke() {
        let out = status(&HostSystem, &[]);
        println!("{out:#?}");
        assert!(!out.tailscale.pinned_version.is_empty());
    }
}
