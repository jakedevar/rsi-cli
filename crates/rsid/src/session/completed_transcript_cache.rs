//! Bounded, daemon-wide cache for completed conversation reads.
//!
//! The LRU lock is held only for metadata and admission. Each session has its
//! own flight gate, so duplicate readers share one SQLite load while unrelated
//! sessions load concurrently. Zero-cap and oversize results live only as long
//! as the readers already sharing that flight.

use crate::error::Result;
use crate::store::Store;
use parking_lot::Mutex;
use rsi_common::types::ConversationEvent;
use std::collections::{HashMap, VecDeque};
use std::mem::size_of;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

type Transcript = Arc<Vec<ConversationEvent>>;

#[derive(Default)]
struct Flight {
    gate: AsyncMutex<Option<(u64, Transcript)>>,
    /// Resume increments this before an in-flight read may publish its result.
    epoch: AtomicU64,
}

#[derive(Default)]
pub(super) struct CachedTranscripts {
    entries: HashMap<Uuid, (Transcript, u64)>,
    oldest_first: VecDeque<Uuid>,
    flights: HashMap<Uuid, Weak<Flight>>,
    bytes: u64,
}

impl CachedTranscripts {
    fn event_bytes(event: &ConversationEvent) -> u64 {
        // The retained Vec and owned string capacities are charged, including
        // any spare allocation. JSON payloads use their encoded byte length
        // as a deterministic accounting unit independent of allocator internals.
        let strings = event.content.capacity()
            + event.tool_name.as_ref().map_or(0, String::capacity)
            + event.offload_id.as_ref().map_or(0, String::capacity)
            + event.tool_use_id.as_ref().map_or(0, String::capacity);
        let json = event
            .tool_input
            .as_ref()
            .and_then(|value| serde_json::to_vec(value).ok())
            .map_or(0, |value| value.len())
            + event
                .metadata
                .as_ref()
                .and_then(|value| serde_json::to_vec(value).ok())
                .map_or(0, |value| value.len());
        (strings + json) as u64
    }

    #[allow(clippy::ptr_arg)] // The Vec capacity is part of the retained-byte charge.
    fn transcript_bytes(events: &Vec<ConversationEvent>) -> u64 {
        let fixed = size_of::<Uuid>()
            + size_of::<Transcript>()
            + size_of::<u64>()
            + size_of::<Vec<ConversationEvent>>()
            + events.capacity() * size_of::<ConversationEvent>();
        fixed as u64 + events.iter().map(Self::event_bytes).sum::<u64>()
    }

    fn remove(&mut self, session_id: Uuid) {
        if let Some((_, bytes)) = self.entries.remove(&session_id) {
            self.bytes -= bytes;
        }
        self.oldest_first.retain(|id| *id != session_id);
    }

    fn shrink(&mut self, cap: u64) {
        while self.bytes > cap {
            let Some(oldest) = self.oldest_first.pop_front() else {
                break;
            };
            if let Some((_, bytes)) = self.entries.remove(&oldest) {
                self.bytes -= bytes;
            }
        }
    }

    fn get(&mut self, session_id: Uuid) -> Option<Transcript> {
        let events = Arc::clone(&self.entries.get(&session_id)?.0);
        self.oldest_first.retain(|id| *id != session_id);
        self.oldest_first.push_back(session_id);
        Some(events)
    }

    fn admit(&mut self, session_id: Uuid, events: Transcript, bytes: u64, cap: u64) {
        if bytes > cap || cap == 0 {
            return; // Exact oversize bypass: retain existing LRU entries.
        }
        self.remove(session_id);
        self.bytes += bytes;
        self.entries.insert(session_id, (events, bytes));
        self.oldest_first.push_back(session_id);
        self.shrink(cap);
    }

    fn flight(&mut self, session_id: Uuid) -> Arc<Flight> {
        if let Some(flight) = self.flights.get(&session_id).and_then(Weak::upgrade) {
            return flight;
        }
        let flight = Arc::new(Flight::default());
        self.flights.insert(session_id, Arc::downgrade(&flight));
        flight
    }
}

pub(super) struct CompletedTranscriptCache {
    inner: Mutex<CachedTranscripts>,
}

/// A cancelled reader also retires its weak flight entry when it is last.
struct FlightLease<'a> {
    cache: &'a CompletedTranscriptCache,
    session_id: Uuid,
    flight: Option<Arc<Flight>>,
}

