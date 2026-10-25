//! Start the installed rsid from the daemon-down card (local #232).
//!
//! The desktop never builds, bundles, downloads or runs rsid itself. It only
//! asks the OS service manager to start the service the installer already put
//! in place (`scripts/install-release.sh`):
//!
//! * macOS: the launchd user agent `dev.rsi.rsid`
//!   (`~/Library/LaunchAgents/dev.rsi.rsid.plist`, `scripts/rsid-launchd.sh`):
//!   `launchctl kickstart gui/<uid>/dev.rsi.rsid` when the agent is loaded,
//!   else `launchctl bootstrap gui/<uid> <that plist>`, the same two steps as
//!   the installer's `rsid_launchd_start`;
//! * Linux: the systemd user unit `rsid.service` (`scripts/systemd/`):
//!   `systemctl --user start rsid.service`;
//! * anything else: an explicit "not supported" error.
//!
//! A start is only offered when it can help (local #258). Before it runs, the
//! service manager is asked, read-only, whether the service is already running
//! (`launchctl print`, `systemctl --user show`) and which socket it holds:
//!
//! * running: a start would be a no-op (`kickstart` without `-k`, `systemctl
//!   start` on an active unit), so Start is refused and the card offers an
//!   explicit, confirmed Restart (`launchctl kickstart -k`, `systemctl --user
//!   restart`) instead;
//! * this window is looking at a socket other than the service's: neither a
//!   start nor a restart could make this window connect, so both are refused.
//!
//! Every program is a fixed absolute path and every argument is a constant or
//! derived from the process's own uid and home directory: nothing the webview
//! sends reaches an argv, and no shell is involved.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

pub const LAUNCHD_LABEL: &str = "dev.rsi.rsid";
pub const SYSTEMD_UNIT: &str = "rsid.service";
const LAUNCHCTL: &str = "/bin/launchctl";
/// Where `systemctl` lives on the distributions we know; the first that exists is used.
const SYSTEMCTL: &[&str] = &[
    "/usr/bin/systemctl",
    "/bin/systemctl",
    "/run/current-system/sw/bin/systemctl",
];
/// Long enough for `systemctl start` to wait out a slow start; the card keeps polling after it.
const RUN_LIMIT: Duration = Duration::from_secs(60);
const MAX_STDERR: usize = 8 * 1024;
const MAX_STDOUT: usize = 64 * 1024;
/// `launchctl print` and `systemctl show` answer at once.
const PROBE_LIMIT: Duration = Duration::from_secs(5);

/// What this host has installed, as far as starting rsid goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Service {
    Launchd {
        uid: u32,
        plist: PathBuf,
    },
    Systemd {
        systemctl: PathBuf,
        unit_file: PathBuf,
    },
    /// The OS is supported but no installed rsid service was found.
    NotInstalled {
        manager: &'static str,
        looked: Vec<PathBuf>,
    },
    Unsupported {
        reason: String,
    },
}

/// The facts detection reads, gathered once so the selection logic is pure.
#[derive(Debug, Clone, Default)]
pub struct HostFacts {
    pub home: Option<PathBuf>,
    pub xdg_config_home: Option<PathBuf>,
    pub uid: Option<u32>,
}

pub fn launchd_plist(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist"))
}

/// The user-unit directories systemd searches, most specific first.
pub fn systemd_unit_dirs(facts: &HostFacts) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    match (&facts.xdg_config_home, &facts.home) {
        (Some(x), _) if x.is_absolute() => dirs.push(x.join("systemd/user")),
        (_, Some(h)) => dirs.push(h.join(".config/systemd/user")),
        _ => {}
    }
    if let Some(h) = &facts.home {
        dirs.push(h.join(".local/share/systemd/user"));
    }
    for d in [
        "/etc/systemd/user",
        "/usr/local/lib/systemd/user",
        "/usr/lib/systemd/user",
    ] {
        dirs.push(PathBuf::from(d));
    }
    dirs
}

/// The macOS selection: the installed agent plist, or what was looked for.
pub fn select_launchd(facts: &HostFacts, exists: &dyn Fn(&Path) -> bool) -> Service {
    let Some(home) = &facts.home else {
        return Service::NotInstalled {
            manager: "launchd",
            looked: Vec::new(),
        };
    };
    let plist = launchd_plist(home);
    match facts.uid {
        Some(uid) if exists(&plist) => Service::Launchd { uid, plist },
        _ => Service::NotInstalled {
            manager: "launchd",
            looked: vec![plist],
        },
    }
}

