//! Store commit order of stamped session-metadata writes (#265, hub #2818).
//!
//! The daemon mirrors each such write into its runtime copy of the session
//! after the write returns, so mirrors can arrive in any order. A wall clock
//! can step back and two writes can read the same instant, so the stamps do
//! not give commit order either. Each write is therefore given a `write_seq`
//! inside its immediate transaction, after the Store has taken the row's write
//! lock and before the commit: a larger `write_seq` committed later.
//!
//! The same step records which unbroken run of stamped writes the write ends
//! (each write in a run replaced exactly the value the previous one stored, so
//! no other Store write moved the row's `updated_at` between them). A runtime
//! copy uses the run to advance its `updated_at` without first seeing every
//! earlier write of the run, while an unstamped Store change (an archive, a
//! status write) still starts a new run and stays visible as drift.
//!
//! Each session also keeps the `write_seq` of the last stamped write of each
//! field, so a session published into a runtime map takes from its row exactly
//! the fields a stamped write changed after the runtime copy last saw them,
//! and never an unstamped change.
//!
//! In-process only: runtime copies are rebuilt from the Store on restart, so
//! the order never has to survive one, and nothing is persisted.
//!
//! Lifecycle (#2876): a session's record lives as long as its row can be
//! published into a runtime map, so the registry follows the Store's session
//! set, not the lifetime churn of the process. Archive and soft delete keep
//! the row (it can be unarchived or undeleted, and a publication in flight may
//! still need the record to see a write whose mirror found no runtime entry),
//! so they keep the record. [`retire_session_writes`] drops it when the row is
//! hard-deleted (`purge_session`): the id is never reused, a publication finds
//! no row to reconcile, a delayed mirror carries its own stamp and applies to
//! no entry, and a publication that read the record before the purge fails
//! `is_current` closed and reads again. `write_seq` stays globally monotonic,
//! so a record recreated later can never repeat a retired sequence number.

use super::sessions::SessionWriteStamp;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use uuid::Uuid;

/// The session field one stamped metadata write sets: every stamped writer
/// sets exactly one (#263, #265).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionWriteField {
    Title,
    Description,
    Rating,
    Project,
    Workflow,
    ActiveTask,
    Label,
    Parent,
    Pin,
}

impl SessionWriteField {
    pub const ALL: [Self; 9] = [
        Self::Title,
        Self::Description,
        Self::Rating,
        Self::Project,
        Self::Workflow,
        Self::ActiveTask,
        Self::Label,
        Self::Parent,
        Self::Pin,
    ];
    pub const COUNT: usize = Self::ALL.len();
}

/// The stamped writes of one session committed in this process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionWrites {
    /// The last one.
    pub last: SessionWriteStamp,
    /// The `write_seq` of the last write of each field, 0 for none.
    fields: [u64; SessionWriteField::COUNT],
}

impl SessionWrites {
    /// The `write_seq` of the last stamped write of `field`, 0 for none.
    pub fn field(&self, field: SessionWriteField) -> u64 {
        self.fields[field as usize]
    }
}

/// The next `write_seq`; 0 means "no stamped write".
static NEXT_WRITE_SEQ: AtomicU64 = AtomicU64::new(1);

/// The committed stamped writes of each session in this process.
fn session_writes_by_id() -> &'static Mutex<HashMap<Uuid, SessionWrites>> {
    static WRITES: OnceLock<Mutex<HashMap<Uuid, SessionWrites>>> = OnceLock::new();
    WRITES.get_or_init(Default::default)
}

fn lock_session_writes() -> MutexGuard<'static, HashMap<Uuid, SessionWrites>> {
    session_writes_by_id()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// One stamped write ordered inside its transaction and not yet committed;
/// dropping it (a failed commit) records nothing.
pub(super) struct OrderedSessionWrite {
    session_id: Uuid,
    field: SessionWriteField,
    stamp: SessionWriteStamp,
}

impl OrderedSessionWrite {
    /// Order a write of `field` of `session_id` that replaced
    /// `previous_updated_at` and stores `stamped_at`. Call inside the write's
    /// immediate transaction, after its UPDATE and before its commit: the
    /// transaction holds the row's write lock, so `write_seq` assignment order
    /// is commit order.
    pub(super) fn begin(
        session_id: Uuid,
        field: SessionWriteField,
        previous_updated_at: Option<DateTime<Utc>>,
        stamped_at: DateTime<Utc>,
    ) -> Self {
        let writes = lock_session_writes();
        let write_seq = NEXT_WRITE_SEQ.fetch_add(1, Ordering::Relaxed);
        let (run_seq, run_base) = match writes.get(&session_id) {
            Some(writes) if previous_updated_at == Some(writes.last.stamped_at) => {
                (writes.last.run_seq, writes.last.run_base)
            }
            _ => (write_seq, previous_updated_at),
        };
        Self {
            session_id,
            field,
            stamp: SessionWriteStamp {
                previous_updated_at,
                stamped_at,
                write_seq,
                run_seq,
                run_base,
            },
        }
    }

