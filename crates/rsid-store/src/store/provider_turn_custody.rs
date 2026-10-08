//! Durable turn identity and replay cursor. This module does not prove process
//! liveness: callers must verify the shim lock and platform identity before
//! claiming or signaling a turn. `boot_id` is the owning daemon's boot UUID,
//! not the operating system's boot identity.

use super::Store;
use crate::error::{DaemonError, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use rsi_common::closure_kernel::ConversationEventProvenanceV1;
use rsi_common::types::{ConversationEvent, PendingQuestion};
use rusqlite::{OptionalExtension, Row, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::path::{Component, PathBuf};
use std::str::FromStr;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderTurnCustodyState {
    Live,
    Adopted,
    Finished,
    Abandoned,
}

impl ProviderTurnCustodyState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Adopted => "adopted",
            Self::Finished => "finished",
            Self::Abandoned => "abandoned",
        }
    }
}

/// The PID identifies the shim. Linux callers supply its /proc start-time
/// ticks; platforms without that identity source may leave `start_time` null.
#[derive(Debug, Clone)]
pub struct NewProviderTurnCustody {
    pub invocation_id: Uuid,
    pub session_id: Uuid,
    pub spool_dir: PathBuf,
    pub pid: u32,
    pub start_time: Option<u64>,
    pub boot_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderTurnCustody {
    pub invocation_id: Uuid,
    pub session_id: Uuid,
    pub spool_dir: PathBuf,
    pub pid: u32,
    pub start_time: Option<u64>,
    pub boot_id: Uuid,
    /// Bytes consumed through the last complete newline. The stream reader
    /// must commit this cursor together with the events it persists.
    pub stdout_offset: u64,
    pub state: ProviderTurnCustodyState,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl ProviderTurnCustody {
    /// A row alone is never enough to preserve a process across a deploy.
    /// Linux also requires the recorded shim incarnation and exact env tuple.
    pub fn is_adoptable(&self) -> Result<bool> {
        self.is_adoptable_at(
            std::path::Path::new("/proc"),
            rsi_common::identity::process_ownership_namespace().as_bytes(),
        )
    }

    pub fn is_adoptable_at(&self, proc_root: &std::path::Path, namespace: &[u8]) -> Result<bool> {
        #[cfg(target_os = "linux")]
        use std::io::Read;
        if !matches!(
            self.state,
            ProviderTurnCustodyState::Live | ProviderTurnCustodyState::Adopted
        ) {
            return Ok(false);
        }
        let file = match std::fs::OpenOptions::new()
            .write(true)
            .open(self.spool_dir.join("alive.lock"))
        {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            other => other?,
        };
        match file.try_lock() {
            Ok(()) => return Ok(false),
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::MetadataExt;
            let process = proc_root.join(self.pid.to_string());
            let read = |name: &str, limit: u64| -> Result<Option<Vec<u8>>> {
                let file = match std::fs::File::open(process.join(name)) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    other => other?,
                };
                let mut bytes = Vec::new();
                file.take(limit + 1).read_to_end(&mut bytes)?;
                if bytes.len() as u64 > limit {
                    return Err(DaemonError::Process(
                        "turn identity exceeds its bound".into(),
                    ));
                }
                Ok(Some(bytes))
            };
            let Some(stat) = read("stat", 16384)? else {
                return Ok(false);
            };
            let stat = String::from_utf8_lossy(&stat);
            let start = stat
                .rsplit_once(") ")
                .and_then(|(_, fields)| fields.split_whitespace().nth(19))
                .and_then(|value| value.parse::<u64>().ok());
            if self.start_time.is_none() || start != self.start_time {
                return Ok(false);
            }
            if std::fs::metadata(&process)?.uid()
                != std::fs::metadata(proc_root.join("self"))?.uid()
            {
                return Ok(false);
            }
            let Some(env) = read("environ", 2 * 1024 * 1024)? else {
                return Ok(false);
            };
            let exact = |key: &[u8], expected: &[u8]| {
                let mut values = env
                    .split(|byte| *byte == 0)
                    .filter_map(|field| field.strip_prefix(key));
                values.next() == Some(expected) && values.next().is_none()
            };
            if !exact(b"RSI_SESSION_ID=", self.session_id.to_string().as_bytes())
                || !exact(
                    b"RSI_MODEL_INVOCATION_ID=",
                    self.invocation_id.to_string().as_bytes(),
                )
                || !exact(b"RSI_PROCESS_OWNERSHIP_NAMESPACE=", namespace)
            {
                return Ok(false);
            }
            // Recheck after reading identity; an exited shim releases the lock.
            match file.try_lock() {
                Ok(()) => return Ok(false),
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = (proc_root, namespace);
        Ok(true)
    }
}

/// Daemon-private line receipt, supplied by the spool reader, never from JSON.
#[derive(Debug, Clone, Copy)]
pub struct ProviderTurnCursor {
    pub invocation_id: Uuid,
    pub boot_id: Uuid,
    pub expected_offset: u64,
    pub next_offset: u64,
}

pub struct ProviderTurnEvent {
    pub event: ConversationEvent,
    pub provenance: Option<ConversationEventProvenanceV1>,
    pub question: Option<PendingQuestion>,
}

fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn sqlite_integer(value: u64) -> Result<i64> {
    i64::try_from(value)
        .map_err(|_| DaemonError::Store("turn custody integer exceeds SQLite range".into()))
}

fn decode<T>(row: &Row<'_>, index: usize) -> rusqlite::Result<T>
where
    T: FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    let value: String = row.get(index)?;
    value.parse().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

fn map_row(row: &Row<'_>) -> rusqlite::Result<ProviderTurnCustody> {
    let state: String = row.get(7)?;
    let state = match state.as_str() {
        "live" => ProviderTurnCustodyState::Live,
        "adopted" => ProviderTurnCustodyState::Adopted,
        "finished" => ProviderTurnCustodyState::Finished,
        "abandoned" => ProviderTurnCustodyState::Abandoned,
        _ => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                7,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "unknown turn custody state",
                )),
            ));
        }
    };
    Ok(ProviderTurnCustody {
        invocation_id: decode(row, 0)?,
        session_id: decode(row, 1)?,
        spool_dir: PathBuf::from(row.get::<_, String>(2)?),
        pid: row.get(3)?,
        start_time: row.get(4)?,
        boot_id: decode(row, 5)?,
        stdout_offset: row.get(6)?,
        state,
        created_at: decode(row, 8)?,
        updated_at: decode(row, 9)?,
    })
}

