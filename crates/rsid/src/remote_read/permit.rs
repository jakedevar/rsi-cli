use super::store_budget::{SQL_DEADLINE, with_store_budget_deadline};
use super::{ReadError, Result};
use crate::store::Store;
use rusqlite::Connection;
use serde::Serialize;
use std::io::{self, Write};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;

const MAX_IN_FLIGHT: usize = 4;
const DEADLINE: Duration = Duration::from_millis(250);
const MAX_RESPONSE_BYTES: usize = 512 * 1024;

/// The deadline is cooperative: source reads must check it between bounded
/// steps. It cannot preempt a stalled disk or a blocking system call.
#[derive(Clone, Copy)]
pub struct RemoteReadBudget {
    deadline: Instant,
}

impl RemoteReadBudget {
    pub fn check(&self) -> Result<()> {
        if Instant::now() >= self.deadline {
            Err(ReadError::Busy)
        } else {
            Ok(())
        }
    }
}

/// One daemon-owned limit for Remote reads. Reserve before allocating work;
/// no semaphore wait queue is created for rejected requests.
#[derive(Clone)]
pub struct RemoteReadLimiter {
    slots: Arc<Semaphore>,
    deadline: Duration,
    sql_deadline: Duration,
}

impl RemoteReadLimiter {
    pub(crate) fn new() -> Self {
        Self {
            slots: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
            deadline: DEADLINE,
            sql_deadline: SQL_DEADLINE,
        }
    }

    /// A limiter with its own read and Store transaction deadlines. The production deadline is 250 ms
    /// from admission and includes blocking-pool wait, so a test that asserts
    /// a read's content on a loaded host needs a generous one, and a test of
    /// the deadline itself needs a zero one.
    #[cfg(test)]
    pub(crate) fn with_deadline(deadline: Duration, sql_deadline: Duration) -> Self {
        Self {
            deadline,
            sql_deadline,
            ..Self::new()
        }
    }

    /// Permits not held by an admitted read, a completed result or a pending
    /// response.
    #[cfg(test)]
    pub(crate) fn available_permits(&self) -> usize {
        self.slots.available_permits()
    }

    /// Every Remote RPC connection shares this one daemon process limit.
    pub fn global() -> &'static Self {
        static LIMITER: OnceLock<RemoteReadLimiter> = OnceLock::new();
        LIMITER.get_or_init(Self::new)
    }

    /// The blocking closure owns its permit even if its caller is canceled
    /// while waiting in the blocking pool or while the closure is running.
    /// Completion carries the permit onward for serialization and delivery.
    pub fn spawn<T, F>(&self, work: F) -> Result<JoinHandle<RemoteReadCompleted<T>>>
    where
        T: Send + 'static,
        F: FnOnce(RemoteReadBudget) -> Result<T> + Send + 'static,
    {
        let permit = Arc::clone(&self.slots)
            .try_acquire_owned()
            .map_err(|_| ReadError::Admission)?;
        let budget = RemoteReadBudget {
            deadline: Instant::now() + self.deadline,
        };
        Ok(tokio::task::spawn_blocking(move || {
            let result = budget.check().and_then(|_| work(budget));
            RemoteReadCompleted { result, permit }
        }))
    }

    /// Run a read in capture → Store → finish order under one permit. Runtime
    /// callbacks execute without the Store lock. Store acquisition never
    /// waits, and its transaction/budget are restored before `finish` runs.
    /// The completed result still owns the permit through socket settlement.
    pub fn spawn_staged_store<C, S, T>(
        &self,
        store: Arc<Mutex<Store>>,
        capture: impl FnOnce(RemoteReadBudget) -> Result<C> + Send + 'static,
        read_store: impl FnOnce(&Connection, &C, RemoteReadBudget) -> Result<S> + Send + 'static,
        finish: impl FnOnce(C, S, RemoteReadBudget) -> Result<T> + Send + 'static,
    ) -> Result<JoinHandle<RemoteReadCompleted<T>>>
    where
        C: Send + 'static,
        S: Send + 'static,
        T: Send + 'static,
    {
        let sql_deadline = self.sql_deadline;
        self.spawn(move |budget| {
            budget.check()?;
            let captured = capture(budget)?;
            budget.check()?;
            let saved = with_store_budget_deadline(
                store.try_lock_owned().map_err(|_| ReadError::Busy)?,
                sql_deadline,
                |conn| {
                    budget.check()?;
                    read_store(conn, &captured, budget)
                },
            )?;
            budget.check()?;
            let completed = finish(captured, saved, budget)?;
            budget.check()?;
            Ok(completed)
        })
    }
}

