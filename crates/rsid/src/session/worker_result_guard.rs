//! Issue #1098: a worker never silently ends without a `RESULT` line.
//!
//! A leaf worker with a parent (a manager or lead owns it) that ends a turn
//! without a final `RESULT ...` line while a process it spawned or a daemon job
//! it owns is still running gets exactly one automatic continuation: a one-shot
//! same-session Resume wake telling it to use a daemon job and a completion
//! wake, then report. A worker that ended its turn with an armed durable wake of
//! its own (`when.jobs_terminal`, `resume`, `on_terminal`) or an open
//! `wake: owner` job is waiting correctly: the daemon will resume it, so it
//! gets no nudge (#1734, #1958, #364). A second miss settles the turn with
//! `stop_reason = "no_result"` (surfaced as the terminal reason) and never
//! continues again. A final report is also a result line the launch brief
//! declares or a handoff the worker published to its Issue (#151). Operator sessions, managers and Epic leads are untouched.

use std::sync::Arc;

use rsi_common::types::{Session, SessionStatus};
use uuid::Uuid;

use crate::store::Store;

#[path = "worker_result_sha.rs"]
pub(super) mod result_sha;

/// Closed `stop_reason` recorded when the worker misses its RESULT twice.
pub(crate) const NO_RESULT_STOP_REASON: &str = "no_result";

// The store half (constants and `impl Store`) lives in `store::worker_no_result`
// so `store` has no edge into `session` (issue #1021 S4).
pub(crate) use crate::store::worker_no_result::{
    NO_RESULT_MARKER, NO_RESULT_RETIRED, NO_RESULT_WAKE_NAME_PREFIX, no_result_wake_id,
};

/// True for the refusal that retires a no-result continuation wake.
pub(crate) fn is_no_result_retired(error: &crate::error::DaemonError) -> bool {
    matches!(error, crate::error::DaemonError::InvalidParam(code) if code == NO_RESULT_RETIRED)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoResultVerdict {
    /// Not a covered worker, or the turn reported its RESULT, or nothing runs.
    NotApplicable,
    /// First miss with owned work still running: continue once.
    FirstMiss,
    /// Already continued once and still no RESULT: settle `no_result`.
    SecondMiss,
}

/// True when any line of `text` is a final report: a `RESULT` line or a
/// `PIPELINE HANDOFF` marker (#1392; see `store::worker_no_result`).
pub(crate) fn has_result_line(text: &str) -> bool {
    crate::store::worker_no_result::is_final_report(text)
}

/// #1392: true when any assistant message of the current turn (after the last
/// user message) is a final report. The monitor's turn accumulator joins
/// messages with no separator, so a `RESULT` message that follows prose
/// without a trailing newline lands mid-line there; each message is checked
/// on its own here.
pub(crate) fn turn_messages_report(events: &[rsi_common::types::ConversationEvent]) -> bool {
    use rsi_common::types::{EventType, Role};
    events
        .iter()
        .rev()
        .filter(|event| event.event_type == EventType::Message)
        .take_while(|event| event.role != Some(Role::User))
        .any(|event| event.role == Some(Role::Assistant) && has_result_line(&event.content))
}

/// #1399: long-lived helpers a provider starts for itself (the native-tool
/// MCP gateway, Codex's code-mode runner, other stdio MCP servers). They live
/// as long as the provider, so counting them reported "your run is still
/// going" after every command had exited. `comm` is the kernel's short
/// command name (15 bytes on Linux, 16 on macOS).
pub(crate) fn is_provider_helper_comm(comm: &str) -> bool {
    matches!(comm, "rsi-agent-mcp" | "codex-code-mode")
        || comm.ends_with("-mcp")
        || comm.starts_with("mcp-")
        || comm.contains("mcp-server")
        || comm.contains("mcp_server")
}

/// How long a descendant must survive between two samples to count as a
/// still-running run (#1459). A foreground command that just returned exits
/// its shell, wrappers and pipe tails within milliseconds of the provider
/// emitting its result; a single sample at that instant saw those exiting
/// processes and reported "your run is still going" for a finished test.
pub(crate) const DESCENDANT_SETTLE: std::time::Duration = std::time::Duration::from_millis(400);

/// Why the process table could not prove "no owned children" (#3378). Every
/// variant is an *unknown*, never an empty table: the result boundary treats it
/// as "cannot finalize" so a worker with a live child is not abandoned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProbeUnknown {
    /// The process list itself could not be read (`/proc` unreadable,
    /// `proc_listallpids` failed or stayed saturated).
    EnumerationFailed,
    /// The list was read but at least one candidate row was unreadable or
    /// malformed, so that process could be an owned child.
    RowUnreadable,
    /// The platform has no process-table reader (constructed only where
    /// `process_table` has no Linux or macOS implementation).
    #[cfg_attr(any(target_os = "linux", target_os = "macos"), allow(dead_code))]
    Unsupported,
    /// The blocking probe task did not complete.
    ProbeTaskFailed,
}