impl Store {
    /// Free a crashed daemon's active slot before registering a fresh turn.
    /// Keep an acquired lock through the observed-boot CAS; a held lock means
    /// the old turn still owns its spool and needs startup adoption.
    pub fn abandon_unlocked_provider_turns_from_prior_boot(
        &self,
        session_id: Uuid,
        current_boot_id: Uuid,
    ) -> Result<()> {
        if current_boot_id.is_nil() {
            return Err(DaemonError::Store(
                "turn custody boot UUID must be non-nil".into(),
            ));
        }
        let old: Option<ProviderTurnCustody> = self.conn.query_row(
            "SELECT invocation_id,session_id,spool_dir,pid,start_time,boot_id,stdout_offset,state,created_at,updated_at
             FROM provider_turn_custody WHERE session_id=?1 AND boot_id<>?2 AND state IN ('live','adopted')",
            params![session_id.to_string(), current_boot_id.to_string()], map_row,
        ).optional()?;
        let Some(old) = old else {
            return Ok(());
        };
        let lock = match std::fs::OpenOptions::new()
            .write(true)
            .open(old.spool_dir.join("alive.lock"))
        {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            other => Some(other?),
        };
        if let Some(lock) = &lock {
            match lock.try_lock() {
                Ok(()) => {},
                Err(std::fs::TryLockError::WouldBlock) => return Err(DaemonError::Process(
                    "live_detached_turn: prior-boot turn still holds alive.lock; startup adoption required".into(),
                )),
                Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
            }
        }
        if !self.abandon_provider_turn_custody(old.invocation_id, old.boot_id)? {
            return Err(DaemonError::Process(
                "turn_custody_changed: prior-boot custody changed during cleanup".into(),
            ));
        }
        drop(lock);
        Ok(())
    }

