//! Complete holder proof and namespace-aware lander liveness (#1140).
//!
//! A candidate is only reclaimed when every same-user process's cwd, root,
//! exe, open descriptors and file mappings were read, for the process and for
//! every thread that does not provably share that state with it, and none
//! refers to the candidate. A read error that is not "the process or thread
//! just exited" leaves the proof incomplete, which retains the candidate.

use std::collections::HashSet;
use std::io;
use std::os::fd::RawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use super::fsys::{Budget, Ident};

/// Largest `maps` file read for one task; a bigger one is not proven complete.
const MAPS_LIMIT: usize = 32 << 20;

#[derive(Debug)]
struct Held {
    /// Path as the holder sees it (a trailing ` (deleted)` is kept: the prefix
    /// still names the directory the file was unlinked from).
    path: PathBuf,
    /// Device and inode of the held object, when it could be resolved.
    ident: Option<Ident>,
}

/// Snapshot of what same-user processes hold open.
#[derive(Debug, Default)]
pub(super) struct Holders {
    held: Vec<Held>,
}

impl Holders {
    /// Whether any holder refers to `candidate` by path or (when `idents` is
    /// given) by inode, which also catches bind/chroot views of the same tree.
    pub(super) fn holds(&self, candidate: &Path, idents: Option<&HashSet<Ident>>) -> bool {
        self.held.iter().any(|held| {
            held.path.starts_with(candidate)
                || match (held.ident, idents) {
                    (Some(ident), Some(set)) => set.contains(&ident),
                    _ => false,
                }
        })
    }
}

/// `/proc/<pid>` of the systemd user manager and its PAM helper hold no
/// scratch and restrict their own inspection. Authenticated by the exact
/// process name and by living in the user manager's own cgroup, not by name
/// alone.
fn is_user_manager(dir: &Path, uid: u32) -> bool {
    let comm_ok = std::fs::read_to_string(dir.join("comm"))
        .is_ok_and(|comm| matches!(comm.trim(), "(sd-pam)" | "systemd"));
    comm_ok
        && std::fs::read_to_string(dir.join("cgroup")).is_ok_and(|cgroup| {
            cgroup
                .lines()
                .any(|line| line.ends_with(&format!("/user@{uid}.service/init.scope")))
        })
}

// `kcmp(2)` types: do two tasks share the same resource?
const KCMP_VM: i32 = 1;
const KCMP_FILES: i32 = 2;
const KCMP_FS: i32 = 3;

/// `Some(true)` when tasks `a` and `b` provably share the resource, `Some(false)`
/// when they provably do not, `None` when the kernel cannot say (no `kcmp`, no
/// permission, a task gone): then the task is scanned in full.
#[cfg(target_os = "linux")]
fn shares(a: u32, b: u32, kind: i32) -> Option<bool> {
    // SAFETY: kcmp takes plain integers and has no pointer arguments here.
    let rc = unsafe {
        nix::libc::syscall(
            nix::libc::SYS_kcmp,
            i64::from(a),
            i64::from(b),
            i64::from(kind),
            0i64,
            0i64,
        )
    };
    match rc {
        0 => Some(true),
        r if r > 0 => Some(false),
        _ => None,
    }
}

#[cfg(not(target_os = "linux"))]
fn shares(_a: u32, _b: u32, _kind: i32) -> Option<bool> {
    None
}

/// Why a scan could not be completed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// The kernel refused a read of this process (typically a non-dumpable
    /// process). Nothing proves it holds nothing: the proof is incomplete.
    Denied,
    /// Another read error that is not "it just exited".
    Unexplained,
    /// The pass budget ran out.
    Budget,
}

struct Incomplete {
    stop: Stop,
    /// Which part of the process could not be read (`fd`, `cwd`, ...).
    refused: String,
}

impl Incomplete {
    fn budget() -> Self {
        Self {
            stop: Stop::Budget,
            refused: String::new(),
        }
    }

    fn from_io(error: &io::Error, refused: &str) -> Self {
        Self {
            stop: if error.kind() == io::ErrorKind::PermissionDenied {
                Stop::Denied
            } else {
                Stop::Unexplained
            },
            refused: refused.to_string(),
        }
    }
}