impl ProbeUnknown {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::EnumerationFailed => "process_table_enumeration_failed",
            Self::RowUnreadable => "process_table_row_unreadable",
            Self::Unsupported => "process_table_unsupported",
            Self::ProbeTaskFailed => "process_probe_task_failed",
        }
    }
}

/// Typed outcome of the owned-children probe (#3378).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChildrenOutcome {
    /// A complete table showed no live descendant that survived the settle.
    None,
    /// At least one descendant survived the settle window.
    Alive,
    /// The table could not be read well enough to say; fail closed.
    Unknown(ProbeUnknown),
}

impl ChildrenOutcome {
    /// True when the result boundary must not conclude "no owned work":
    /// a live child, or an unknown (fail closed).
    pub(crate) fn blocks_finalize(self) -> bool {
        !matches!(self, Self::None)
    }
}

/// Re-reads of an incomplete table before the probe settles on unknown.
pub(crate) const UNKNOWN_RETRIES: usize = 2;
/// Pause between those re-reads.
pub(crate) const UNKNOWN_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(50);

/// One process-table row: what the descendant walk needs of a process.
#[derive(Clone)]
pub(crate) struct ProcRow {
    pub(crate) pid: u32,
    pub(crate) ppid: u32,
    /// The kernel's short command name (15 bytes on Linux, 16 on macOS).
    pub(crate) comm: String,
    pub(crate) zombie: bool,
}

/// A read of the process table: the rows seen and how many candidates could
/// not be read (a process that merely exited mid-read is not counted).
#[derive(Default, Clone)]
pub(crate) struct ProcessTable {
    pub(crate) rows: Vec<ProcRow>,
    pub(crate) unreadable_rows: usize,
}

/// Parse `/proc/<pid>/stat`: `pid (comm) state ppid ...`; comm may contain
/// spaces and parens. `None` when the line is malformed.
#[cfg(any(target_os = "linux", test))]
fn parse_proc_stat(pid: u32, stat: &str) -> Option<ProcRow> {
    let (head, rest) = stat.rsplit_once(')')?;
    let comm = head.split_once('(').map_or("", |(_, comm)| comm);
    let mut fields = rest.split_whitespace();
    let state = fields.next();
    let ppid = fields.next()?.parse::<u32>().ok()?;
    Some(ProcRow {
        pid,
        ppid,
        comm: comm.to_string(),
        zombie: matches!(state, Some("Z" | "X" | "x")),
    })
}

/// True when a read failed only because the process is gone (it exited between
/// the listing and the read): `ENOENT`, or `ESRCH` while it was dying.
#[cfg(any(target_os = "linux", test))]
fn process_gone(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::NotFound || error.raw_os_error() == Some(3)
}

/// Read a `/proc`-shaped directory (`<root>/<pid>/stat`).
#[cfg(any(target_os = "linux", test))]
pub(crate) fn read_proc_dir(root: &std::path::Path) -> Result<ProcessTable, ProbeUnknown> {
    let entries = std::fs::read_dir(root).map_err(|_| ProbeUnknown::EnumerationFailed)?;
    let mut table = ProcessTable::default();
    for entry in entries {
        let Ok(entry) = entry else {
            // A directory entry that cannot be read may be any process.
            table.unreadable_rows += 1;
            continue;
        };
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        match std::fs::read_to_string(entry.path().join("stat")) {
            Ok(stat) => match parse_proc_stat(pid, &stat) {
                Some(row) => table.rows.push(row),
                None => table.unreadable_rows += 1,
            },
            Err(error) if process_gone(&error) => {}
            Err(_) => table.unreadable_rows += 1,
        }
    }
    Ok(table)
}