impl Drop for FlightLease<'_> {
    fn drop(&mut self) {
        self.flight.take();
        let mut cache = self.cache.inner.lock();
        if cache
            .flights
            .get(&self.session_id)
            .is_some_and(|flight| flight.upgrade().is_none())
        {
            cache.flights.remove(&self.session_id);
        }
    }
}

impl CompletedTranscriptCache {
    pub(super) fn new() -> Self {
        Self {
            inner: Mutex::new(CachedTranscripts::default()),
        }
    }

    pub(super) async fn evict(&self, session_id: Uuid) {
        let mut cache = self.inner.lock();
        cache.remove(session_id);
        if let Some(flight) = cache.flights.get(&session_id).and_then(Weak::upgrade) {
            flight.epoch.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// An acknowledged cap update has evicted entries under the admission lock.
    pub(super) async fn publish_cap(&self, cap: &AtomicU64, new_cap: u64) {
        let mut cache = self.inner.lock();
        cap.store(new_cap, Ordering::Release);
        cache.shrink(new_cap);
    }

    #[cfg(test)]
    pub(super) async fn contains(&self, session_id: Uuid) -> bool {
        self.inner.lock().entries.contains_key(&session_id)
    }

    pub(super) async fn get_or_load(
        &self,
        store: &Arc<AsyncMutex<Store>>,
        session_id: Uuid,
        cap: &AtomicU64,
    ) -> Result<Vec<ConversationEvent>> {
        self.get_or_load_with(
            session_id,
            || cap.load(Ordering::Acquire),
            || async { super::queries::load_completed_events_from_store(store, session_id).await },
        )
        .await
    }

    async fn get_or_load_with<C, F, Fut>(
        &self,
        session_id: Uuid,
        cap: C,
        load: F,
    ) -> Result<Vec<ConversationEvent>>
    where
        C: Fn() -> u64,
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Vec<ConversationEvent>>>,
    {
        let flight = {
            let mut cache = self.inner.lock();
            cache.shrink(cap());
            if let Some(events) = cache.get(session_id) {
                drop(cache);
                return Ok(events.as_ref().clone());
            }
            cache.flight(session_id)
        };
        let lease = FlightLease {
            cache: self,
            session_id,
            flight: Some(flight),
        };
        let flight = lease.flight.as_ref().expect("flight lease owns the gate");
        let mut gate = flight.gate.lock().await;
        let epoch = flight.epoch.load(Ordering::Acquire);
        {
            let mut cache = self.inner.lock();
            cache.shrink(cap());
            if let Some(events) = cache.get(session_id) {
                drop(cache);
                drop(gate);
                return Ok(events.as_ref().clone());
            }
        }
        if let Some((published_epoch, events)) = gate.as_ref()
            && *published_epoch == epoch
        {
            let events = Arc::clone(events);
            drop(gate);
            return Ok(events.as_ref().clone());
        }
        let events = Arc::new(load().await?);
        let bytes = CachedTranscripts::transcript_bytes(events.as_ref());
        {
            let mut cache = self.inner.lock();
            let current_cap = cap();
            cache.shrink(current_cap);
            if flight.epoch.load(Ordering::Acquire) == epoch {
                cache.admit(session_id, Arc::clone(&events), bytes, current_cap);
                *gate = Some((epoch, Arc::clone(&events)));
            }
        }
        drop(gate);
        Ok(events.as_ref().clone())
    }
}

#[cfg(all(
    test,
    any(not(feature = "test-shard-mode"), feature = "test-shard-session-01")
))]
mod tests {
    use super::*;
    use rsi_common::types::{EventType, Role};

    fn admit_events(
        cache: &mut CachedTranscripts,
        id: Uuid,
        events: Vec<ConversationEvent>,
        cap: u64,
    ) {
        let bytes = CachedTranscripts::transcript_bytes(&events);
        cache.admit(id, Arc::new(events), bytes, cap);
    }