/// The Linux selection: the first installed `rsid.service` and a `systemctl` to start it.
pub fn select_systemd(facts: &HostFacts, exists: &dyn Fn(&Path) -> bool) -> Service {
    let looked: Vec<PathBuf> = systemd_unit_dirs(facts)
        .into_iter()
        .map(|d| d.join(SYSTEMD_UNIT))
        .collect();
    let unit_file = looked.iter().find(|p| exists(p)).cloned();
    let systemctl = SYSTEMCTL
        .iter()
        .map(PathBuf::from)
        .find(|p| exists(p.as_path()));
    match (unit_file, systemctl) {
        (Some(unit_file), Some(systemctl)) => Service::Systemd {
            systemctl,
            unit_file,
        },
        (Some(_), None) => Service::NotInstalled {
            manager: "systemd",
            looked: SYSTEMCTL.iter().map(PathBuf::from).collect(),
        },
        (None, _) => Service::NotInstalled {
            manager: "systemd",
            looked,
        },
    }
}

/// One step of a start: a fixed program and its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub program: PathBuf,
    pub args: Vec<String>,
}

impl Step {
    fn new(program: impl Into<PathBuf>, args: &[&str]) -> Self {
        Step {
            program: program.into(),
            args: args.iter().map(|a| (*a).to_string()).collect(),
        }
    }

    /// The command as the card shows it under Details (program name, not path).
    pub fn display(&self) -> String {
        let name = self
            .program
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        std::iter::once(name)
            .chain(self.args.iter().cloned())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// The fixed steps for one service: a read-only `probe`, then `start` (or the
/// launchd-only `fallback` when the agent is not loaded) or `restart`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub probe: Step,
    pub start: Step,
    pub fallback: Option<Step>,
    pub restart: Step,
}

/// The fixed argv for a detected service; `None` when there is nothing to start.
pub fn plan(service: &Service) -> Option<Plan> {
    match service {
        Service::Launchd { uid, plist } => {
            let domain = format!("gui/{uid}");
            let target = format!("{domain}/{LAUNCHD_LABEL}");
            let plist = plist.to_string_lossy();
            Some(Plan {
                probe: Step::new(LAUNCHCTL, &["print", &target]),
                start: Step::new(LAUNCHCTL, &["kickstart", &target]),
                fallback: Some(Step::new(LAUNCHCTL, &["bootstrap", &domain, &plist])),
                restart: Step::new(LAUNCHCTL, &["kickstart", "-k", &target]),
            })
        }
        Service::Systemd { systemctl, .. } => Some(Plan {
            probe: Step::new(
                systemctl.clone(),
                &[
                    "--user",
                    "show",
                    SYSTEMD_UNIT,
                    "--property=ActiveState",
                    "--property=MainPID",
                ],
            ),
            start: Step::new(systemctl.clone(), &["--user", "start", SYSTEMD_UNIT]),
            fallback: None,
            restart: Step::new(systemctl.clone(), &["--user", "restart", SYSTEMD_UNIT]),
        }),
        Service::NotInstalled { .. } | Service::Unsupported { .. } => None,
    }
}

/// What the webview learns about the service (no argv, no secrets).
pub fn describe(service: &Service) -> Value {
    match service {
        Service::Launchd { plist, .. } => json!({
            "kind": "installed", "manager": "launchd", "name": LAUNCHD_LABEL, "path": plist,
        }),
        Service::Systemd { unit_file, .. } => json!({
            "kind": "installed", "manager": "systemd", "name": SYSTEMD_UNIT, "path": unit_file,
        }),
        Service::NotInstalled { manager, looked } => json!({
            "kind": "not-installed", "manager": manager, "looked": looked,
        }),
        Service::Unsupported { reason } => json!({ "kind": "unsupported", "reason": reason }),
    }
}

/// How one step ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ran {
    pub command: String,
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Ran {
    fn ok(&self) -> bool {
        self.status == Some(0)
    }
}

/// Whether the service manager says the service is running right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    Running {
        pid: Option<u32>,
    },
    NotRunning,
    /// The probe could not run or its output was not understood.
    Unknown,
}

/// What the read-only probe learned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inspection {
    /// launchd: the agent is loaded (`launchctl print` succeeded); systemd: always true.
    pub loaded: bool,
    pub state: RunState,
    /// The socket the service holds, when its manager told us; else the default applies.
    pub service_socket: Option<PathBuf>,
}

/// The value after `key = ` on the first line of `text` that starts with it.
fn launchd_field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let prefix = format!("{key} = ");
    text.lines()
        .find_map(|l| l.trim().strip_prefix(prefix.as_str()))
        .map(str::trim)
}