/// Every process the host lists, by `/proc/*/stat`.
#[cfg(target_os = "linux")]
fn process_table() -> Result<ProcessTable, ProbeUnknown> {
    read_proc_dir(std::path::Path::new("/proc"))
}

/// Spare slots over the pid-count estimate, for processes started between the
/// estimate and the listing.
#[cfg(any(target_os = "macos", test))]
const PID_LIST_HEADROOM: usize = 64;

/// Listings tried, each with twice the buffer of the last, before a still-full
/// buffer is an enumeration failure.
#[cfg(any(target_os = "macos", test))]
pub(crate) const PID_LIST_ATTEMPTS: usize = 4;

/// Every pid `list` reports. `list` fills the buffer it is given and returns
/// how many entries it wrote (`None` when the call failed). A count that fills
/// the buffer may have dropped pids, so the listing reruns with a doubled
/// buffer; still full after `PID_LIST_ATTEMPTS`, or a failed call, is
/// `EnumerationFailed`, never a partial list read as complete (hub #3398).
#[cfg(any(target_os = "macos", test))]
pub(crate) fn list_all_pids_with(
    estimate: usize,
    mut list: impl FnMut(&mut [i32]) -> Option<usize>,
) -> Result<Vec<i32>, ProbeUnknown> {
    let mut capacity = estimate.saturating_add(PID_LIST_HEADROOM);
    for _ in 0..PID_LIST_ATTEMPTS {
        let mut pids = vec![0; capacity];
        let count = list(&mut pids).ok_or(ProbeUnknown::EnumerationFailed)?;
        if count < pids.len() {
            pids.truncate(count);
            return Ok(pids);
        }
        capacity = capacity.saturating_mul(2);
    }
    Err(ProbeUnknown::EnumerationFailed)
}

/// Every process the host lists, by `proc_listallpids` and
/// `proc_pidinfo(PROC_PIDT_SHORTBSDINFO)`: macOS has no `/proc` (#193).
#[cfg(target_os = "macos")]
fn process_table() -> Result<ProcessTable, ProbeUnknown> {
    use nix::libc;
    // SAFETY: a null buffer of size 0 only asks for the current pid count.
    let estimate = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    let Ok(estimate) = usize::try_from(estimate) else {
        return Err(ProbeUnknown::EnumerationFailed);
    };
    let pids = list_all_pids_with(estimate, |pids| {
        let bytes = libc::c_int::try_from(std::mem::size_of_val(pids)).ok()?;
        // SAFETY: the buffer holds `bytes` bytes and outlives the call; the
        // kernel writes at most that many.
        let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
        usize::try_from(count).ok()
    })?;
    let mut table = ProcessTable::default();
    for pid in pids.into_iter().filter(|&pid| pid > 0) {
        match bsd_row_from(read_bsd_short_info(pid)) {
            BsdRow::Row(row) => table.rows.push(row),
            BsdRow::Gone => {}
            BsdRow::Unreadable => table.unreadable_rows += 1,
        }
    }
    Ok(table)
}

/// `struct proc_bsdshortinfo` from `<sys/proc_info.h>` (libc does not bind
/// it). The kernel fills every field; the walk reads pid, ppid, status and
/// comm.
#[cfg(any(target_os = "macos", test))]
#[repr(C)]
#[derive(Clone, Copy, Default)]
#[allow(dead_code)]
pub(crate) struct ProcBsdShortInfo {
    pub(crate) pbsi_pid: u32,
    pub(crate) pbsi_ppid: u32,
    pub(crate) pbsi_pgid: u32,
    pub(crate) pbsi_status: u32,
    pub(crate) pbsi_comm: [u8; 16],
    pub(crate) pbsi_flags: u32,
    pub(crate) pbsi_uid: u32,
    pub(crate) pbsi_gid: u32,
    pub(crate) pbsi_ruid: u32,
    pub(crate) pbsi_rgid: u32,
    pub(crate) pbsi_svuid: u32,
    pub(crate) pbsi_svgid: u32,
    pub(crate) pbsi_rfu: u32,
}

#[cfg(any(target_os = "macos", test))]
const _: () = assert!(std::mem::size_of::<ProcBsdShortInfo>() == 64);

/// `SZOMB` from `<sys/proc.h>`: the process is a zombie.
#[cfg(any(target_os = "macos", test))]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) const BSD_SZOMB: u32 = 5;

#[cfg(target_os = "macos")]
const _: () = assert!(BSD_SZOMB == nix::libc::SZOMB);

