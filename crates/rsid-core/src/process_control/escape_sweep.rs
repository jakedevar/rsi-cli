//! Termination of descendants that left their process group (#3900).
//!
//! `ProcessContainment::Group` lets a descendant call `setsid`/`setpgid` (Docker's `ssh://`
//! connection helper does, #3890), so signalling the leader's group no longer reaches it. An
//! [`EscapeSweep`] closes that gap on Linux without a cgroup (which needs delegation the daemon
//! may not have): it tags the command's environment with a unique marker and makes the leader a
//! child subreaper, and [`EscapeSweep::sweep`] SIGKILLs every live process that either carries
//! the marker or descends from the leader. Elsewhere it is a no-op and the group kill remains the
//! only containment.
//!
//! Limits: a descendant that both clears its environment and has its parent die after the leader
//! is already gone is not found. The sweep is a best-effort `/proc` scan, not a kernel guarantee.

use std::process::Command;

/// Environment variable that carries the per-run marker into every descendant.
pub const ESCAPE_SWEEP_ENV: &str = "RSI_ESCAPE_SWEEP_ID";

/// One armed command's escape sweep. Cheap to hold; scans only when [`sweep`](Self::sweep) runs.
#[derive(Debug)]
pub struct EscapeSweep {
    #[cfg(target_os = "linux")]
    marker: String,
}

impl EscapeSweep {
    /// Tag `command` so its descendants can be found later, and (Linux) make the spawned leader a
    /// child subreaper so a descendant orphaned while the leader lives stays below it.
    pub fn arm(command: &mut Command) -> Self {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::process::CommandExt;
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos());
            let marker = format!(
                "{}-{}-{nanos}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            );
            command.env(ESCAPE_SWEEP_ENV, &marker);
            // SAFETY: the callback makes one async-signal-safe `prctl` syscall after fork.
            unsafe {
                command.pre_exec(|| {
                    // Best effort: a kernel or sandbox that refuses it still has the marker scan.
                    nix::libc::prctl(nix::libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0);
                    Ok(())
                });
            }
            Self { marker }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = command;
            Self {}
        }
    }

    /// SIGKILL every live process that carries this run's marker or descends from `leader`
    /// (pass `None` once the leader is reaped, because its numeric id may name something else).
    /// Repeats until a scan finds nothing alive, so a process forked during the sweep is caught.
    pub fn sweep(&self, leader: Option<nix::unistd::Pid>) {
        #[cfg(target_os = "linux")]
        {
            let me = std::process::id();
            for _ in 0..64 {
                let targets = linux::live_targets(&self.marker, leader, me);
                if targets.is_empty() {
                    return;
                }
                for pid in targets {
                    // SAFETY: plain `kill(2)` of a pid just observed carrying our marker or
                    // descending from our child.
                    unsafe {
                        nix::libc::kill(pid as nix::libc::pid_t, nix::libc::SIGKILL);
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = leader;
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::{HashMap, HashSet};

    /// `(state, ppid)` from `/proc/<pid>/stat`; the command name may hold spaces and parentheses,
    /// so parse after the last `)`.
    fn state_and_parent(pid: u32) -> Option<(u8, u32)> {
        let stat = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
        let close = stat.iter().rposition(|byte| *byte == b')')?;
        let mut fields = stat.get(close + 1..)?.split(|byte| *byte == b' ').skip(1);
        let state = *fields.next()?.first()?;
        let ppid = std::str::from_utf8(fields.next()?).ok()?.parse().ok()?;
        Some((state, ppid))
    }

    fn carries_marker(pid: u32, marker: &str) -> bool {
        let Ok(environ) = std::fs::read(format!("/proc/{pid}/environ")) else {
            return false;
        };
        let needle = format!("{}={marker}", super::ESCAPE_SWEEP_ENV);
        environ
            .split(|byte| *byte == 0)
            .any(|entry| entry == needle.as_bytes())
    }

    /// Live (non-zombie) pids to kill: marker carriers plus the leader's descendant tree.
    pub(super) fn live_targets(
        marker: &str,
        leader: Option<nix::unistd::Pid>,
        me: u32,
    ) -> Vec<u32> {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        let mut state = HashMap::new();
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            if let Some((letter, parent)) = state_and_parent(pid) {
                state.insert(pid, letter);
                children.entry(parent).or_default().push(pid);
            }
        }
        let mut found = HashSet::new();
        if let Some(root) = leader.map(|pid| pid.as_raw() as u32) {
            let mut stack = vec![root];
            while let Some(pid) = stack.pop() {
                for child in children.get(&pid).into_iter().flatten() {
                    if found.insert(*child) {
                        stack.push(*child);
                    }
                }
            }
            if state.contains_key(&root) {
                found.insert(root);
            }
        }
        for pid in state.keys() {
            if !found.contains(pid) && carries_marker(*pid, marker) {
                found.insert(*pid);
            }
        }
        found.remove(&me);
        found
            .into_iter()
            .filter(|pid| state.get(pid).is_some_and(|letter| *letter != b'Z'))
            .collect()
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::process::Stdio;

    fn alive(pid: u32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .map(|stat| {
                !stat
                    .rsplit(')')
                    .next()
                    .unwrap_or("")
                    .trim_start()
                    .starts_with('Z')
            })
            .unwrap_or(false)
    }

    #[test]
    fn sweep_kills_a_setsid_descendant_that_left_the_group() {
        let dir = tempfile::tempdir().expect("pid file directory");
        let pid_file = dir.path().join("escaped.pid");
        let mut command = Command::new("sh");
        command
            .env("PID_FILE", &pid_file)
            .args([
                "-c",
                "setsid sh -c 'echo $$ > \"$PID_FILE\"; exec sleep 30' & wait",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let sweep = EscapeSweep::arm(&mut command);
        let mut leader = command.spawn().expect("spawn");
        let escaped: u32 = (0..200)
            .find_map(|_| {
                std::thread::sleep(std::time::Duration::from_millis(25));
                std::fs::read_to_string(&pid_file).ok()?.trim().parse().ok()
            })
            .expect("escaped child published its pid");
        assert!(alive(escaped));
        sweep.sweep(Some(nix::unistd::Pid::from_raw(leader.id() as i32)));
        let _ = leader.wait();
        let dead = (0..200).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(25));
            !alive(escaped)
        });
        assert!(dead, "setsid descendant {escaped} survived the sweep");
    }
}
