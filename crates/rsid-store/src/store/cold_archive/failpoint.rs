//! Crash injection for the cold-archive tests (#2384).
//!
//! A test arms one named point on its thread; the next time the pass reaches it
//! the pass returns an error and stops, leaving every durable effect already
//! made exactly as a process death at that point would. The next pass must pick
//! up from there. Release builds compile `hit` to a no-op.

use super::ColdResult;

#[cfg(any(test, feature = "test-seam"))]
mod armed {
    use std::cell::Cell;

    thread_local! {
        static ARMED: Cell<Option<&'static str>> = const { Cell::new(None) };
    }

    pub fn arm(point: &'static str) {
        ARMED.with(|cell| cell.set(Some(point)));
    }

    pub fn disarm() {
        ARMED.with(|cell| cell.set(None));
    }

    thread_local! {
        static FAILING_SESSION: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
    }

    /// Every move of `session_id` on this thread fails (until
    /// [`clear_session_failure`]), as a session whose delete always errors would.
    pub fn fail_session(session_id: &str) {
        FAILING_SESSION.with(|cell| *cell.borrow_mut() = Some(session_id.to_string()));
    }

    pub fn clear_session_failure() {
        FAILING_SESSION.with(|cell| *cell.borrow_mut() = None);
    }

    pub fn session_fails(session_id: &str) -> bool {
        FAILING_SESSION.with(|cell| cell.borrow().as_deref() == Some(session_id))
    }

    pub fn take_if(point: &str) -> bool {
        ARMED.with(|cell| {
            if cell.get() == Some(point) {
                cell.set(None);
                true
            } else {
                false
            }
        })
    }
}

#[cfg(any(test, feature = "test-seam"))]
pub use armed::{arm, clear_session_failure, disarm, fail_session};

/// The points a pass can be killed at, in order: `after_stage` (P2 committed,
/// nothing written), `write_before_commit` (inside the archive transaction),
/// `after_write` (P4 committed, not yet verified), `after_verify` (P5 passed,
/// the index not yet told).
pub const POINTS: [&str; 4] = [
    "after_stage",
    "write_before_commit",
    "after_write",
    "after_verify",
];

/// The move transaction's last step before its commit (P6) is the point
/// `move_before_commit`; it is not in [`POINTS`] (a stage pass never reaches it).
///
/// Fails with `cold_archive_injected_failure` when `point` is armed.
pub fn hit(point: &'static str) -> ColdResult<()> {
    #[cfg(any(test, feature = "test-seam"))]
    if armed::take_if(point) {
        return Err(super::ColdError::new(
            "cold_archive_injected_failure",
            point,
        ));
    }
    let _ = point;
    Ok(())
}

/// Fails the move of `session_id` with `cold_move_injected` while a test marked
/// it failing ([`fail_session`]). Unlike [`hit`] this is an ordinary session
/// failure, not a process death: the pass isolates it and carries on.
pub fn hit_session(session_id: &str) -> ColdResult<()> {
    #[cfg(any(test, feature = "test-seam"))]
    if armed::session_fails(session_id) {
        return Err(super::ColdError::new(
            "cold_move_injected",
            format!("injected move failure of {session_id}"),
        ));
    }
    let _ = session_id;
    Ok(())
}