/// One macOS process row read.
#[cfg(any(target_os = "macos", test))]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) enum BsdRow {
    Row(ProcRow),
    /// The process exited between the listing and the read (`ESRCH`).
    Gone,
    /// The kernel refused the read or returned a short struct.
    Unreadable,
}

/// Classify one short-info read: the filled struct, or the error of a refused
/// or short read. The short info is readable for every user's process (#251,
/// hub #3401): `PROC_PIDTBSDINFO` refuses another uid's process with `EPERM`,
/// which counted every such host process as unreadable and failed every probe
/// closed. Only `ESRCH` (the process exited) is gone; any other refusal stays
/// unreadable, since that process could be an owned child.
#[cfg(any(target_os = "macos", test))]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn bsd_row_from(read: std::io::Result<ProcBsdShortInfo>) -> BsdRow {
    match read {
        Ok(info) => {
            let len = info.pbsi_comm.iter().position(|&byte| byte == 0);
            let comm = &info.pbsi_comm[..len.unwrap_or(info.pbsi_comm.len())];
            BsdRow::Row(ProcRow {
                pid: info.pbsi_pid,
                ppid: info.pbsi_ppid,
                comm: String::from_utf8_lossy(comm).into_owned(),
                zombie: info.pbsi_status == BSD_SZOMB,
            })
        }
        // ESRCH is 3 on macOS and Linux.
        Err(error) if error.raw_os_error() == Some(3) => BsdRow::Gone,
        Err(_) => BsdRow::Unreadable,
    }
}

/// `proc_pidinfo(PROC_PIDT_SHORTBSDINFO)` for `pid`; a short read is an error
/// with no OS code.
#[cfg(target_os = "macos")]
pub(crate) fn read_bsd_short_info(pid: nix::libc::c_int) -> std::io::Result<ProcBsdShortInfo> {
    use nix::libc;
    /// `PROC_PIDT_SHORTBSDINFO` from `<sys/proc_info.h>`.
    const PROC_PIDT_SHORTBSDINFO: libc::c_int = 13;
    let mut info = ProcBsdShortInfo::default();
    let size = libc::c_int::try_from(std::mem::size_of::<ProcBsdShortInfo>())
        .map_err(std::io::Error::other)?;
    // SAFETY: `info` is a properly sized and aligned `proc_bsdshortinfo` that
    // outlives the call; proc_pidinfo writes at most `size` bytes into it.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            PROC_PIDT_SHORTBSDINFO,
            0,
            (&mut info as *mut ProcBsdShortInfo).cast(),
            size,
        )
    };
    if written == size {
        Ok(info)
    } else if written <= 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Err(std::io::Error::other("short proc_bsdshortinfo read"))
    }
}

/// No process table on other platforms: the answer is an explicit unknown,
/// never "no children" (#3378).
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_table() -> Result<ProcessTable, ProbeUnknown> {
    Err(ProbeUnknown::Unsupported)
}

/// Direct children of `pid`, provider helpers and zombies included (test
/// fixtures use it to wait for a stand-in provider tree to start).
#[cfg(test)]
pub(crate) fn direct_child_count(pid: u32) -> usize {
    process_table().map_or(0, |table| {
        table.rows.iter().filter(|row| row.ppid == pid).count()
    })
}

/// One sample of the descendants of a pid: the definite live set and, when the
/// table was incomplete, why that set may be missing processes.
#[derive(Default)]
pub(crate) struct DescendantSample {
    pub(crate) live: std::collections::HashSet<u32>,
    pub(crate) unknown: Option<ProbeUnknown>,
}