/// The first `*.sock` argument of the service's program, from `launchctl print`'s
/// `arguments = { ... }` block (the installer runs `rsi-socket-hold <socket> -- ...`).
fn launchd_socket(text: &str) -> Option<PathBuf> {
    let mut lines = text.lines().map(str::trim);
    lines.find(|l| *l == "arguments = {")?;
    lines
        .take_while(|l| *l != "}")
        .find(|l| l.ends_with(".sock"))
        .map(PathBuf::from)
}

/// Read the probe's output for `service`. Pure: the tests feed it recorded output.
pub fn parse_probe(service: &Service, ran: &Ran) -> Inspection {
    match service {
        Service::Launchd { .. } => {
            if !ran.ok() {
                // A non-zero status is launchd saying "no such service" (not loaded);
                // no status at all means the probe itself could not run.
                return Inspection {
                    loaded: false,
                    state: if ran.status.is_some() {
                        RunState::NotRunning
                    } else {
                        RunState::Unknown
                    },
                    service_socket: None,
                };
            }
            let state = match launchd_field(&ran.stdout, "state") {
                Some("running") => RunState::Running {
                    pid: launchd_field(&ran.stdout, "pid").and_then(|p| p.parse().ok()),
                },
                Some(_) => RunState::NotRunning,
                None => RunState::Unknown,
            };
            Inspection {
                loaded: true,
                state,
                service_socket: launchd_socket(&ran.stdout),
            }
        }
        _ => {
            let field = |k: &str| {
                let prefix = format!("{k}=");
                ran.stdout
                    .lines()
                    .find_map(|l| l.trim().strip_prefix(prefix.as_str()))
                    .map(str::trim)
            };
            let state = if !ran.ok() {
                RunState::Unknown
            } else {
                match field("ActiveState") {
                    Some("active" | "activating" | "reloading") => RunState::Running {
                        pid: field("MainPID")
                            .and_then(|p| p.parse::<u32>().ok())
                            .filter(|p| *p > 0),
                    },
                    Some(_) => RunState::NotRunning,
                    None => RunState::Unknown,
                }
            };
            Inspection {
                loaded: true,
                state,
                service_socket: None,
            }
        }
    }
}

/// What the webview learns about an installed service: the base fields, whether
/// it is running (and its pid), and the two sockets (the window's and the service's).
pub fn describe_installed(
    service: &Service,
    ins: &Inspection,
    app_socket: &Path,
    default_service_socket: Option<&Path>,
) -> Value {
    let mut v = describe(service);
    let service_socket = ins
        .service_socket
        .as_deref()
        .or(default_service_socket)
        .map(Path::to_path_buf);
    let (state, pid) = match ins.state {
        RunState::Running { pid } => ("running", pid),
        RunState::NotRunning => ("stopped", None),
        RunState::Unknown => ("unknown", None),
    };
    v["state"] = json!(state);
    v["pid"] = json!(pid);
    v["socket"] = json!({
        "app": app_socket,
        "service": service_socket,
        "matches": service_socket.as_deref().is_none_or(|s| s == app_socket),
    });
    v
}

/// Start the service, or restart it on an explicit request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Start,
    Restart,
}

/// Why nothing was run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// Start only: the service is already running, so a start would do nothing.
    Running { pid: Option<u32> },
    /// This window looks at a socket the service does not hold.
    WrongSocket { app: PathBuf, service: PathBuf },
}