/// Most processes named per failed inventory, and in one report.
pub(super) const MAX_BLOCKERS: usize = 8;

/// A process that kept the holder proof incomplete. Every field is what the
/// kernel reports, for the operator to read: `comm` is display only and is
/// never an input to any decision.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Blocker {
    pub pid: u32,
    /// `/proc/<pid>/comm`, or `unreadable`. Display only.
    pub comm: String,
    /// Real, effective, saved and filesystem uid, when readable.
    pub uids: Option<[u32; 4]>,
    pub parent_pid: Option<u32>,
    /// The parent's real uid, when readable.
    pub parent_uid: Option<u32>,
    /// What could not be read: `fd`, `cwd`, `root`, `exe`, `maps`, `task`.
    pub refused: String,
    /// `permission denied`, or `read error`.
    pub why: String,
    /// One line for a human.
    pub summary: String,
}

/// Why the holder proof of an unproven candidate is incomplete.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct ProofFailure {
    /// `process-unreadable`, `budget` or `inventory-unreadable`.
    pub cause: String,
    pub blockers: Vec<Blocker>,
    /// Blocking processes not named because of the cap.
    pub omitted: u32,
}

impl ProofFailure {
    fn inventory() -> Self {
        Self {
            cause: "inventory-unreadable".into(),
            ..Self::default()
        }
    }
}

/// Printable, bounded, single-line text from the kernel.
pub(super) fn display_only(text: &str) -> String {
    text.trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(32)
        .collect()
}

/// Describe a blocking process from world-readable `/proc` files. Display
/// only: nothing here decides whether a candidate is held.
fn blocker(proc_root: &Path, pid: u32, incomplete: &Incomplete) -> Blocker {
    let read = |pid: u32, file: &str| {
        std::fs::read_to_string(proc_root.join(pid.to_string()).join(file)).ok()
    };
    let comm = read(pid, "comm")
        .map(|c| display_only(&c))
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| "unreadable".into());
    let uids = read(pid, "status").and_then(|s| id_line(&s, "Uid"));
    let parent_pid = read(pid, "stat").and_then(|s| stat_ppid(&s));
    let parent_uid = parent_pid
        .and_then(|ppid| read(ppid, "status"))
        .and_then(|s| id_line(&s, "Uid"))
        .map(|ids| ids[0]);
    let why = if incomplete.stop == Stop::Denied {
        "permission denied"
    } else {
        "read error"
    };
    let uid_text = uids.map_or_else(
        || "uid unknown".to_string(),
        |ids| format!("uid {}", ids[0]),
    );
    let parent_text = match (parent_pid, parent_uid) {
        (Some(ppid), Some(0)) => format!("parent {ppid} root-owned"),
        (Some(ppid), Some(uid)) => format!("parent {ppid} uid {uid}"),
        (Some(ppid), None) => format!("parent {ppid}"),
        _ => "parent unknown".to_string(),
    };
    Blocker {
        pid,
        summary: format!(
            "kept: unreadable process {pid} ({comm}, {uid_text}, {parent_text}); {why} reading {}",
            incomplete.refused
        ),
        comm,
        uids,
        parent_pid,
        parent_uid,
        refused: incomplete.refused.clone(),
        why: why.into(),
    }
}

/// The kernel's `Uid:` line: real, effective, saved, filesystem.
pub(super) fn id_line(status: &str, key: &str) -> Option<[u32; 4]> {
    let rest = status
        .lines()
        .find_map(|line| line.strip_prefix(key)?.strip_prefix(':'))?;
    let mut ids = rest.split_whitespace().map(|n| n.parse::<u32>().ok());
    Some([ids.next()??, ids.next()??, ids.next()??, ids.next()??])
}

/// The parent pid from a `/proc/<pid>/stat`. The command name may hold spaces
/// and parentheses, so fields are counted after its last `)`.
pub(super) fn stat_ppid(stat: &str) -> Option<u32> {
    // After the name: state, then ppid.
    stat.get(stat.rfind(')')? + 1..)?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// What a task has that the leader's inventory does not already cover.
#[derive(Clone, Copy)]
struct TaskScope {
    fs: bool,
    files: bool,
    vm: bool,
}

struct Scanner<'a> {
    held: Vec<Held>,
    budget: &'a mut Budget,
    own_pid: String,
    own_fds: &'a [RawFd],
}

