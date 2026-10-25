//! #286: a launchd job stop ends every process the job started.
//!
//! `launchctl bootout` (with `AbandonProcessGroup=false`) kills only the
//! service's process group. A job child that started its own session or group
//! (`scripts/scoped-test` runs cargo and its slot holder with
//! `start_new_session`) survives it, is reparented to launchd and keeps using
//! CPU and writing job paths after the job reads as stopped (#284).
//!
//! The job's processes are found three ways, so neither reparenting nor a new
//! session hides one:
//! - every process whose environment carries the job's marker
//!   ([`MARKER_ENV`]`=rsi-job-<id>`, set only in this job's plist, inherited
//!   by everything it starts unless a child clears its environment);
//! - the service's own process and its descendants, read before bootout
//!   while the parent links still hold;
//! - every process in a session or process group led by a process already
//!   proved to be the job's (a new-session child's own children keep its
//!   session id after it dies).
//!
//! Only processes of the daemon's own uid are read or signalled, never the
//! daemon or its ancestors. Each signal re-checks the process's start time, so
//! a reused pid is never signalled. The ended set is confirmed empty by a
//! fresh read of the process table.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

/// The environment name of the per-job marker in the launchd plist.
pub(super) const MARKER_ENV: &str = "RSI_JOB_UNIT";

/// SIGTERM, then this long before SIGKILL.
pub(super) const TERM_GRACE: Duration = Duration::from_secs(5);
/// After SIGKILL, this long for the processes to disappear.
pub(super) const KILL_GRACE: Duration = Duration::from_secs(5);
/// Pause between process-table reads.
pub(super) const POLL: Duration = Duration::from_millis(100);

/// One process of the daemon's uid, as the sweep needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Row {
    pub(super) pid: i32,
    pub(super) ppid: i32,
    pub(super) pgid: i32,
    pub(super) sid: i32,
    /// Start time (microseconds): with the pid it names one process.
    pub(super) started: u64,
    pub(super) zombie: bool,
    /// The environment carries this job's marker.
    pub(super) marked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Signal {
    Term,
    Kill,
}

/// The processes proved to be the job's, kept across reads: a process seen
/// once stays the job's for as long as the same incarnation lives.
#[derive(Debug, Default)]
pub(super) struct Custody {
    seen: HashMap<i32, u64>,
}

impl Custody {
    /// Add the job's processes in `rows` to custody (the service's `root`
    /// process, marked processes, processes already seen, members of a
    /// session or group a seen process leads, and every descendant of those)
    /// and return the live ones (zombies are already dead). `own_pid` and its
    /// ancestors are never the job's.
    pub(super) fn absorb(
        &mut self,
        rows: &[Row],
        root: Option<i32>,
        own_pid: i32,
    ) -> Vec<(i32, u64)> {
        let by_pid: HashMap<i32, &Row> = rows.iter().map(|row| (row.pid, row)).collect();
        let mut protected = HashSet::from([0, 1, own_pid]);
        let mut cursor = own_pid;
        while let Some(row) = by_pid.get(&cursor) {
            if !protected.insert(row.ppid) || row.ppid <= 1 {
                break;
            }
            cursor = row.ppid;
        }
        // A session or group id names a seen leader only while no other
        // incarnation of that pid is alive.
        let led_by_seen = |id: i32| {
            self.seen.get(&id).is_some_and(|started| {
                by_pid
                    .get(&id)
                    .is_none_or(|leader| leader.started == *started)
            })
        };
        let mut members: HashSet<i32> = rows
            .iter()
            .filter(|row| !protected.contains(&row.pid))
            .filter(|row| {
                root == Some(row.pid)
                    || row.marked
                    || self.seen.get(&row.pid) == Some(&row.started)
                    || led_by_seen(row.sid)
                    || led_by_seen(row.pgid)
            })
            .map(|row| row.pid)
            .collect();
        let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
        for row in rows {
            children.entry(row.ppid).or_default().push(row.pid);
        }
        let mut queue: Vec<i32> = members.iter().copied().collect();
        while let Some(parent) = queue.pop() {
            for &child in children.get(&parent).into_iter().flatten() {
                if !protected.contains(&child) && members.insert(child) {
                    queue.push(child);
                }
            }
        }
        let mut live = Vec::new();
        for pid in members {
            let row = by_pid[&pid];
            self.seen.insert(pid, row.started);
            if !row.zombie {
                live.push((pid, row.started));
            }
        }
        live.sort_unstable();
        live
    }
}