impl Refusal {
    fn code(&self) -> &'static str {
        match self {
            Refusal::Running { .. } => "running",
            Refusal::WrongSocket { .. } => "wrong-socket",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attempt {
    Ran(Ran),
    Refused(Refusal),
}

/// Probe, decide, and run at most one more step. `run` is the real runner or a fake.
pub fn attempt(
    plan: &Plan,
    service: &Service,
    mode: Mode,
    app_socket: &Path,
    default_service_socket: Option<&Path>,
    run: &mut dyn FnMut(&Step) -> Ran,
) -> Attempt {
    let ins = parse_probe(service, &run(&plan.probe));
    let held = ins.service_socket.as_deref().or(default_service_socket);
    if let Some(held) = held.filter(|h| *h != app_socket) {
        return Attempt::Refused(Refusal::WrongSocket {
            app: app_socket.to_path_buf(),
            service: held.to_path_buf(),
        });
    }
    let step = match (mode, ins.state) {
        (Mode::Start, RunState::Running { pid }) => {
            return Attempt::Refused(Refusal::Running { pid });
        }
        (Mode::Restart, RunState::Running { .. }) => &plan.restart,
        _ => match &plan.fallback {
            Some(fallback) if !ins.loaded => fallback,
            _ => &plan.start,
        },
    };
    Attempt::Ran(run(step))
}

/// The start result for the webview: `ok` is the service manager's own verdict.
pub fn outcome(manager: &str, attempt: &Attempt) -> Value {
    match attempt {
        Attempt::Ran(ran) => json!({
            "ok": ran.ok(), "manager": manager, "command": ran.command,
            "status": ran.status, "stderr": ran.stderr, "refused": null,
        }),
        Attempt::Refused(why) => json!({
            "ok": false, "manager": manager, "command": "",
            "status": null, "stderr": "", "refused": why.code(),
        }),
    }
}

fn manager_of(service: &Service) -> &'static str {
    match service {
        Service::Launchd { .. } => "launchd",
        Service::Systemd { .. } => "systemd",
        Service::NotInstalled { manager, .. } => manager,
        Service::Unsupported { .. } => "none",
    }
}

/// Why there is nothing to start, in words the card can show.
pub fn nothing_to_start(service: &Service) -> String {
    match service {
        Service::NotInstalled { manager, looked } => {
            let looked: Vec<String> = looked.iter().map(|p| p.display().to_string()).collect();
            format!(
                "no installed rsid {manager} service was found (looked for {})",
                looked.join(", ")
            )
        }
        Service::Unsupported { reason } => reason.clone(),
        _ => String::new(),
    }
}

fn host_facts() -> HostFacts {
    let var = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    HostFacts {
        home: var("HOME"),
        xdg_config_home: var("XDG_CONFIG_HOME"),
        uid: current_uid(),
    }
}

#[cfg(unix)]
fn current_uid() -> Option<u32> {
    // SAFETY: getuid has no preconditions and cannot fail.
    Some(unsafe { libc::getuid() })
}

#[cfg(not(unix))]
fn current_uid() -> Option<u32> {
    None
}

fn path_exists(p: &Path) -> bool {
    p.is_file()
}

#[cfg(target_os = "macos")]
fn detect() -> Service {
    select_launchd(&host_facts(), &path_exists)
}

#[cfg(target_os = "linux")]
fn detect() -> Service {
    select_systemd(&host_facts(), &path_exists)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn detect() -> Service {
    let _ = (host_facts, path_exists);
    Service::Unsupported {
        reason: "starting rsid from this window is not supported on this system".to_string(),
    }
}

/// Read at most `cap` bytes from a pipe and drain the rest so the child never blocks.
fn capture<R: Read + Send + 'static>(
    pipe: Option<R>,
    cap: usize,
) -> Option<std::thread::JoinHandle<String>> {
    pipe.map(|mut p| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = (&mut p).take(cap as u64).read_to_end(&mut buf);
            let _ = std::io::copy(&mut p, &mut std::io::sink());
            String::from_utf8_lossy(&buf).trim().to_string()
        })
    })
}

/// Spawn one fixed step with no shell, bounded in time and in captured output.
fn run_step(step: &Step, limit: Duration) -> Ran {
    let command = step.display();
    let spawned = Command::new(&step.program)
        .args(&step.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            return Ran {
                command,
                status: None,
                stdout: String::new(),
                stderr: format!("cannot run {}: {e}", step.program.display()),
            };
        }
    };
    let out_reader = capture(child.stdout.take(), MAX_STDOUT);
    let err_reader = capture(child.stderr.take(), MAX_STDERR);
    let deadline = Instant::now() + limit;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Ok(s),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(format!("did not finish within {} s", limit.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => break Err(e.to_string()),
        }
    };
    let stdout = out_reader.and_then(|h| h.join().ok()).unwrap_or_default();
    let mut stderr = err_reader.and_then(|h| h.join().ok()).unwrap_or_default();
    match status {
        Ok(s) => Ran {
            command,
            status: s.code(),
            stdout,
            stderr,
        },
        Err(why) => {
            if !stderr.is_empty() {
                stderr.push('\n');
            }
            stderr.push_str(&why);
            Ran {
                command,
                status: None,
                stdout,
                stderr,
            }
        }
    }
}

/// The probe is read-only and quick; a hung one must not hold the card up.
fn run_real(step: &Step) -> Ran {
    let limit = if step.args.iter().any(|a| a == "print" || a == "show") {
        PROBE_LIMIT
    } else {
        RUN_LIMIT
    };
    run_step(step, limit)
}

/// The socket the installer's service holds unless its manager says otherwise.
fn default_service_socket() -> Option<PathBuf> {
    host_facts().home.map(|h| crate::daemon::home_socket(&h))
}

/// What this host has installed and whether it is running, for the card.
#[tauri::command]
pub async fn daemon_service() -> Value {
    tokio::task::spawn_blocking(|| {
        let service = detect();
        let Some(plan) = plan(&service) else {
            return describe(&service);
        };
        let ins = parse_probe(&service, &run_real(&plan.probe));
        describe_installed(
            &service,
            &ins,
            &crate::daemon::default_socket(),
            default_service_socket().as_deref(),
        )
    })
    .await
    .unwrap_or_else(|e| json!({ "kind": "unsupported", "reason": e.to_string() }))
}