/// The only normal path for releasing a completed Remote permit is after a
/// bounded serialized response has been written and flushed. Socket errors
/// also settle this response and release its permit. The RPC owner maps the
/// source result to a JSON-RPC response while this object remains alive.
pub struct RemoteReadCompleted<T> {
    result: Result<T>,
    permit: OwnedSemaphorePermit,
}

impl<T> RemoteReadCompleted<T> {
    pub async fn send_json_line<W, R>(
        self,
        writer: &mut W,
        map_response: impl FnOnce(Result<T>) -> R,
    ) -> io::Result<()>
    where
        W: AsyncWrite + Unpin,
        R: Serialize,
    {
        let Self { result, permit } = self;
        let response = map_response(result);
        let mut output = BoundedOutput::default();
        serde_json::to_writer(&mut output, &response)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Remote response too large"))?;
        output.write_all(b"\n")?;
        writer.write_all(&output.bytes).await?;
        writer.flush().await?;
        drop(permit);
        Ok(())
    }
}

#[derive(Default)]
struct BoundedOutput {
    bytes: Vec<u8>,
}

impl Write for BoundedOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.bytes.len().saturating_add(bytes.len()) > MAX_RESPONSE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Remote response too large",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(all(
    test,
    any(not(feature = "test-shard-mode"), feature = "test-shard-store-04")
))]
mod tests {
    use super::*;
    use crate::remote_read::RemoteCursorSigner;
    use rsi_common::remote_read::ReadRequestV1;
    use tokio::io::AsyncReadExt;
    use uuid::Uuid;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[tokio::test]
    async fn staged_store_worker_releases_lock_before_cursor_and_keeps_permit() {
        let store = Store::open_in_memory().unwrap();
        store.conn.busy_timeout(Duration::from_secs(10)).unwrap();
        let store = Arc::new(Mutex::new(store));
        let project = Uuid::new_v4();
        let request: ReadRequestV1 = serde_json::from_value(serde_json::json!({
            "method":"RemoteListProjectsV1",
            "params":{"project_ids":[project],"limit":1}
        }))
        .unwrap();
        let signer = RemoteCursorSigner::new(Uuid::new_v4());
        let limiter = RemoteReadLimiter::new();
        let capture_store = Arc::clone(&store);
        let finish_store = Arc::clone(&store);
        let completed = limiter
            .spawn_staged_store(
                Arc::clone(&store),
                move |_| {
                    assert!(capture_store.try_lock().is_ok());
                    Ok(project)
                },
                |conn, captured, _| {
                    assert!(!conn.is_autocommit());
                    Ok(*captured)
                },
                move |captured, saved, _| {
                    assert!(finish_store.try_lock().is_ok());
                    assert_eq!(captured, saved);
                    let cursor = signer
                        .sign_list_position(&request, [7; 32], Some(saved), true)?
                        .unwrap();
                    assert_eq!(
                        signer.verify_list_position(&request, [7; 32], &cursor)?,
                        saved
                    );
                    Ok(cursor)
                },
            )
            .unwrap()
            .await
            .unwrap();
        assert_eq!(limiter.slots.available_permits(), MAX_IN_FLIGHT - 1);
        let (mut writer, mut reader) = tokio::io::duplex(2048);
        completed
            .send_json_line(&mut writer, |result| result.unwrap())
            .await
            .unwrap();
        drop(writer);
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await.unwrap();
        assert!(serde_json::from_slice::<serde_json::Value>(&bytes).is_ok());
        assert_eq!(limiter.slots.available_permits(), MAX_IN_FLIGHT);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[tokio::test]
    async fn staged_store_worker_refuses_held_store_without_waiting() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let held = Arc::clone(&store).lock_owned().await;
        let limiter = RemoteReadLimiter::new();
        let completed = limiter
            .spawn_staged_store(
                store,
                |_| Ok(()),
                |_, _, _| -> Result<()> { panic!("busy Store must not be read") },
                |_, _, _| -> Result<()> { panic!("busy Store must not finish") },
            )
            .unwrap()
            .await
            .unwrap();
        assert!(matches!(&completed.result, Err(ReadError::Busy)));
        assert_eq!(limiter.slots.available_permits(), MAX_IN_FLIGHT - 1);
        drop(completed);
        drop(held);
        assert_eq!(limiter.slots.available_permits(), MAX_IN_FLIGHT);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[tokio::test]
    async fn permit_covers_socket_write_flush_and_settles_after_disconnect() {
        let limiter = RemoteReadLimiter::new();
        let mut completed = Vec::new();
        for index in 0..MAX_IN_FLIGHT {
            completed.push(limiter.spawn(move |_| Ok(index)).unwrap().await.unwrap());
        }
        assert!(matches!(
            limiter.spawn(|_| Ok(())),
            Err(ReadError::Admission)
        ));

        let (mut writer, mut reader) = tokio::io::duplex(16);
        let first = completed.remove(0);
        let send = tokio::spawn(async move {
            first
                .send_json_line(
                    &mut writer,
                    |value| serde_json::json!({"value":value.unwrap(),"padding":"x".repeat(128)}),
                )
                .await
        });
        tokio::task::yield_now().await;
        assert!(matches!(
            limiter.spawn(|_| Ok(())),
            Err(ReadError::Admission)
        ));
        let mut received = Vec::new();
        reader.read_to_end(&mut received).await.unwrap();
        send.await.unwrap().unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&received).unwrap()["value"],
            0
        );
        drop(completed);
        let reused = limiter.spawn(|_| Ok(())).unwrap().await.unwrap();
        drop(reused);