/// End the job's live processes: SIGTERM each, SIGKILL whatever is left after
/// `term_grace`, and return how many were signalled once a fresh read shows
/// none. `read` reads the process table, `signal` signals one process (pid,
/// start time).
///
/// # Errors
/// A table that cannot be read, or job processes still alive `kill_grace`
/// after SIGKILL (named by pid).
pub(super) fn end_job_processes(
    custody: &mut Custody,
    own_pid: i32,
    read: &mut dyn FnMut() -> Result<Vec<Row>, String>,
    signal: &mut dyn FnMut(i32, u64, Signal),
    term_grace: Duration,
    kill_grace: Duration,
    poll: Duration,
) -> Result<usize, String> {
    let start = Instant::now();
    let mut termed = HashSet::new();
    let mut signalled = HashSet::new();
    loop {
        let live = custody.absorb(&read()?, None, own_pid);
        if live.is_empty() {
            return Ok(signalled.len());
        }
        let elapsed = start.elapsed();
        if elapsed >= term_grace + kill_grace {
            let pids: Vec<i32> = live.iter().map(|(pid, _)| *pid).collect();
            return Err(format!(
                "{} process(es) the job started are still alive after SIGKILL: pids {pids:?}",
                pids.len()
            ));
        }
        for (pid, started) in live {
            signalled.insert(pid);
            if elapsed >= term_grace {
                signal(pid, started, Signal::Kill);
            } else if termed.insert(pid) {
                signal(pid, started, Signal::Term);
            }
        }
        std::thread::sleep(poll);
    }
}

/// `MARKER_ENV=<label>` as it appears in an environment record.
pub(super) fn marker(label: &str) -> Vec<u8> {
    format!("{MARKER_ENV}={label}").into_bytes()
}

/// Whether a NUL-separated environment record holds exactly `marker`.
pub(super) fn environment_marked(environment: &[u8], marker: &[u8]) -> bool {
    environment
        .split(|byte| *byte == 0)
        .any(|entry| entry == marker)
}

/// The largest argument record read for the marker; larger is unmarked (the
/// descendant and session links still cover such a process).
#[cfg(target_os = "macos")]
const ENV_MAX_BYTES: usize = 2 * 1024 * 1024;

/// Every live process of the daemon's effective uid. The pid list is the
/// saturation-checked listing (hub #3398) and each process is first read with
/// the short BSD info, which another uid's process does not refuse (#251), so
/// other users' processes are skipped without failing the read.
///
/// # Errors
/// The pid list could not be read completely.
#[cfg(target_os = "macos")]
pub(super) fn read_table(marker: &[u8]) -> Result<Vec<Row>, String> {
    use crate::session::worker_result_guard::{list_all_pids_with, read_bsd_short_info};
    use nix::libc;
    use rsid_core::libproc_macos::{
        exited, procargs_environment, process_arguments, process_start,
    };
    // SAFETY: a null buffer of size 0 only asks for the current pid count.
    let estimate = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    let estimate =
        usize::try_from(estimate).map_err(|_| "cannot count the host's processes".to_string())?;
    let pids = list_all_pids_with(estimate, |pids| {
        let bytes = libc::c_int::try_from(std::mem::size_of_val(pids)).ok()?;
        // SAFETY: the buffer holds `bytes` bytes and outlives the call.
        let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
        usize::try_from(count).ok()
    })
    .map_err(|unknown| format!("cannot list the host's processes: {unknown:?}"))?;
    let uid = nix::unistd::Uid::effective().as_raw();
    let mut rows = Vec::new();
    let mut unreadable = 0usize;
    for pid in pids.into_iter().filter(|&pid| pid > 1) {
        let info = match read_bsd_short_info(pid) {
            Ok(info) => info,
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => continue,
            Err(_) => {
                unreadable += 1;
                continue;
            }
        };
        if info.pbsi_uid != uid {
            continue;
        }
        let start = match process_start(pid) {
            Ok(start) => start,
            Err(code) if exited(code) => continue,
            Err(_) => {
                unreadable += 1;
                continue;
            }
        };
        // SAFETY: getsid reads one process's session id.
        let sid = unsafe { libc::getsid(pid) };
        let marked = !start.zombie
            && process_arguments(pid, ENV_MAX_BYTES)
                .ok()
                .and_then(|record| procargs_environment(&record))
                .is_some_and(|environment| environment_marked(&environment, marker));
        rows.push(Row {
            pid,
            ppid: i32::try_from(info.pbsi_ppid).unwrap_or(0),
            pgid: i32::try_from(info.pbsi_pgid).unwrap_or(0),
            sid,
            started: start.started_micros,
            zombie: start.zombie,
            marked,
        });
    }
    if unreadable > 0 {
        tracing::debug!(
            unreadable,
            "launchd job sweep could not read some processes"
        );
    }
    Ok(rows)
}