impl Scanner<'_> {
    /// Record a link's target and the identity it resolves to. A target that is
    /// not a file-system path (`pipe:[n]`) cannot be inside a candidate.
    fn push_link(&mut self, link: &Path, target: PathBuf) -> Result<(), Incomplete> {
        if !target.is_absolute() {
            return Ok(());
        }
        let ident = match std::fs::metadata(link) {
            Ok(meta) => Some(Ident {
                dev: meta.dev(),
                ino: meta.ino(),
            }),
            // Closed or exited while we looked: nothing held.
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            // Cannot say what this absolute path is.
            Err(error) => return Err(Incomplete::from_io(&error, "link")),
        };
        self.held.push(Held {
            path: target,
            ident,
        });
        Ok(())
    }

    fn scan_link(&mut self, task: &Path, name: &str, exempt: bool) -> Result<(), Incomplete> {
        let link = task.join(name);
        match std::fs::read_link(&link) {
            Ok(target) => self.push_link(&link, target),
            // A zombie or just-exited task has no cwd/root/exe.
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(_) if exempt => Ok(()),
            Err(error) => Err(Incomplete::from_io(&error, name)),
        }
    }

    fn scan_fds(&mut self, task: &Path, pid: &str, exempt: bool) -> Result<(), Incomplete> {
        let fds = match std::fs::read_dir(task.join("fd")) {
            Ok(fds) => fds,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(_) if exempt => return Ok(()),
            Err(error) => return Err(Incomplete::from_io(&error, "fd")),
        };
        for fd in fds {
            let fd = match fd {
                Ok(fd) => fd,
                Err(error) if error.kind() == io::ErrorKind::NotFound => break,
                Err(error) => return Err(Incomplete::from_io(&error, "fd")),
            };
            if !self.budget.spend(1) {
                return Err(Incomplete::budget());
            }
            if pid == self.own_pid
                && fd
                    .file_name()
                    .to_str()
                    .and_then(|n| n.parse::<RawFd>().ok())
                    .is_some_and(|n| self.own_fds.contains(&n))
            {
                continue;
            }
            match std::fs::read_link(fd.path()) {
                Ok(target) => self.push_link(&fd.path(), target)?,
                // The descriptor closed while we looked.
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                // The user manager lists its fds but denies the links.
                Err(_) if exempt => {}
                Err(error) => return Err(Incomplete::from_io(&error, "fd")),
            }
        }
        Ok(())
    }

    fn scan_maps(&mut self, task: &Path, exempt: bool) -> Result<(), Incomplete> {
        use std::io::Read;
        let file = match std::fs::File::open(task.join("maps")) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(_) if exempt => return Ok(()),
            Err(error) => return Err(Incomplete::from_io(&error, "maps")),
        };
        let mut maps = String::new();
        match file.take(MAPS_LIMIT as u64 + 1).read_to_string(&mut maps) {
            Ok(_) if maps.len() > MAPS_LIMIT => return Err(Incomplete::budget()),
            Ok(_) => {}
            // The task exited mid-read.
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(_) if exempt => return Ok(()),
            Err(error) => return Err(Incomplete::from_io(&error, "maps")),
        }
        for line in maps.lines() {
            if !self.budget.spend(1) {
                return Err(Incomplete::budget());
            }
            if let Some((path, ident)) = parse_maps_line(line) {
                self.held.push(Held { path, ident });
            }
        }
        Ok(())
    }

    fn scan_task(
        &mut self,
        task: &Path,
        pid: &str,
        scope: TaskScope,
        exempt: bool,
    ) -> Result<(), Incomplete> {
        if scope.fs {
            self.scan_link(task, "cwd", exempt)?;
            self.scan_link(task, "root", exempt)?;
        }
        if scope.vm {
            self.scan_link(task, "exe", exempt)?;
            self.scan_maps(task, exempt)?;
        }
        if scope.files {
            self.scan_fds(task, pid, exempt)?;
        }
        Ok(())
    }
}