        let completed = limiter.spawn(|_| Ok(())).unwrap().await.unwrap();
        let (mut writer, reader) = tokio::io::duplex(16);
        drop(reader);
        assert!(
            completed
                .send_json_line(&mut writer, |result| result.unwrap())
                .await
                .is_err()
        );
        assert_eq!(limiter.slots.available_permits(), MAX_IN_FLIGHT);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[tokio::test]
    async fn canceled_waiter_cannot_release_a_running_blocking_worker() {
        let limiter = RemoteReadLimiter::new();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let work = limiter
            .spawn(move |budget| {
                budget.check()?;
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            })
            .unwrap();
        let waiter = tokio::spawn(async move { work.await });
        started_rx.await.unwrap();
        waiter.abort();
        assert_eq!(limiter.slots.available_permits(), MAX_IN_FLIGHT - 1);
        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while limiter.slots.available_permits() != MAX_IN_FLIGHT {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[tokio::test]
    async fn oversized_serialization_writes_no_partial_response() {
        let limiter = RemoteReadLimiter::new();
        let completed = limiter.spawn(|_| Ok(())).unwrap().await.unwrap();
        let (mut writer, mut reader) = tokio::io::duplex(16);
        let error = completed
            .send_json_line(&mut writer, |_| "x".repeat(MAX_RESPONSE_BYTES))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        drop(writer);
        let mut received = Vec::new();
        reader.read_to_end(&mut received).await.unwrap();
        assert!(received.is_empty());
        assert_eq!(limiter.slots.available_permits(), MAX_IN_FLIGHT);
    }
}