    /// Insert once, at offset zero. Duplicate identities, spool reuse, an
    /// invocation belonging to another session, or a second active turn for
    /// the session are errors; no existing custody is overwritten.
    pub fn insert_provider_turn_custody(&self, turn: &NewProviderTurnCustody) -> Result<()> {
        if turn.invocation_id.is_nil() || turn.session_id.is_nil() || turn.boot_id.is_nil() {
            return Err(DaemonError::Store(
                "turn custody UUID must be non-nil".into(),
            ));
        }
        if turn.pid == 0 || turn.pid > i32::MAX as u32 {
            return Err(DaemonError::Store(
                "turn custody PID must be positive".into(),
            ));
        }
        let path = &turn.spool_dir;
        if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err(DaemonError::Store(
                "turn custody spool directory must be absolute without parent traversal".into(),
            ));
        }
        let path = path.to_str().ok_or_else(|| {
            DaemonError::Store("turn custody spool directory must be UTF-8".into())
        })?;
        let start_time = turn.start_time.map(sqlite_integer).transpose()?;
        let at = now();
        self.conn.execute(
            "INSERT INTO provider_turn_custody
             (invocation_id,session_id,spool_dir,pid,start_time,boot_id,stdout_offset,state,created_at,updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,0,'live',?7,?7)",
            params![turn.invocation_id.to_string(), turn.session_id.to_string(), path,
                turn.pid, start_time, turn.boot_id.to_string(), at],
        )?;
        Ok(())
    }

    pub fn get_provider_turn_custody(
        &self,
        invocation_id: Uuid,
    ) -> Result<Option<ProviderTurnCustody>> {
        Ok(self.conn.query_row(
            "SELECT invocation_id,session_id,spool_dir,pid,start_time,boot_id,stdout_offset,state,created_at,updated_at
             FROM provider_turn_custody WHERE invocation_id=?1",
            [invocation_id.to_string()], map_row,
        ).optional()?)
    }

    /// Startup candidates only: being listed is not proof that a shim lives.
    pub fn list_active_provider_turn_custody(&self) -> Result<Vec<ProviderTurnCustody>> {
        let mut statement = self.conn.prepare(
            "SELECT invocation_id,session_id,spool_dir,pid,start_time,boot_id,stdout_offset,state,created_at,updated_at
             FROM provider_turn_custody WHERE state IN ('live','adopted') ORDER BY created_at,invocation_id"
        )?;
        Ok(statement
            .query_map([], map_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn adoptable_provider_turn_for_session(
        &self,
        session_id: Uuid,
    ) -> Result<Option<ProviderTurnCustody>> {
        let Some(invocation) = self.session_model_invocation_id(session_id)? else {
            return Ok(None);
        };
        let Some(turn) = self.get_provider_turn_custody(invocation)? else {
            return Ok(None);
        };
        if turn.session_id == session_id && turn.is_adoptable()? {
            Ok(Some(turn))
        } else {
            Ok(None)
        }
    }

    /// Compare the boot observed in the custody row, then transfer ownership.
    /// A stale claimant (including a retry after a successful claim) gets
    /// false. Re-adoption on a later restart works from `adopted` too.
    pub fn claim_provider_turn_custody(
        &self,
        invocation_id: Uuid,
        expected_boot_id: Uuid,
        new_boot_id: Uuid,
    ) -> Result<bool> {
        if new_boot_id.is_nil() {
            return Err(DaemonError::Store(
                "turn custody boot UUID must be non-nil".into(),
            ));
        }
        if expected_boot_id == new_boot_id {
            return Ok(false);
        }
        Ok(self.conn.execute(
            "UPDATE provider_turn_custody SET boot_id=?3,state='adopted',updated_at=?4
             WHERE invocation_id=?1 AND boot_id=?2 AND state IN ('live','adopted')",
            params![
                invocation_id.to_string(),
                expected_boot_id.to_string(),
                new_boot_id.to_string(),
                now()
            ],
        )? == 1)
    }

    /// A boot and cursor CAS prevents stale spool readers from advancing the
    /// replay cursor. The caller supplies a complete-line boundary; this store
    /// method cannot inspect a spool or persist stream events on its own.
    pub fn advance_provider_turn_stdout_offset(
        &self,
        invocation_id: Uuid,
        boot_id: Uuid,
        expected_offset: u64,
        new_offset: u64,
    ) -> Result<bool> {
        if new_offset < expected_offset {
            return Err(DaemonError::Store(
                "turn custody cursor cannot move backwards".into(),
            ));
        }
        let expected = sqlite_integer(expected_offset)?;
        let next = sqlite_integer(new_offset)?;
        Ok(self.conn.execute(
            "UPDATE provider_turn_custody SET stdout_offset=?4,updated_at=?5
             WHERE invocation_id=?1 AND boot_id=?2 AND stdout_offset=?3
               AND stdout_offset<?4 AND state IN ('live','adopted')",
            params![
                invocation_id.to_string(),
                boot_id.to_string(),
                expected,
                next,
                now()
            ],
        )? == 1)
    }

    /// Commit all events produced by one complete NDJSON line and its cursor.
    /// Empty/ignored lines commit an empty batch. A stale boot or cursor changes
    /// neither events nor the cursor. An insert failure rolls the whole batch
    /// back; a question's prior identity remains explicitly unresolved.
    pub fn consume_provider_turn_line(
        &self,
        cursor: ProviderTurnCursor,
        events: &[ProviderTurnEvent],
    ) -> Result<Option<Vec<i64>>> {
        if cursor.next_offset <= cursor.expected_offset {
            return Err(DaemonError::Store(
                "turn line requires a complete advancing cursor".into(),
            ));
        }
        let expected = sqlite_integer(cursor.expected_offset)?;
        let next = sqlite_integer(cursor.next_offset)?;
        // Reserve only the last question, which is the answerable publication
        // after a frame containing several tool blocks. Fence invalidation too.
        let question_index = events.iter().rposition(|item| item.question.is_some());
        let publication = if let Some(index) = question_index {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let current: Option<String> = tx.query_row(
                "SELECT session_id FROM provider_turn_custody WHERE invocation_id=?1 AND boot_id=?2
                 AND stdout_offset=?3 AND state IN ('live','adopted')",
                params![cursor.invocation_id.to_string(), cursor.boot_id.to_string(), expected], |r| r.get(0),
            ).optional()?;
            let Some(session_id) = current else {
                return Ok(None);
            };
            if events
                .iter()
                .any(|item| item.event.session_id.to_string() != session_id)
            {
                return Err(DaemonError::Store(
                    "turn event belongs to another session".into(),
                ));
            }
            let item = &events[index];
            let raw = serde_json::to_string(item.question.as_ref().unwrap())?;
            let publication = Self::reserve_pending_question_in_transaction(
                &tx,
                item.event.session_id,
                Some(&raw),
            )?;
            tx.commit()?;
            Some(publication)
        } else {
            None
        };
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let session_id: Option<String> = tx.query_row(
            "UPDATE provider_turn_custody SET stdout_offset=?4,updated_at=?5
             WHERE invocation_id=?1 AND boot_id=?2 AND stdout_offset=?3 AND state IN ('live','adopted')
             RETURNING session_id",
            params![cursor.invocation_id.to_string(), cursor.boot_id.to_string(), expected, next, now()], |r| r.get(0),
        ).optional()?;
        let Some(session_id) = session_id else {
            return Ok(None);
        };
        let mut ids = Vec::with_capacity(events.len());
        for (index, item) in events.iter().enumerate() {
            if item.event.session_id.to_string() != session_id {
                return Err(DaemonError::Store(
                    "turn event belongs to another session".into(),
                ));
            }
            let id = if question_index == Some(index) {
                Self::bind_pending_question_in_transaction(
                    &tx,
                    publication.as_ref().unwrap(),
                    &item.event,
                    item.provenance.as_ref(),
                    item.question.as_ref().unwrap(),
                )?
            } else {
                let id =
                    Self::insert_event_in_transaction(&tx, &item.event, item.provenance.as_ref())?;
                if item.event.tool_name.as_deref() == Some("AskUserQuestion") {
                    tx.execute("UPDATE pending_question_publications SET state='unresolved',conversation_event_id=NULL,
                        event_sequence=NULL,tool_use_id=NULL,model_invocation_id=NULL WHERE session_id=?1",
                        [session_id.as_str()])?;
                }
                id
            };
            ids.push(id);
        }
        tx.commit()?;
        for (item, id) in events.iter().zip(&ids) {
            self.note_committed_provider_event(&item.event, *id);
        }
        Ok(Some(ids))
    }

    /// Call after observing durable exit.json and consuming the final events.
    pub fn finish_provider_turn_custody(&self, invocation_id: Uuid, boot_id: Uuid) -> Result<bool> {
        self.settle_provider_turn_custody(
            invocation_id,
            boot_id,
            ProviderTurnCustodyState::Finished,
        )
    }

    /// Call when the lock is released without a durable exit, or identity
    /// cannot be proven. Abandonment is distinct from provider completion.
    pub fn abandon_provider_turn_custody(
        &self,
        invocation_id: Uuid,
        boot_id: Uuid,
    ) -> Result<bool> {
        self.settle_provider_turn_custody(
            invocation_id,
            boot_id,
            ProviderTurnCustodyState::Abandoned,
        )
    }

    fn settle_provider_turn_custody(
        &self,
        invocation_id: Uuid,
        boot_id: Uuid,
        state: ProviderTurnCustodyState,
    ) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE provider_turn_custody SET state=?3,updated_at=?4
             WHERE invocation_id=?1 AND boot_id=?2 AND state IN ('live','adopted')",
            params![
                invocation_id.to_string(),
                boot_id.to_string(),
                state.as_str(),
                now()
            ],
        )? == 1)
    }
}

#[cfg(test)]
mod tests;
