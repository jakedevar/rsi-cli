//! A5 hermetic injection test for the surgical resume-orphan reaper
//! (`rsid::session::reap_orphans_for_session`, Change 2). No daemon, no provider
//! CLI, no DB — just real OS processes and `/proc`.
//!
//! Positive: a real live process stamped with THIS session's `RSI_SESSION_ID` is
//! SIGKILLed by the reap, so a resume can never end up with two live processes
//! for the id. Negatives (surgical-match / daemon-safety): a process stamped
//! with a DIFFERENT id, and a process with NO stamp, are provably left alive.
//!
//! Stand-in note: the ticket sketches `sh -c "trap '' INT TERM; while :; do
//! sleep 1; done"`. Because `reap_orphans_for_session` escalates directly to
//! SIGKILL — which is uncatchable, so whether the target ignores INT/TERM is
//! irrelevant to the reap — a single long-lived `sleep` is an equivalent but
//! *race-free* stand-in. The `sh … sleep 1` loop would additionally fork a
//! second process that inherits the same `RSI_SESSION_ID` stamp (the `sleep`
//! grandchild), making the exact `reaped == 1` count racy. A lone stamped
//! process yields a deterministic count.

#![cfg(target_os = "linux")]

use nix::errno::Errno;
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use rsi_common::identity::{
    ENV_PROCESS_OWNERSHIP_NAMESPACE, ENV_SESSION_ID, process_ownership_namespace,
};
use std::time::Duration;
use uuid::Uuid;

/// RAII safety net: SIGKILL the pid on drop so a failed/panicking test can never
/// leak the stand-in. Best-effort — ESRCH (already reaped) is ignored.
struct KillOnDrop(i32);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = signal::kill(Pid::from_raw(self.0), Signal::SIGKILL);
    }
}

/// signal-0 existence probe: `Ok` while the pid table entry exists (including a
/// not-yet-reaped zombie), `Err(ESRCH)` once it is gone.
fn alive(pid: i32) -> bool {
    signal::kill(Pid::from_raw(pid), None::<Signal>).is_ok()
}

/// Spawn a single long-lived process. `sid_value == Some(v)` stamps
/// `RSI_SESSION_ID=v`; `None` explicitly clears any inherited stamp.
fn spawn_stand_in(sid_value: Option<&str>) -> tokio::process::Child {
    let mut cmd = tokio::process::Command::new("sleep");
    cmd.arg("2147483647"); // ~68 years; a single process, no children, no shell
    match sid_value {
        Some(v) => {
            cmd.env(ENV_SESSION_ID, v).env(
                ENV_PROCESS_OWNERSHIP_NAMESPACE,
                process_ownership_namespace(),
            );
        }
        None => {
            cmd.env_remove(ENV_SESSION_ID);
        }
    }
    cmd.spawn().expect("spawn sleep stand-in")
}

/// Drive the (blocking `/proc`-walking) reaper the same way `continue_session`
/// does — on a blocking thread.
async fn reap(sid: Uuid) -> rsid::error::Result<usize> {
    tokio::task::spawn_blocking(move || rsid::session::reap_orphans_for_session(sid))
        .await
        .expect("reap task joins")
}

/// The core proof: an orphan stamped with THIS session id is SIGKILLed by the
/// resume-time reap, so it can never become a second live process.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reaps_live_orphan_stamped_with_this_session() {
    let sid = Uuid::new_v4();
    let mut child = spawn_stand_in(Some(&sid.to_string()));
    let pid = child.id().expect("child pid") as i32;
    let _guard = KillOnDrop(pid);

    assert!(alive(pid), "stand-in must be alive before reap");

    let reaped = reap(sid).await.expect("runtime orphan proof succeeds");
    assert_eq!(reaped, 1, "exactly one exact-env-match orphan reaped");

    // We are the parent — reap the zombie so the existence probe can reach ESRCH.
    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;

    let mut gone = false;
    for _ in 0..200 {
        if signal::kill(Pid::from_raw(pid), None::<Signal>) == Err(Errno::ESRCH) {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        gone,
        "orphan pid must be ESRCH (dead + reaped) after the reap"
    );
}

/// Surgical-match negative: a process stamped with a DIFFERENT session id is
/// never touched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn does_not_reap_different_session_id() {
    let other = Uuid::new_v4();
    let target = Uuid::new_v4();
    let mut child = spawn_stand_in(Some(&other.to_string()));
    let pid = child.id().expect("child pid") as i32;
    let _guard = KillOnDrop(pid);

    assert!(alive(pid));
    assert_eq!(
        reap(target).await.expect("runtime orphan proof succeeds"),
        0,
        "a different RSI_SESSION_ID must never be reaped"
    );
    assert!(alive(pid), "mismatched-id process must remain alive");

    let _ = child.kill().await;
    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
}

/// Daemon-safety negative: a process with NO `RSI_SESSION_ID` stamp (like the
/// daemon itself) is never touched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn does_not_reap_unstamped_process() {
    let target = Uuid::new_v4();
    let mut child = spawn_stand_in(None);
    let pid = child.id().expect("child pid") as i32;
    let _guard = KillOnDrop(pid);

    assert!(alive(pid));
    assert_eq!(
        reap(target).await.expect("runtime orphan proof succeeds"),
        0,
        "an unstamped process must never be reaped (daemon-safety)"
    );
    assert!(alive(pid), "unstamped process must remain alive");

    let _ = child.kill().await;
    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
}