    /// The write committed: record it. The caller still holds the Store, so
    /// one Store's records follow its commits; a record never replaces a
    /// newer one, so writes through two Store handles can at worst start a
    /// new run (no advance, fail closed).
    pub(super) fn committed(self) -> SessionWriteStamp {
        let mut writes = lock_session_writes();
        let recorded = writes.entry(self.session_id).or_insert(SessionWrites {
            last: self.stamp,
            fields: [0; SessionWriteField::COUNT],
        });
        if recorded.last.write_seq < self.stamp.write_seq {
            recorded.last = self.stamp;
        }
        let field = &mut recorded.fields[self.field as usize];
        *field = (*field).max(self.stamp.write_seq);
        self.stamp
    }
}

/// Drop the record of `session_id` (its row was hard-deleted). Returns whether
/// a record existed. See the module docs for why this is safe against delayed
/// mirrors and in-flight publications.
pub(super) fn retire_session_writes(session_id: Uuid) -> bool {
    lock_session_writes().remove(&session_id).is_some()
}

/// The committed stamped writes of `session_id` in this process. Read it
/// under the Store lock together with the row to know which writes the row
/// reflects: every stamped write up to `last` and none after it.
pub fn session_writes(session_id: Uuid) -> Option<SessionWrites> {
    lock_session_writes().get(&session_id).copied()
}

/// The last committed stamped write of `session_id` in this process.
pub fn last_session_write(session_id: Uuid) -> Option<SessionWriteStamp> {
    session_writes(session_id).map(|writes| writes.last)
}

/// The `write_seq` of [`last_session_write`], 0 when none. Needs no Store
/// lock: a publisher compares it with the one it read to learn whether a
/// stamped write of the session committed since.
pub fn last_session_write_seq(session_id: Uuid) -> u64 {
    last_session_write(session_id).map_or(0, |last| last.write_seq)
}

/// The `updated_at` a stamped write of `session_id` stores.
#[cfg(not(any(test, feature = "test-seam")))]
pub(super) fn session_write_clock(_session_id: Uuid) -> DateTime<Utc> {
    Utc::now()
}

/// The `updated_at` a stamped write of `session_id` stores: the next time a
/// test scripted for that session, else `Utc::now()`.
#[cfg(any(test, feature = "test-seam"))]
pub(super) fn session_write_clock(session_id: Uuid) -> DateTime<Utc> {
    scripted_clock()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get_mut(&session_id)
        .and_then(std::collections::VecDeque::pop_front)
        .unwrap_or_else(Utc::now)
}

#[cfg(any(test, feature = "test-seam"))]
type ScriptedClock = Mutex<HashMap<Uuid, std::collections::VecDeque<DateTime<Utc>>>>;

#[cfg(any(test, feature = "test-seam"))]
fn scripted_clock() -> &'static ScriptedClock {
    static CLOCK: OnceLock<ScriptedClock> = OnceLock::new();
    CLOCK.get_or_init(Default::default)
}

/// Script the times the next stamped writes of `session_id` store, in order
/// (test seam): a wall clock that steps back or ties (#265). Keyed by session,
/// so concurrent tests do not interfere.
#[cfg(any(test, feature = "test-seam"))]
pub fn script_session_write_clock_for_test(
    session_id: Uuid,
    times: impl IntoIterator<Item = DateTime<Utc>>,
) {
    scripted_clock()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .entry(session_id)
        .or_default()
        .extend(times);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    fn write(session_id: Uuid, field: SessionWriteField) -> SessionWriteStamp {
        let now = Utc::now();
        OrderedSessionWrite::begin(session_id, field, None, now).committed()
    }

    /// #2876: retiring drops the record, a later write starts a fresh one with
    /// a larger `write_seq`, and another session's record is untouched.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn retiring_a_session_drops_only_its_record_and_never_repeats_a_sequence_2876() {
        let (retired, kept) = (Uuid::new_v4(), Uuid::new_v4());
        let old = write(retired, SessionWriteField::Title);
        let kept_stamp = write(kept, SessionWriteField::Parent);
        assert!(session_writes(retired).is_some());

        assert!(retire_session_writes(retired));
        assert_eq!(session_writes(retired), None);
        assert_eq!(last_session_write_seq(retired), 0);
        assert!(!retire_session_writes(retired), "retiring twice is a no-op");
        assert_eq!(last_session_write(kept), Some(kept_stamp));

        let again = write(retired, SessionWriteField::Title);
        assert!(again.write_seq > old.write_seq);
        assert_eq!(again.run_seq, again.write_seq, "a fresh run");
        retire_session_writes(retired);
        retire_session_writes(kept);
    }

    /// #2876: a stale in-flight fence (a `write_seq` read before the retirement)
    /// never reads as current afterwards, and the delayed stamp it was read
    /// from is still usable as the value it carried.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_fence_read_before_retirement_is_not_current_after_it_2876() {
        let session = Uuid::new_v4();
        let stamp = write(session, SessionWriteField::Title);
        let fence = last_session_write_seq(session);
        assert_eq!(fence, stamp.write_seq);
        retire_session_writes(session);
        assert_ne!(last_session_write_seq(session), fence);
        assert_eq!(stamp.write_seq, fence, "the delayed mirror keeps its stamp");
    }
}