/// Descendants of `pid` in one read of `table`, excluding zombies, provider
/// helpers and everything below a helper (#1399).
pub(crate) fn sample_descendants(
    pid: u32,
    table: Result<ProcessTable, ProbeUnknown>,
) -> DescendantSample {
    let table = match table {
        Ok(table) => table,
        Err(unknown) => {
            return DescendantSample {
                live: Default::default(),
                unknown: Some(unknown),
            };
        }
    };
    let mut live = std::collections::HashSet::new();
    let mut parent_of = std::collections::HashMap::<u32, u32>::new();
    let mut zombies = std::collections::HashSet::<u32>::new();
    let mut helpers = std::collections::HashSet::<u32>::new();
    for row in table.rows {
        if row.zombie {
            zombies.insert(row.pid);
        }
        if is_provider_helper_comm(&row.comm) {
            helpers.insert(row.pid);
        }
        parent_of.insert(row.pid, row.ppid);
    }
    for (&child, _) in &parent_of {
        if zombies.contains(&child) || helpers.contains(&child) {
            continue;
        }
        let mut cursor = child;
        let mut hops = 0;
        while let Some(&parent) = parent_of.get(&cursor) {
            if parent == pid {
                live.insert(child);
                break;
            }
            if parent <= 1 || hops > 64 || helpers.contains(&parent) {
                break;
            }
            cursor = parent;
            hops += 1;
        }
    }
    DescendantSample {
        live,
        unknown: (table.unreadable_rows > 0).then_some(ProbeUnknown::RowUnreadable),
    }
}

/// Sample, re-reading up to `retries` times (with `backoff`) while the sample
/// shows no live descendant but could not rule one out. A definite live
/// descendant, a complete table or an unsupported platform ends it at once.
fn sample_with_retry(
    pid: u32,
    read: &mut dyn FnMut() -> Result<ProcessTable, ProbeUnknown>,
    retries: usize,
    backoff: std::time::Duration,
) -> DescendantSample {
    let mut sample = sample_descendants(pid, read());
    for _ in 0..retries {
        if !sample.live.is_empty()
            || matches!(sample.unknown, None | Some(ProbeUnknown::Unsupported))
        {
            break;
        }
        std::thread::sleep(backoff);
        sample = sample_descendants(pid, read());
    }
    sample
}

/// The settled-descendant probe over an injectable table reader: descendants
/// present in two samples `settle` apart (#1459), failing closed to
/// `Unknown` whenever either sample could not rule out a live child (#3378).
pub(crate) fn probe_children_with(
    pid: u32,
    settle: std::time::Duration,
    retries: usize,
    backoff: std::time::Duration,
    read: &mut dyn FnMut() -> Result<ProcessTable, ProbeUnknown>,
) -> ChildrenOutcome {
    let first = sample_with_retry(pid, read, retries, backoff);
    if first.live.is_empty() {
        return first
            .unknown
            .map_or(ChildrenOutcome::None, ChildrenOutcome::Unknown);
    }
    std::thread::sleep(settle);
    let second = sample_with_retry(pid, read, retries, backoff);
    if second.live.intersection(&first.live).next().is_some() {
        return ChildrenOutcome::Alive;
    }
    // Nothing survived in a readable view, or the second view was incomplete
    // and an unreadable row may be the survivor.
    match second.unknown {
        Some(unknown) => ChildrenOutcome::Unknown(unknown),
        None => ChildrenOutcome::None,
    }
}

/// Descendants of `pid` that are still live after `settle`, or an unknown.
pub(crate) fn settled_descendants(pid: u32, settle: std::time::Duration) -> ChildrenOutcome {
    probe_children_with(
        pid,
        settle,
        UNKNOWN_RETRIES,
        UNKNOWN_RETRY_BACKOFF,
        &mut process_table,
    )
}

/// Live descendants of `pid` in one sample, for tests.
#[cfg(test)]
pub(crate) fn live_descendant_count(pid: u32) -> usize {
    sample_descendants(pid, process_table()).live.len()
}

#[cfg(test)]
fn test_provider_pids() -> &'static std::sync::Mutex<std::collections::HashMap<Uuid, u32>> {
    static PIDS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<Uuid, u32>>> =
        std::sync::OnceLock::new();
    PIDS.get_or_init(Default::default)
}

/// Test seam: stand in a real pid for a scripted provider process.
#[cfg(test)]
pub(crate) fn install_test_provider_pid(session_id: Uuid, pid: u32) {
    test_provider_pids()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(session_id, pid);
}