    fn event(session_id: Uuid, size: usize) -> ConversationEvent {
        ConversationEvent {
            id: 1,
            session_id,
            sequence: 1,
            event_type: EventType::Message,
            role: Some(Role::Assistant),
            created_at: chrono::Utc::now(),
            content: "x".repeat(size),
            tool_name: None,
            tool_input: None,
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn lru_oversize_and_shrink() {
        let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let small = vec![event(a, 16)];
        let one = CachedTranscripts::transcript_bytes(&small);
        let mut cache = CachedTranscripts::default();
        admit_events(&mut cache, a, small.clone(), one - 1);
        assert_eq!(cache.bytes, 0);
        admit_events(&mut cache, a, small.clone(), one);
        assert_eq!(cache.bytes, one);
        cache.remove(a);
        admit_events(&mut cache, a, small, one * 2);
        admit_events(&mut cache, b, vec![event(b, 16)], one * 2);
        cache.get(a);
        admit_events(&mut cache, c, vec![event(c, 16)], one * 2);
        assert!(cache.entries.contains_key(&a));
        assert!(cache.entries.contains_key(&c));
        assert!(!cache.entries.contains_key(&b));
        admit_events(&mut cache, b, vec![event(b, one as usize * 3)], one * 2);
        assert_eq!(cache.entries.len(), 2);
        cache.shrink(one);
        assert_eq!(cache.bytes, one);
        cache.shrink(0);
        assert_eq!(cache.bytes, 0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn accounting_charges_spare_vec_capacity() {
        let id = Uuid::new_v4();
        let tight = vec![event(id, 16)];
        let mut spare = Vec::with_capacity(16);
        spare.push(event(id, 16));
        assert!(spare.capacity() > tight.capacity());
        assert!(
            CachedTranscripts::transcript_bytes(&spare)
                > CachedTranscripts::transcript_bytes(&tight)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn concurrent_duplicate_reads_load_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let cache = Arc::new(CompletedTranscriptCache::new());
        let loads = Arc::new(AtomicUsize::new(0));
        let id = Uuid::new_v4();
        let mut joins = Vec::new();
        for _ in 0..8 {
            let cache = Arc::clone(&cache);
            let loads = Arc::clone(&loads);
            joins.push(tokio::spawn(async move {
                cache
                    .get_or_load_with(
                        id,
                        || 1024,
                        || async {
                            loads.fetch_add(1, Ordering::SeqCst);
                            tokio::task::yield_now().await;
                            Ok(vec![event(id, 16)])
                        },
                    )
                    .await
                    .unwrap()
            }));
        }
        for join in joins {
            assert_eq!(join.await.unwrap()[0].session_id, id);
        }
        assert_eq!(loads.load(Ordering::SeqCst), 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn distinct_sessions_load_concurrently() {
        let cache = Arc::new(CompletedTranscriptCache::new());
        let both_started = Arc::new(tokio::sync::Barrier::new(3));
        let mut joins = Vec::new();
        for id in [Uuid::new_v4(), Uuid::new_v4()] {
            let cache = Arc::clone(&cache);
            let both_started = Arc::clone(&both_started);
            joins.push(tokio::spawn(async move {
                cache
                    .get_or_load_with(
                        id,
                        || 1024,
                        || async {
                            both_started.wait().await;
                            Ok(vec![event(id, 16)])
                        },
                    )
                    .await
                    .unwrap()
            }));
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), both_started.wait())
            .await
            .expect("unrelated loads reach SQLite together");
        for join in joins {
            assert_eq!(join.await.unwrap().len(), 1);
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn resume_eviction_fences_in_flight_cache_admission() {
        let cache = Arc::new(CompletedTranscriptCache::new());
        let id = Uuid::new_v4();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let reader = {
            let cache = Arc::clone(&cache);
            tokio::spawn(async move {
                cache
                    .get_or_load_with(
                        id,
                        || 1024,
                        || async {
                            started_tx.send(()).unwrap();
                            release_rx.await.unwrap();
                            Ok(vec![event(id, 16)])
                        },
                    )
                    .await
                    .unwrap()
            })
        };
        started_rx.await.unwrap();
        cache.evict(id).await;
        release_tx.send(()).unwrap();
        assert_eq!(reader.await.unwrap().len(), 1);
        assert!(!cache.contains(id).await);
        cache
            .get_or_load_with(id, || 1024, || async { Ok(vec![event(id, 32)]) })
            .await
            .unwrap();
        assert!(cache.contains(id).await);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn published_zero_evicts_before_ack_and_controls_later_reads() {
        let cache = CompletedTranscriptCache::new();
        let id = Uuid::new_v4();
        let cap = AtomicU64::new(1024);
        cache
            .get_or_load_with(
                id,
                || cap.load(Ordering::Acquire),
                || async { Ok(vec![event(id, 16)]) },
            )
            .await
            .unwrap();
        assert!(cache.contains(id).await);
        cache.publish_cap(&cap, 0).await;
        assert_eq!(cap.load(Ordering::Acquire), 0);
        assert!(!cache.contains(id).await);
        cache
            .get_or_load_with(
                id,
                || cap.load(Ordering::Acquire),
                || async { Ok(vec![event(id, 16)]) },
            )
            .await
            .unwrap();
        assert!(!cache.contains(id).await);
    }
}