/// No process table here: the launchd runtime only runs on macOS.
#[cfg(not(target_os = "macos"))]
pub(super) fn read_table(_marker: &[u8]) -> Result<Vec<Row>, String> {
    Err(rsi_common::agent_jobs::JOB_PLATFORM_UNSUPPORTED.into())
}

/// Signal one process, only when it is still the incarnation the sweep read.
pub(super) fn signal_process(label: &str, pid: i32, started: u64, signal: Signal) {
    use nix::sys::signal::{Signal as Nix, kill};
    #[cfg(target_os = "macos")]
    match rsid_core::libproc_macos::process_start(pid) {
        Ok(now) if now.started_micros == started && !now.zombie => {}
        _ => return,
    }
    #[cfg(not(target_os = "macos"))]
    let _ = started;
    let (sig, name) = match signal {
        Signal::Term => (Nix::SIGTERM, "SIGTERM"),
        Signal::Kill => (Nix::SIGKILL, "SIGKILL"),
    };
    let outcome = match kill(nix::unistd::Pid::from_raw(pid), sig) {
        Ok(()) => "sent".to_string(),
        Err(error) => error.to_string(),
    };
    let target = rsid_core::process_control::SignalTarget {
        unit: Some(label.to_string()),
        comm: None,
    };
    rsid_core::process_control::log_signal(
        &rsid_core::process_control::SignalRecord {
            pid,
            pgid: None,
            signal: name,
            reason: "launchd job stop: a process the job started outside its process group (#286)",
            target: &target,
        },
        std::panic::Location::caller(),
        &outcome,
    );
}