/// Whether the worker's provider process still has live descendants. Called
/// at the result boundary, before terminal settlement reaps the cohort.
pub(crate) async fn owned_children_alive(
    active: &Arc<
        tokio::sync::RwLock<std::collections::HashMap<Uuid, super::types::TrackedSession>>,
    >,
    session_id: Uuid,
) -> bool {
    let pid = {
        let guard = active.read().await;
        let Some(tracked) = guard.get(&session_id) else {
            return false;
        };
        // Cheap gate: only leaf workers with a parent are ever covered.
        if tracked.session.parent_id.is_none()
            || !rsi_common::is_leaf_kind(tracked.session.session_kind)
        {
            return false;
        }
        let pid = tracked.process.as_ref().and_then(|p| p.os_pid());
        #[cfg(test)]
        let pid = pid.or_else(|| {
            test_provider_pids()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&session_id)
                .copied()
        });
        pid
    };
    let Some(pid) = pid else {
        return false;
    };
    let outcome = tokio::task::spawn_blocking(move || settled_descendants(pid, DESCENDANT_SETTLE))
        .await
        .unwrap_or(ChildrenOutcome::Unknown(ProbeUnknown::ProbeTaskFailed));
    if let ChildrenOutcome::Unknown(unknown) = outcome {
        // #3378: fail closed. The turn is treated as still running, which
        // earns the one bounded continuation; a second miss settles anyway.
        tracing::warn!(
            session_id = %session_id,
            reason = unknown.code(),
            "owned-children probe unknown; treating the worker as having live children"
        );
    }
    outcome.blocks_finalize()
}

/// Decide the verdict for a worker turn about to finalize `Completed`.
pub(crate) async fn worker_no_result_verdict(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    session: &Session,
    assistant_output: &str,
    turn_reported: bool,
    children_at_result: bool,
) -> NoResultVerdict {
    if session.status == SessionStatus::Archived
        || has_result_line(assistant_output)
        || turn_reported
    {
        return NoResultVerdict::NotApplicable;
    }
    let guard = store.lock().await;
    let covered = match guard.worker_no_result_covered(session) {
        Ok(covered) => covered,
        Err(error) => {
            tracing::warn!(session_id = %session.id, %error, "no-result worker check failed");
            return NoResultVerdict::NotApplicable;
        }
    };
    if !covered {
        return NoResultVerdict::NotApplicable;
    }
    // #3218: a reviewer that submitted its receipt has delivered its result.
    if guard
        .worker_submitted_review_receipt(session.id)
        .unwrap_or(false)
    {
        return NoResultVerdict::NotApplicable;
    }
    // #151: the launch brief's declared result format or a published Issue
    // handoff is a final report, even without a `RESULT`/`PIPELINE HANDOFF` line.
    if guard
        .latest_turn_reported_in_declared_form(session.id)
        .unwrap_or(false)
    {
        return NoResultVerdict::NotApplicable;
    }
    // #364: a queued/running `wake: owner` job also parks the worker before
    // settlement creates its wake row, including after a previous nudge.
    if guard
        .owner_running_job_has_completion_wake(session.id)
        .unwrap_or(false)
    {
        return NoResultVerdict::NotApplicable;
    }
    match guard.worker_no_result_nudged(session.id) {
        Ok(true) => return NoResultVerdict::SecondMiss,
        Ok(false) => {}
        Err(error) => {
            tracing::warn!(session_id = %session.id, %error, "no-result nudge lookup failed");
            return NoResultVerdict::NotApplicable;
        }
    }
    // #1958: an armed durable wake the worker owns is the pending result.
    // Check after the nudge marker so the nudge's own wake cannot hide a
    // second miss.
    if guard
        .owner_has_armed_result_wake(session.id)
        .unwrap_or(false)
    {
        return NoResultVerdict::NotApplicable;
    }
    let job_running = guard
        .owner_has_running_agent_job(session.id)
        .unwrap_or(false);
    if children_at_result || job_running {
        NoResultVerdict::FirstMiss
    } else {
        NoResultVerdict::NotApplicable
    }
}

/// Insert the worker's single one-shot Resume wake. Idempotent by id.
pub(crate) async fn schedule_no_result_continuation(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    session: &Session,
) -> crate::error::Result<bool> {
    let delivery = rsi_common::daemon_message::wrap(
        "worker-no-result",
        &format!(
            "{NO_RESULT_MARKER} Your turn ended without a final report while your run is still going. Use AgentSubmitJob with wake:\"none\" for long work, arm one AgentScheduleWake with mode:\"when\" and when.jobs_terminal naming your job ids, then end the turn. If your job is already submitted, arm that wake for the existing job instead of resubmitting it. When the completion wake resumes you, report the result using `RESULT ...` or your required `PIPELINE HANDOFF — <STAGE>:` format."
        ),
    );
    let mut wake = super::harness::tools::schedule_wake::build_agent_scheduled_job(
        super::harness::tools::schedule_wake::ScheduleWakeRequest {
            message: delivery,
            in_seconds: Some(1),
            at: None,
            name: Some(format!("{NO_RESULT_WAKE_NAME_PREFIX}{}", session.id)),
            every_seconds: None,
            mode: Some("resume".to_string()),
            working_dir: session
                .sandbox_root
                .clone()
                .unwrap_or_else(|| session.working_dir.clone()),
            provider: Some(session.provider),
            model: session.model.clone(),
            project_id: session.project_id,
            origin_session_id: Some(session.id),
            watch_session_id: None,
        },
    )
    .map_err(crate::error::DaemonError::InvalidParam)?;
    wake.id = no_result_wake_id(session.id);
    let guard = store.lock().await;
    if guard.worker_no_result_nudged(session.id)? {
        return Ok(false);
    }
    guard.insert_scheduled_job(&wake)?;
    Ok(true)
}