fn run_mode(mode: Mode) -> Result<Value, String> {
    let service = detect();
    let plan = plan(&service).ok_or_else(|| nothing_to_start(&service))?;
    let did = attempt(
        &plan,
        &service,
        mode,
        &crate::daemon::default_socket(),
        default_service_socket().as_deref(),
        &mut run_real,
    );
    eprintln!(
        "rsi-desktop: {mode:?} manager={} result={did:?}",
        manager_of(&service)
    );
    Ok(outcome(manager_of(&service), &did))
}

/// Ask the OS service manager to start the installed rsid. Takes no arguments:
/// the argv is fixed by what is installed. `ok: false` carries the exit status
/// and stderr, or `refused` when starting could not help (already running, or
/// this window looks at another socket); an `Err` means there was nothing to start.
#[tauri::command]
pub async fn start_daemon() -> Result<Value, String> {
    tokio::task::spawn_blocking(|| run_mode(Mode::Start))
        .await
        .map_err(|e| e.to_string())?
}

/// Restart the installed rsid when it is running but not answering. The card
/// asks the operator first; this takes no arguments either.
#[tauri::command]
pub async fn restart_daemon() -> Result<Value, String> {
    tokio::task::spawn_blocking(|| run_mode(Mode::Restart))
        .await
        .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn facts(home: &str) -> HostFacts {
        HostFacts {
            home: Some(PathBuf::from(home)),
            xdg_config_home: None,
            uid: Some(501),
        }
    }

    fn only(paths: &[&str]) -> impl Fn(&Path) -> bool {
        let set: HashSet<PathBuf> = paths.iter().map(PathBuf::from).collect();
        move |p: &Path| set.contains(p)
    }

    fn argv(step: &Step) -> Vec<String> {
        std::iter::once(step.program.display().to_string())
            .chain(step.args.iter().cloned())
            .collect()
    }

    #[test]
    fn macos_kickstarts_the_installed_agent_and_bootstraps_it_when_unloaded() {
        let plist = "/Users/op/Library/LaunchAgents/dev.rsi.rsid.plist";
        let service = select_launchd(&facts("/Users/op"), &only(&[plist]));
        assert_eq!(
            service,
            Service::Launchd {
                uid: 501,
                plist: PathBuf::from(plist)
            }
        );
        let p = plan(&service).unwrap();
        assert_eq!(
            argv(&p.probe),
            ["/bin/launchctl", "print", "gui/501/dev.rsi.rsid"]
        );
        assert_eq!(
            argv(&p.restart),
            ["/bin/launchctl", "kickstart", "-k", "gui/501/dev.rsi.rsid"]
        );
        assert_eq!(
            argv(&p.start),
            ["/bin/launchctl", "kickstart", "gui/501/dev.rsi.rsid"]
        );
        assert_eq!(
            argv(p.fallback.as_ref().unwrap()),
            ["/bin/launchctl", "bootstrap", "gui/501", plist]
        );
        assert_eq!(
            p.start.display(),
            "launchctl kickstart gui/501/dev.rsi.rsid"
        );
    }

    #[test]
    fn macos_without_the_agent_plist_has_nothing_to_start() {
        let service = select_launchd(&facts("/Users/op"), &only(&[]));
        assert_eq!(plan(&service), None);
        assert_eq!(describe(&service)["kind"], "not-installed");
        assert!(
            nothing_to_start(&service)
                .contains("/Users/op/Library/LaunchAgents/dev.rsi.rsid.plist")
        );
        let no_home = select_launchd(&HostFacts::default(), &only(&[]));
        assert_eq!(plan(&no_home), None);
    }

    #[test]
    fn linux_starts_the_installed_user_unit_with_a_fixed_argv() {
        let unit = "/home/op/.config/systemd/user/rsid.service";
        let service = select_systemd(&facts("/home/op"), &only(&[unit, "/usr/bin/systemctl"]));
        let p = plan(&service).unwrap();
        assert_eq!(p.fallback, None);
        assert_eq!(
            argv(&p.start),
            ["/usr/bin/systemctl", "--user", "start", "rsid.service"]
        );
        assert_eq!(
            argv(&p.restart),
            ["/usr/bin/systemctl", "--user", "restart", "rsid.service"]
        );
        assert_eq!(
            argv(&p.probe),
            [
                "/usr/bin/systemctl",
                "--user",
                "show",
                "rsid.service",
                "--property=ActiveState",
                "--property=MainPID"
            ]
        );
        let d = describe(&service);
        assert_eq!(d["manager"], "systemd");
        assert_eq!(d["name"], "rsid.service");
        assert_eq!(d["path"], unit);
    }

    #[test]
    fn linux_finds_the_unit_in_xdg_config_or_system_dirs_and_needs_systemctl() {
        let mut f = facts("/home/op");
        f.xdg_config_home = Some(PathBuf::from("/cfg"));
        let xdg = select_systemd(
            &f,
            &only(&["/cfg/systemd/user/rsid.service", "/bin/systemctl"]),
        );
        assert!(matches!(&xdg, Service::Systemd { unit_file, systemctl }
            if unit_file == Path::new("/cfg/systemd/user/rsid.service") && systemctl == Path::new("/bin/systemctl")));
        // A relative XDG_CONFIG_HOME is ignored, as systemd does.
        f.xdg_config_home = Some(PathBuf::from("cfg"));
        assert_eq!(
            systemd_unit_dirs(&f)[0],
            PathBuf::from("/home/op/.config/systemd/user")
        );
        let system = select_systemd(
            &facts("/home/op"),
            &only(&["/usr/lib/systemd/user/rsid.service", "/usr/bin/systemctl"]),
        );
        assert!(matches!(system, Service::Systemd { .. }));
        let no_unit = select_systemd(&facts("/home/op"), &only(&["/usr/bin/systemctl"]));
        assert_eq!(plan(&no_unit), None);
        assert!(nothing_to_start(&no_unit).contains("/home/op/.config/systemd/user/rsid.service"));
        let no_systemctl = select_systemd(
            &facts("/home/op"),
            &only(&["/home/op/.config/systemd/user/rsid.service"]),
        );
        assert_eq!(plan(&no_systemctl), None);
    }

    #[test]
    fn unsupported_has_no_plan_and_says_why() {
        let s = Service::Unsupported {
            reason: "not supported on this system".into(),
        };
        assert_eq!(plan(&s), None);
        assert_eq!(describe(&s)["kind"], "unsupported");
        assert_eq!(nothing_to_start(&s), "not supported on this system");
    }

    /// (stdout, stderr, exit status) per step, in order; the second value records each display().
    type Canned = Vec<(&'static str, &'static str, Option<i32>)>;

    fn fake(
        results: Canned,
    ) -> (
        impl FnMut(&Step) -> Ran,
        std::rc::Rc<std::cell::RefCell<Vec<String>>>,
    ) {
        let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let log = seen.clone();
        let mut it = results.into_iter();
        let run = move |s: &Step| {
            log.borrow_mut().push(s.display());
            let (out, err, status) = it.next().expect("no more fake results");
            Ran {
                command: s.display(),
                status,
                stdout: out.to_string(),
                stderr: err.to_string(),
            }
        };
        (run, seen)
    }

    /// `launchctl print` for the installed agent while it runs (shape recorded on macOS).
    const LAUNCHD_RUNNING: &str = "gui/501/dev.rsi.rsid = {\n\tactive count = 1\n\tpath = /Users/op/Library/LaunchAgents/dev.rsi.rsid.plist\n\ttype = LaunchAgent\n\tstate = running\n\n\tprogram = /Users/op/.local/bin/rsi-socket-hold\n\targuments = {\n\t\t/Users/op/.local/bin/rsi-socket-hold\n\t\t/Users/op/.rsi/daemon.sock\n\t\t--\n\t\t/Users/op/rsid-supervisor.sh\n\t\trsid\n\t}\n\n\tpid = 4242\n\tstate = waiting\n}\n";
    /// The same agent loaded but not running (between restarts).
    const LAUNCHD_WAITING: &str = "gui/501/dev.rsi.rsid = {\n\tstate = not running\n\targuments = {\n\t\t/h/rsi-socket-hold\n\t\t/Users/op/.rsi/daemon.sock\n\t}\n}\n";

    fn launchd() -> Service {
        Service::Launchd {
            uid: 501,
            plist: PathBuf::from("/Users/op/Library/LaunchAgents/dev.rsi.rsid.plist"),
        }
    }

    fn systemd() -> Service {
        Service::Systemd {
            systemctl: PathBuf::from("/usr/bin/systemctl"),
            unit_file: PathBuf::from("/etc/systemd/user/rsid.service"),
        }
    }

    const APP: &str = "/Users/op/.rsi/daemon.sock";

    fn go(service: &Service, mode: Mode, app: &str, results: Canned) -> (Attempt, Vec<String>) {
        let p = plan(service).unwrap();
        let (mut run, seen) = fake(results);
        let default = PathBuf::from("/Users/op/.rsi/daemon.sock");
        let did = attempt(&p, service, mode, Path::new(app), Some(&default), &mut run);
        let seen = seen.borrow().clone();
        (did, seen)
    }

    fn ran_of(a: &Attempt) -> &Ran {
        match a {
            Attempt::Ran(r) => r,
            other => panic!("expected a run, got {other:?}"),
        }
    }

    #[test]
    fn the_probe_reads_state_pid_and_the_held_socket_from_launchctl_print() {
        let ok = |out: &str| Ran {
            command: String::new(),
            status: Some(0),
            stdout: out.to_string(),
            stderr: String::new(),
        };
        let ins = parse_probe(&launchd(), &ok(LAUNCHD_RUNNING));
        assert_eq!(ins.state, RunState::Running { pid: Some(4242) });
        assert!(ins.loaded);
        assert_eq!(
            ins.service_socket,
            Some(PathBuf::from("/Users/op/.rsi/daemon.sock"))
        );
        let waiting = parse_probe(&launchd(), &ok(LAUNCHD_WAITING));
        assert_eq!(waiting.state, RunState::NotRunning);
        assert!(waiting.loaded);
        let unreadable = parse_probe(&launchd(), &ok("something else"));
        assert_eq!(unreadable.state, RunState::Unknown);
        // Not loaded: launchctl exits non-zero. Could not run at all: no status.
        let gone = |status| Ran {
            command: String::new(),
            status,
            stdout: String::new(),
            stderr: "Could not find service".into(),
        };
        let unloaded = parse_probe(&launchd(), &gone(Some(113)));
        assert_eq!(
            (unloaded.loaded, unloaded.state),
            (false, RunState::NotRunning)
        );
        assert_eq!(
            parse_probe(&launchd(), &gone(None)).state,
            RunState::Unknown
        );
    }

    #[test]
    fn the_systemd_probe_reads_active_state_and_main_pid() {
        let show = |out: &str, status| Ran {
            command: String::new(),
            status,
            stdout: out.to_string(),
            stderr: String::new(),
        };
        let st = |out, status| parse_probe(&systemd(), &show(out, status)).state;
        assert_eq!(
            st("ActiveState=active\nMainPID=812\n", Some(0)),
            RunState::Running { pid: Some(812) }
        );
        assert_eq!(
            st("ActiveState=activating\nMainPID=0\n", Some(0)),
            RunState::Running { pid: None }
        );
        assert_eq!(
            st("ActiveState=inactive\nMainPID=0\n", Some(0)),
            RunState::NotRunning
        );
        assert_eq!(
            st("ActiveState=failed\nMainPID=0\n", Some(0)),
            RunState::NotRunning
        );
        assert_eq!(st("", Some(1)), RunState::Unknown);
    }

    #[test]
    fn a_running_agent_is_never_kickstarted_by_start() {
        let (did, seen) = go(
            &launchd(),
            Mode::Start,
            APP,
            vec![(LAUNCHD_RUNNING, "", Some(0))],
        );
        assert_eq!(did, Attempt::Refused(Refusal::Running { pid: Some(4242) }));
        assert_eq!(
            seen,
            ["launchctl print gui/501/dev.rsi.rsid"],
            "only the read-only probe ran"
        );
        let o = outcome("launchd", &did);
        assert_eq!(
            (o["ok"].clone(), o["refused"].clone()),
            (json!(false), json!("running"))
        );
        assert_eq!(o["command"], "");
    }

    #[test]
    fn restart_kickstarts_with_k_only_a_running_agent() {
        let (did, seen) = go(
            &launchd(),
            Mode::Restart,
            APP,
            vec![(LAUNCHD_RUNNING, "", Some(0)), ("", "", Some(0))],
        );
        assert!(ran_of(&did).ok());
        assert_eq!(
            seen,
            [
                "launchctl print gui/501/dev.rsi.rsid",
                "launchctl kickstart -k gui/501/dev.rsi.rsid"
            ]
        );
        // Not running: restart is a plain start, never `-k`.
        let (did, seen) = go(
            &launchd(),
            Mode::Restart,
            APP,
            vec![(LAUNCHD_WAITING, "", Some(0)), ("", "", Some(0))],
        );
        assert!(ran_of(&did).ok());
        assert_eq!(seen[1], "launchctl kickstart gui/501/dev.rsi.rsid");
    }

    #[test]
    fn a_loaded_idle_agent_is_kickstarted_and_an_unloaded_one_bootstrapped() {
        let (did, seen) = go(
            &launchd(),
            Mode::Start,
            APP,
            vec![(LAUNCHD_WAITING, "", Some(0)), ("", "", Some(0))],
        );
        assert!(ran_of(&did).ok());
        assert_eq!(
            seen,
            [
                "launchctl print gui/501/dev.rsi.rsid",
                "launchctl kickstart gui/501/dev.rsi.rsid"
            ]
        );
        let (did, seen) = go(
            &launchd(),
            Mode::Start,
            APP,
            vec![
                ("", "Could not find service", Some(113)),
                ("", "Bootstrap failed: 5: Input/output error", Some(5)),
            ],
        );
        assert_eq!(
            seen,
            [
                "launchctl print gui/501/dev.rsi.rsid",
                "launchctl bootstrap gui/501 /Users/op/Library/LaunchAgents/dev.rsi.rsid.plist"
            ]
        );
        let o = outcome("launchd", &did);
        assert_eq!(o["ok"], false);
        assert_eq!(o["status"], 5);
        assert_eq!(o["refused"], json!(null));
        assert_eq!(o["stderr"], "Bootstrap failed: 5: Input/output error");
    }

    #[test]
    fn a_window_on_another_socket_gets_neither_start_nor_restart() {
        for mode in [Mode::Start, Mode::Restart] {
            let (did, seen) = go(
                &launchd(),
                mode,
                "/tmp/elsewhere/none.sock",
                vec![(LAUNCHD_RUNNING, "", Some(0))],
            );
            assert_eq!(
                did,
                Attempt::Refused(Refusal::WrongSocket {
                    app: PathBuf::from("/tmp/elsewhere/none.sock"),
                    service: PathBuf::from("/Users/op/.rsi/daemon.sock"),
                })
            );
            assert_eq!(seen.len(), 1, "only the probe ran");
            assert_eq!(outcome("launchd", &did)["refused"], "wrong-socket");
        }
        // Without a socket in the probe output, the default stands in for it (systemd).
        let (did, _) = go(
            &systemd(),
            Mode::Start,
            "/tmp/x.sock",
            vec![("ActiveState=inactive\nMainPID=0\n", "", Some(0))],
        );
        assert!(matches!(did, Attempt::Refused(Refusal::WrongSocket { .. })));
    }

    #[test]
    fn systemd_starts_an_idle_unit_restarts_a_running_one_and_reports_status() {
        let (did, seen) = go(
            &systemd(),
            Mode::Start,
            APP,
            vec![
                ("ActiveState=inactive\nMainPID=0\n", "", Some(0)),
                ("", "Job for rsid.service failed.", Some(1)),
            ],
        );
        assert_eq!(seen[1], "systemctl --user start rsid.service");
        let o = outcome("systemd", &did);
        assert_eq!(
            (o["ok"].clone(), o["status"].clone()),
            (json!(false), json!(1))
        );
        let (did, _) = go(
            &systemd(),
            Mode::Start,
            APP,
            vec![("ActiveState=active\nMainPID=9\n", "", Some(0))],
        );
        assert_eq!(did, Attempt::Refused(Refusal::Running { pid: Some(9) }));
        let (did, seen) = go(
            &systemd(),
            Mode::Restart,
            APP,
            vec![
                ("ActiveState=active\nMainPID=9\n", "", Some(0)),
                ("", "", Some(0)),
            ],
        );
        assert!(ran_of(&did).ok());
        assert_eq!(seen[1], "systemctl --user restart rsid.service");
    }

    #[test]
    fn describe_installed_reports_state_pid_and_whether_the_sockets_match() {
        let ins = parse_probe(
            &launchd(),
            &Ran {
                command: String::new(),
                status: Some(0),
                stdout: LAUNCHD_RUNNING.to_string(),
                stderr: String::new(),
            },
        );
        let d = describe_installed(&launchd(), &ins, Path::new(APP), None);
        assert_eq!(
            (d["kind"].clone(), d["state"].clone(), d["pid"].clone()),
            (json!("installed"), json!("running"), json!(4242))
        );
        assert_eq!(
            d["socket"],
            json!({"app": APP, "service": APP, "matches": true})
        );
        let other = describe_installed(&launchd(), &ins, Path::new("/tmp/none.sock"), None);
        assert_eq!(other["socket"]["matches"], false);
        assert_eq!(other["socket"]["service"], APP);
        // The default stands in when the manager named no socket; none known means no verdict against it.
        let bare = Inspection {
            loaded: true,
            state: RunState::NotRunning,
            service_socket: None,
        };
        let d = describe_installed(
            &systemd(),
            &bare,
            Path::new("/tmp/none.sock"),
            Some(Path::new(APP)),
        );
        assert_eq!(
            (d["state"].clone(), d["socket"]["matches"].clone()),
            (json!("stopped"), json!(false))
        );
        assert_eq!(
            describe_installed(&systemd(), &bare, Path::new("/tmp/none.sock"), None)["socket"]["matches"],
            true
        );
    }
}