/// Mapping lines carry `address perms offset dev inode path`; the device is
/// `major:minor` in hex.
fn parse_maps_line(line: &str) -> Option<(PathBuf, Option<Ident>)> {
    let mut rest = line;
    let mut fields = [""; 5];
    for field in &mut fields {
        rest = rest.trim_start();
        let end = rest.find(char::is_whitespace)?;
        *field = &rest[..end];
        rest = &rest[end..];
    }
    let path = rest.trim();
    if !path.starts_with('/') {
        return None;
    }
    let ident = fields[3].split_once(':').and_then(|(major, minor)| {
        let major = u32::from_str_radix(major, 16).ok()?;
        let minor = u32::from_str_radix(minor, 16).ok()?;
        Some(Ident {
            dev: nix::libc::makedev(major as _, minor as _) as u64,
            ino: fields[4].parse().ok()?,
        })
    });
    Some((PathBuf::from(path), ident))
}

fn numeric(name: &std::ffi::OsStr) -> Option<&str> {
    name.to_str()
        .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Scan one process: its task list, then each task.
fn scan_process(
    scanner: &mut Scanner<'_>,
    dir: &Path,
    pid: &str,
    exempt: bool,
) -> Result<(), Incomplete> {
    let leader: Option<u32> = pid.parse().ok();
    let tasks = match std::fs::read_dir(dir.join("task")) {
        Ok(tasks) => tasks,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(_) if exempt => return Ok(()),
        Err(error) => return Err(Incomplete::from_io(&error, "task")),
    };
    for task in tasks {
        let task = match task {
            Ok(task) => task,
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(_) if exempt => break,
            Err(error) => return Err(Incomplete::from_io(&error, "task")),
        };
        let task_name = task.file_name();
        let Some(tid) = numeric(&task_name) else {
            continue;
        };
        if !scanner.budget.spend(1) {
            return Err(Incomplete::budget());
        }
        let scope = match (leader, tid.parse::<u32>().ok()) {
            (Some(leader), Some(tid)) if tid != leader => TaskScope {
                fs: shares(leader, tid, KCMP_FS) != Some(true),
                files: shares(leader, tid, KCMP_FILES) != Some(true),
                vm: shares(leader, tid, KCMP_VM) != Some(true),
            },
            _ => TaskScope {
                fs: true,
                files: true,
                vm: true,
            },
        };
        scanner.scan_task(&task.path(), pid, scope, exempt)?;
    }
    Ok(())
}

/// Inventory every same-user process and thread, or the reason it cannot be
/// proven complete (an unreadable inventory, a refused or unexplained read, or
/// the pass budget).
///
/// A thread is scanned for each of cwd/root, exe/maps and descriptors unless
/// `kcmp` proves it shares that state with its thread-group leader; a thread
/// that cannot be compared is scanned in full. `own_fds` are descriptors this
/// pass itself holds open (its pinned root and candidate); they are not
/// holders.
///
/// There is no excuse for a process the kernel refuses to show (a non-dumpable
/// one, such as `sshd-session`): nothing readable without ptrace says what it
/// holds, and a fact about who the process is (its uid, parent or start time)
/// does not say it cannot reference this directory. Such a process is named in
/// the failure so the operator can act; the candidate is retained.
pub(super) fn scan(
    proc_root: &Path,
    uid: u32,
    own_fds: &[RawFd],
    budget: &mut Budget,
) -> Result<Holders, ProofFailure> {
    let mut scanner = Scanner {
        held: Vec::new(),
        budget,
        own_pid: std::process::id().to_string(),
        own_fds,
    };
    let mut failure: Option<ProofFailure> = None;
    let listing = std::fs::read_dir(proc_root).map_err(|_| ProofFailure::inventory())?;
    for entry in listing {
        let entry = entry.map_err(|_| ProofFailure::inventory())?;
        let file_name = entry.file_name();
        let Some(pid) = numeric(&file_name) else {
            continue;
        };
        if !scanner.budget.spend(1) {
            return Err(budget_failure(failure));
        }
        let dir = proc_root.join(pid);
        match std::fs::metadata(&dir) {
            Ok(meta) if meta.uid() != uid => continue,
            Ok(_) => {}
            // The process exited between listing and inspection.
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => return Err(ProofFailure::inventory()),
        }
        let exempt = is_user_manager(&dir, uid);
        let Err(incomplete) = scan_process(&mut scanner, &dir, pid, exempt) else {
            continue;
        };
        if incomplete.stop == Stop::Budget {
            return Err(budget_failure(failure));
        }
        // Keep looking: the operator is told about every blocker (up to the
        // cap), not only the first.
        let failure = failure.get_or_insert_with(|| ProofFailure {
            cause: "process-unreadable".into(),
            ..ProofFailure::default()
        });
        match pid.parse::<u32>() {
            Ok(numeric_pid) if failure.blockers.len() < MAX_BLOCKERS => failure
                .blockers
                .push(blocker(proc_root, numeric_pid, &incomplete)),
            _ => failure.omitted += 1,
        }
    }
    match failure {
        Some(failure) => Err(failure),
        None => Ok(Holders { held: scanner.held }),
    }
}

fn budget_failure(failure: Option<ProofFailure>) -> ProofFailure {
    let mut failure = failure.unwrap_or_default();
    failure.cause = "budget".into();
    failure
}

// ---- lander ownership -------------------------------------------------------

/// Identity of this process's PID namespace and boot, written into an owner
/// file so a sweep can tell whether the recorded pid means anything to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct HostIdentity {
    pub pid_ns: String,
    pub boot_id: String,
}