/// [`end_job_processes`] against the live process table for job `label`.
///
/// # Errors
/// As [`end_job_processes`].
pub(super) fn end_live_job_processes(label: &str, custody: &mut Custody) -> Result<usize, String> {
    let marker = marker(label);
    end_job_processes(
        custody,
        std::process::id() as i32,
        &mut || read_table(&marker),
        &mut |pid, started, signal| signal_process(label, pid, started, signal),
        TERM_GRACE,
        KILL_GRACE,
        POLL,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pid: i32, ppid: i32, pgid: i32, sid: i32) -> Row {
        Row {
            pid,
            ppid,
            pgid,
            sid,
            started: 1000 + pid as u64,
            zombie: false,
            marked: false,
        }
    }

    fn pids(live: &[(i32, u64)]) -> Vec<i32> {
        live.iter().map(|(pid, _)| *pid).collect()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn custody_follows_new_sessions_reparenting_and_the_marker_but_never_the_daemon() {
        // launchd (1) -> daemon (50); launchd -> service root (100, its own
        // session) -> scoped-test (101) -> cargo in a new session (200) ->
        // rustc (201); an unrelated process (300) of the same uid.
        let mut rows = vec![
            row(50, 1, 50, 50),
            row(100, 1, 100, 100),
            row(101, 100, 100, 100),
            row(200, 101, 200, 200),
            row(201, 200, 200, 200),
            row(300, 1, 300, 300),
        ];
        let mut custody = Custody::default();
        assert_eq!(
            pids(&custody.absorb(&rows, Some(100), 50)),
            [100, 101, 200, 201]
        );
        // Bootout killed the service group; cargo is reparented to launchd and
        // then dies, leaving rustc with only its session id and no parent link.
        rows.retain(|row| ![100, 101, 200].contains(&row.pid));
        rows.iter_mut().find(|row| row.pid == 201).unwrap().ppid = 1;
        // A process started by the job with its environment cleared and its
        // own session, whose parent link is gone too, carries only the marker.
        rows.push(Row {
            marked: true,
            ..row(400, 1, 400, 400)
        });
        rows.push(row(401, 400, 400, 400));
        assert_eq!(pids(&custody.absorb(&rows, None, 50)), [201, 400, 401]);
        // The daemon is never the job's, even when a marked process claims it
        // as a parent or it carries the marker itself.
        rows.iter_mut().find(|row| row.pid == 50).unwrap().marked = true;
        assert_eq!(pids(&custody.absorb(&rows, None, 50)), [201, 400, 401]);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn custody_never_takes_a_reused_pid_or_the_group_a_new_incarnation_leads() {
        let mut custody = Custody::default();
        let first = vec![row(100, 1, 100, 100), row(200, 100, 200, 200)];
        assert_eq!(pids(&custody.absorb(&first, Some(100), 50)), [100, 200]);
        // Pid 200 now names another process (another start time) that leads
        // its own session with a child: neither is the job's.
        let reused = vec![
            Row {
                started: 9999,
                ..row(200, 1, 200, 200)
            },
            row(201, 200, 200, 200),
        ];
        assert!(custody.absorb(&reused, None, 50).is_empty());
        // A zombie is the job's but already dead.
        let zombie = vec![Row {
            zombie: true,
            ..row(100, 1, 100, 100)
        }];
        assert!(custody.absorb(&zombie, None, 50).is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn ending_job_processes_terms_then_kills_and_confirms_none_remain() {
        // 200 exits on SIGTERM; 201 ignores it and only SIGKILL ends it.
        let alive = std::cell::RefCell::new(vec![
            Row {
                marked: true,
                ..row(200, 1, 200, 200)
            },
            row(201, 200, 200, 200),
        ]);
        let mut sent = Vec::new();
        let mut custody = Custody::default();
        let ended = end_job_processes(
            &mut custody,
            50,
            &mut || Ok(alive.borrow().clone()),
            &mut |pid, _, signal| {
                sent.push((pid, signal));
                if signal == Signal::Kill || pid == 200 {
                    alive.borrow_mut().retain(|row| row.pid != pid);
                }
            },
            Duration::from_millis(30),
            Duration::from_secs(5),
            Duration::from_millis(5),
        )
        .unwrap();
        assert_eq!(ended, 2);
        assert_eq!(sent.iter().filter(|(_, s)| *s == Signal::Term).count(), 2);
        assert!(sent.contains(&(201, Signal::Kill)));
        assert!(!sent.contains(&(200, Signal::Kill)));

        // A process no signal ends is named, never reported stopped.
        let mut custody = Custody::default();
        let stuck = vec![Row {
            marked: true,
            ..row(300, 1, 300, 300)
        }];
        let error = end_job_processes(
            &mut custody,
            50,
            &mut || Ok(stuck.clone()),
            &mut |_, _, _| {},
            Duration::from_millis(5),
            Duration::from_millis(20),
            Duration::from_millis(5),
        )
        .unwrap_err();
        assert!(error.contains("[300]"), "{error}");
        // An unreadable table is an error, never "nothing left".
        assert!(
            end_job_processes(
                &mut Custody::default(),
                50,
                &mut || Err("no table".into()),
                &mut |_, _, _| {},
                Duration::ZERO,
                Duration::ZERO,
                Duration::ZERO,
            )
            .is_err()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_marker_matches_only_this_jobs_exact_entry() {
        let marker = marker("rsi-job-a");
        assert!(environment_marked(
            b"HOME=/h\0RSI_JOB_UNIT=rsi-job-a\0",
            &marker
        ));
        assert!(!environment_marked(b"RSI_JOB_UNIT=rsi-job-ab\0", &marker));
        assert!(!environment_marked(b"XRSI_JOB_UNIT=rsi-job-a\0", &marker));
    }
}