/// Verdict for the active session about to finalize with `status`.
pub(crate) async fn verdict_for_active(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    active: &Arc<
        tokio::sync::RwLock<std::collections::HashMap<Uuid, super::types::TrackedSession>>,
    >,
    session_id: Uuid,
    status: SessionStatus,
    assistant_output: &str,
    children_at_result: bool,
) -> NoResultVerdict {
    if status != SessionStatus::Completed {
        return NoResultVerdict::NotApplicable;
    }
    let session = active.read().await.get(&session_id).map(|tracked| {
        (
            tracked.session.clone(),
            turn_messages_report(&tracked.events),
        )
    });
    match session {
        Some((session, turn_reported)) => {
            worker_no_result_verdict(
                store,
                &session,
                assistant_output,
                turn_reported,
                children_at_result,
            )
            .await
        }
        None => NoResultVerdict::NotApplicable,
    }
}

/// Schedule the single continuation for the just-finalized Completed worker.
pub(crate) async fn schedule_for_completed(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    completed: &Arc<
        tokio::sync::RwLock<std::collections::HashMap<Uuid, super::types::CompletedSession>>,
    >,
    session_id: Uuid,
) {
    let terminal = completed
        .read()
        .await
        .get(&session_id)
        .map(|completed| completed.session.clone());
    let Some(terminal) = terminal else {
        return;
    };
    if terminal.status != SessionStatus::Completed {
        return;
    }
    match schedule_no_result_continuation(store, &terminal).await {
        Ok(true) => tracing::info!(
            session_id = %session_id,
            "Worker ended without RESULT while its run is live; scheduled one continuation"
        ),
        Ok(false) => {}
        Err(error) => tracing::warn!(
            session_id = %session_id,
            %error,
            "Failed to schedule the no-result continuation"
        ),
    }
}

// The monitor future is polled on a 2 MiB worker stack and its debug frame is
// already large. These constructors are plain functions so the (large) future
// is built in their own short-lived frame and only a `Box` reaches the
// monitor's frame (#1098: an inline `Box::pin(async_fn(..))` still built the
// whole future in the monitor's frame and overflowed the stack).
type ActiveMap =
    Arc<tokio::sync::RwLock<std::collections::HashMap<Uuid, super::types::TrackedSession>>>;
type CompletedMap =
    Arc<tokio::sync::RwLock<std::collections::HashMap<Uuid, super::types::CompletedSession>>>;
type BoxFut<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

#[inline(never)]
pub(crate) fn owned_children_alive_boxed(active: &ActiveMap, session_id: Uuid) -> BoxFut<'_, bool> {
    Box::pin(owned_children_alive(active, session_id))
}

#[inline(never)]
pub(crate) fn verdict_for_active_boxed<'a>(
    store: &'a Arc<rsid_core::store_lock::Mutex<Store>>,
    active: &'a ActiveMap,
    session_id: Uuid,
    status: SessionStatus,
    assistant_output: &'a str,
    children_at_result: bool,
) -> BoxFut<'a, NoResultVerdict> {
    Box::pin(verdict_for_active(
        store,
        active,
        session_id,
        status,
        assistant_output,
        children_at_result,
    ))
}

#[inline(never)]
pub(crate) fn schedule_for_completed_boxed<'a>(
    store: &'a Arc<rsid_core::store_lock::Mutex<Store>>,
    completed: &'a CompletedMap,
    session_id: Uuid,
) -> BoxFut<'a, ()> {
    Box::pin(schedule_for_completed(store, completed, session_id))
}