impl HostIdentity {
    pub(super) fn read(proc_root: &Path) -> Option<Self> {
        let pid_ns = std::fs::read_link(proc_root.join("self/ns/pid")).ok()?;
        let boot_id = std::fs::read_to_string(proc_root.join("sys/kernel/random/boot_id")).ok()?;
        Some(Self {
            pid_ns: pid_ns.to_str()?.to_string(),
            boot_id: boot_id.trim().to_string(),
        })
    }
}

/// Owner file body: `v2 pid ticks pid_ns boot_id sandbox`.
pub(super) fn owner_text(pid: u32, ticks: u64, host: &HostIdentity, sandbox: &Path) -> String {
    format!(
        "v2 {pid} {ticks} {} {} {}\n",
        host.pid_ns,
        host.boot_id,
        sandbox.display()
    )
}

/// What reading a workspace's owner file found.
pub(super) enum OwnerFile {
    /// There is no owner file (the lander never registered one, or it was
    /// removed at the end of a delete).
    Absent,
    Text(String),
    /// It exists but cannot be read.
    Unreadable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LanderOwner {
    Live,
    Gone,
    /// No owner file: the age gate decides.
    Unregistered,
    /// The recorded pid cannot be interpreted from here (another PID
    /// namespace, unreadable host identity, an unreadable or malformed owner
    /// file): never reclaim.
    Ambiguous,
}

/// Whether the registered owner is still running, interpreted in the
/// namespace it was recorded in.
pub(super) fn lander_owner(owner_file: &OwnerFile, proc_root: &Path) -> LanderOwner {
    let text = match owner_file {
        OwnerFile::Absent => return LanderOwner::Unregistered,
        OwnerFile::Unreadable => return LanderOwner::Ambiguous,
        OwnerFile::Text(text) => text,
    };
    let mut fields = text.split_whitespace();
    let (Some("v2"), Some(Ok(pid)), Some(Ok(ticks)), Some(pid_ns), Some(boot_id)) = (
        fields.next(),
        fields.next().map(str::parse::<u32>),
        fields.next().map(str::parse::<u64>),
        fields.next(),
        fields.next(),
    ) else {
        // A malformed or legacy owner cannot name its namespace.
        return LanderOwner::Ambiguous;
    };
    let Some(host) = HostIdentity::read(proc_root) else {
        return LanderOwner::Ambiguous;
    };
    if host.boot_id != boot_id {
        // The machine rebooted: no process of that boot is alive.
        return LanderOwner::Gone;
    }
    if host.pid_ns != pid_ns || ticks == 0 {
        return LanderOwner::Ambiguous;
    }
    match std::fs::read_to_string(proc_root.join(pid.to_string()).join("stat")) {
        Ok(stat) => match crate::governor::parse_start_ticks(&stat) {
            Some(live) if live == ticks => LanderOwner::Live,
            // The pid was reused by another process.
            Some(_) => LanderOwner::Gone,
            None => LanderOwner::Ambiguous,
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => LanderOwner::Gone,
        Err(_) => LanderOwner::Ambiguous,
    }
}
