//! hub #2919 (local #280, #2748 G15): the RSI preamble a `SessionStart`
//! provider lifecycle hook delivers instead of the first prompt.
//!
//! With `SessionStart` in the operator setting `provider_lifecycle_hooks`, a
//! fresh Claude or Codex launch whose hook is installed (a session identity
//! and the daemon's `rsi-rpc` sibling) leaves a deliverable preamble out of its
//! prompt and defers it here. Claude splits long preambles at existing startup
//! part boundaries with [`defer_parts`]; each labelled chunk fits
//! [`MAX_SESSION_START_CONTEXT_CHARS`]. Codex keeps its single-part path.
//! The session's `SessionStart` hook claims it through
//! `RecordProviderLifecycleEvent` ([`claim_part_for_hook`]) and prints it as
//! `additionalContext`. Every other launch passes exactly the prompt it passed
//! before this module existed.
//!
//! A UserPromptSubmit handler retries unconfirmed parts before the model's
//! first response, including sessions that never call a tool. The next-launch
//! prompt remains the final fallback. Store owns the session-scoped JSON state;
//! writes commit before context leaves the launch prompt or a hook reply.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use rsi_common::provider_lifecycle_hooks::{
    MAX_SESSION_START_CONTEXT_CHARS, RecordProviderLifecycleEventParams,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Maximum cached sessions; eviction never removes durable debt.
pub const MAX_TRACKED_SESSIONS: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Delivery {
    Owed,
    Delivered,
}

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    text: String,
    delivery: Delivery,
    chunks: Vec<String>,
    claimed: Vec<bool>,
    confirmed: Vec<bool>,
    batch: Uuid,
    source: Option<String>,
    evidence: Vec<Option<Evidence>>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Evidence {
    params: RecordProviderLifecycleEventParams,
    cursor: u64,
}

#[derive(Default)]
struct Registry {
    entries: HashMap<Uuid, Entry>,
    order: VecDeque<Uuid>,
    store: Option<Weak<rsid_core::store_lock::Mutex<crate::store::Store>>>,
}

impl Registry {
    fn persistent(store: &Arc<rsid_core::store_lock::Mutex<crate::store::Store>>) -> Self {
        Self {
            store: Some(Arc::downgrade(store)),
            ..Self::default()
        }
    }

    fn cache(&mut self, session: Uuid, entry: Option<Entry>) -> bool {
        self.entries.remove(&session);
        self.order.retain(|id| *id != session);
        if let Some(entry) = entry {
            self.entries.insert(session, entry);
            self.order.push_back(session);
        }
        while self.order.len() > MAX_TRACKED_SESSIONS {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            }
        }
        true
    }
}

trait StateAccess {
    fn get(&self, session: Uuid) -> Option<Entry>;
    fn put(&mut self, session: Uuid, entry: Option<Entry>) -> bool;
}

impl StateAccess for Registry {
    fn get(&self, session: Uuid) -> Option<Entry> {
        if let Some(entry) = self.entries.get(&session) {
            return Some(entry.clone());
        }
        let store = self.store.as_ref()?.upgrade()?;
        // Provider command construction is synchronous and can already hold a
        // seat fence on Store. Never block or open a second connection. A busy
        // Store retains the durable debt for the next hook/launch attempt.
        let store = store.try_lock().ok()?;
        match store.get_session_start_preamble(session) {
            Ok(Some(value)) => match serde_json::from_str(&value) {
                Ok(entry) => Some(entry),
                Err(error) => {
                    tracing::error!(%session, %error, "invalid durable SessionStart state");
                    None
                }
            },
            Ok(None) => None,
            Err(error) => {
                tracing::error!(%session, %error, "cannot read durable SessionStart state");
                None
            }
        }
    }

    /// Persist first. A failed or contended write never acknowledges serving
    /// or consuming debt. In particular, a fresh launch keeps its prompt.
    fn put(&mut self, session: Uuid, entry: Option<Entry>) -> bool {
        if let Some(store) = &self.store {
            let Some(store) = store.upgrade() else {
                return false;
            };
            let Ok(store) = store.try_lock() else {
                return false;
            };
            let value = entry
                .as_ref()
                .map(|entry| serde_json::to_string(entry).expect("serializable preamble state"));
            if let Err(error) = store.set_session_start_preamble(session, value.as_deref()) {
                tracing::error!(%session, %error, "cannot persist SessionStart state; retain prompt/debt");
                return false;
            }
        }
        self.cache(session, entry)
    }
}

/// Async hook callers await the shared Store lock before the cache lock. Sync
/// launch construction uses try_lock above, since it can already hold a seat
/// fence. Both paths use exactly the same Store and persist before caching.
struct LockedRegistry<'a> {
    registry: &'a mut Registry,
    store: &'a crate::store::Store,
}

impl StateAccess for LockedRegistry<'_> {
    fn get(&self, session: Uuid) -> Option<Entry> {
        if let Some(entry) = self.registry.entries.get(&session) {
            return Some(entry.clone());
        }
        match self.store.get_session_start_preamble(session) {
            Ok(Some(value)) => match serde_json::from_str(&value) {
                Ok(entry) => Some(entry),
                Err(error) => {
                    tracing::error!(%session, %error, "invalid durable SessionStart state");
                    None
                }
            },
            Ok(None) => None,
            Err(error) => {
                tracing::error!(%session, %error, "cannot read durable SessionStart state");
                None
            }
        }
    }

    fn put(&mut self, session: Uuid, entry: Option<Entry>) -> bool {
        let value = entry
            .as_ref()
            .map(|entry| serde_json::to_string(entry).expect("serializable preamble state"));
        if let Err(error) = self
            .store
            .set_session_start_preamble(session, value.as_deref())
        {
            tracing::error!(%session, %error, "cannot persist SessionStart state; retain debt");
            return false;
        }
        self.registry.cache(session, entry)
    }
}

async fn with_store<T>(
    store: &Arc<rsid_core::store_lock::Mutex<crate::store::Store>>,
    action: impl FnOnce(&mut dyn StateAccess) -> T,
) -> T {
    let store = store.lock().await;
    let mut registry = registry();
    action(&mut LockedRegistry {
        registry: &mut registry,
        store: &store,
    })
}

fn registry() -> std::sync::MutexGuard<'static, Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Bind to the daemon's existing Store before launch/recovery admission.
/// The registry never opens a database connection or accesses a table directly.
pub fn initialize(store: &Arc<rsid_core::store_lock::Mutex<crate::store::Store>>) {
    *registry() = Registry::persistent(store);
}

/// Whether `text` can ride a `SessionStart` hook: non-blank and within the
/// provider's `additionalContext` cap.
#[must_use]
pub fn fits(text: &str) -> bool {
    !text.trim().is_empty() && text.chars().count() <= MAX_SESSION_START_CONTEXT_CHARS
}

/// Launch side: this launch left `text` out of its prompt, so the session's
/// `SessionStart` hook owes it. Replaces any earlier entry (a fresh start).
pub fn defer(session_id: Uuid, text: String) -> bool {
    defer_chunks(session_id, text.clone(), vec![text])
}

fn defer_chunks(session_id: Uuid, text: String, chunks: Vec<String>) -> bool {
    defer_chunks_in(&mut *registry(), session_id, text, chunks)
}

fn defer_chunks_in(
    registry: &mut dyn StateAccess,
    session_id: Uuid,
    text: String,
    chunks: Vec<String>,
) -> bool {
    registry.put(
        session_id,
        Some(Entry {
            claimed: vec![false; chunks.len()],
            confirmed: vec![false; chunks.len()],
            evidence: vec![None; chunks.len()],
            text,
            chunks,
            batch: Uuid::new_v4(),
            source: None,
            delivery: Delivery::Owed,
        }),
    )
}

/// A prompt carrying the preamble ends the debt; clear its session-owned payload.
pub fn forget(session_id: Uuid) {
    registry().put(session_id, None);
}

/// Last resort for the next launch. Consume only after the durable write.
#[must_use]
pub fn take_undelivered(session_id: Uuid) -> Option<String> {
    let mut registry = registry();
    let entry = registry.get(session_id)?;
    if entry.delivery != Delivery::Owed {
        return None;
    }
    registry.put(session_id, None).then_some(entry.text)
}

/// Split only between existing startup parts. Labels count toward the cap.
/// A single unlabelled chunk preserves the existing short-preamble path.
#[must_use]
pub fn split_parts(parts: &[String]) -> Option<Vec<String>> {
    let text = parts.join("\n\n");
    if fits(&text) {
        return Some(vec![text]);
    }
    if text.trim().is_empty() {
        return None;
    }
    // At most one chunk per input part; reserving that label width also
    // covers any smaller chunk count, including decimal digit transitions.
    let label_chars = format!("[RSI preamble part {0}/{0}]\n\n", parts.len())
        .chars()
        .count();
    let budget = MAX_SESSION_START_CONTEXT_CHARS.checked_sub(label_chars)?;
    let mut chunks = Vec::new();
    let mut current = Vec::new();
    let mut chars = 0;
    for part in parts {
        let part_chars = part.chars().count();
        if part_chars > budget {
            return None;
        }
        let separator = if current.is_empty() { 0 } else { 2 };
        if chars + separator + part_chars > budget {
            chunks.push(current.join("\n\n"));
            current.clear();
            chars = 0;
        }
        chars += if current.is_empty() { 0 } else { 2 };
        chars += part_chars;
        current.push(part.as_str());
    }
    if !current.is_empty() {
        chunks.push(current.join("\n\n"));
    }
    let total = chunks.len();
    Some(
        chunks
            .into_iter()
            .enumerate()
            .map(|(index, chunk)| format!("[RSI preamble part {}/{total}]\n\n{chunk}", index + 1))
            .collect(),
    )
}

/// Defer a boundary-split Claude preamble. An unsplittable part leaves the
/// launch on its existing system-prompt path.
pub fn defer_parts(session_id: Uuid, parts: &[String]) -> bool {
    let Some(chunks) = split_parts(parts) else {
        forget(session_id);
        return false;
    };
    defer_chunks(session_id, parts.join("\n\n"), chunks)
}

/// Number of handlers, including after a restart or cache eviction.
#[must_use]
pub fn part_count(session_id: Uuid) -> usize {
    registry()
        .get(session_id)
        .map_or(1, |entry| entry.chunks.len())
}

/// One served part, fenced to the batch whose transcript must confirm it.
#[derive(Clone)]
pub struct HookClaim {
    pub text: String,
    pub batch: Uuid,
    pub index: usize,
    pub fallback: bool,
}

/// Hook side: an unnumbered handler is valid only for a single-part preamble.
#[must_use]
pub fn claim_for_hook(session_id: Uuid, source: &str) -> Option<String> {
    claim_part_for_hook(session_id, source, None).map(|claim| claim.text)
}

/// Claim a one-based part independently of handler completion order. Only
/// transcript confirmation completes delivery. Compact/clear start a new batch;
/// unconfirmed delivery remains owed for the next-launch fallback.
#[must_use]
pub fn claim_part_for_hook(
    session_id: Uuid,
    source: &str,
    part: Option<usize>,
) -> Option<HookClaim> {
    claim_part(session_id, source, part, false)
}

fn claim_part(
    session_id: Uuid,
    source: &str,
    part: Option<usize>,
    fallback: bool,
) -> Option<HookClaim> {
    claim_part_in(&mut *registry(), session_id, source, part, fallback)
}

fn claim_part_in(
    registry: &mut dyn StateAccess,
    session_id: Uuid,
    source: &str,
    part: Option<usize>,
    fallback: bool,
) -> Option<HookClaim> {
    let mut entry = registry.get(session_id)?;
    let index = match part {
        Some(part) => part.checked_sub(1)?,
        None if entry.chunks.len() == 1 => 0,
        None => return None,
    };
    if index >= entry.chunks.len() {
        return None;
    }
    let replay = !fallback && matches!(source, "compact" | "clear");
    if replay
        && (entry.source.as_deref() != Some(source) || entry.claimed.iter().all(|claimed| *claimed))
    {
        entry.claimed.fill(false);
        entry.confirmed.fill(false);
        entry.evidence.fill(None);
        entry.batch = Uuid::new_v4();
        entry.delivery = Delivery::Owed;
    } else if entry.delivery == Delivery::Delivered {
        return None;
    }
    if entry.confirmed[index] || (!fallback && entry.claimed[index]) {
        return None;
    }
    if !fallback {
        entry.source = Some(source.to_string());
    }
    entry.claimed[index] = true;
    let claim = HookClaim {
        text: entry.chunks[index].clone(),
        batch: entry.batch,
        index,
        fallback,
    };
    registry.put(session_id, Some(entry)).then_some(claim)
}

/// Durable transcript watermark captured before the hook reply is returned.
fn remember_evidence_in(
    registry: &mut dyn StateAccess,
    session_id: Uuid,
    claim: &HookClaim,
    params: &RecordProviderLifecycleEventParams,
    cursor: u64,
) -> bool {
    let Some(mut entry) = registry.get(session_id) else {
        return false;
    };
    if entry.batch != claim.batch {
        return false;
    }
    entry.evidence[claim.index] = Some(Evidence {
        params: params.clone(),
        cursor,
    });
    registry.put(session_id, Some(entry))
}

/// Reconcile earlier hook replies synchronously at UserPromptSubmit. Also
/// recovers the watchers' evidence locators after a daemon restart.
fn pending_evidence_in(
    registry: &dyn StateAccess,
    session_id: Uuid,
) -> Vec<(HookClaim, RecordProviderLifecycleEventParams, u64)> {
    let Some(entry) = registry.get(session_id) else {
        return Vec::new();
    };
    entry
        .evidence
        .iter()
        .enumerate()
        .filter_map(|(index, evidence)| {
            let evidence = evidence.as_ref()?;
            (!entry.confirmed[index]).then(|| {
                (
                    HookClaim {
                        text: entry.chunks[index].clone(),
                        batch: entry.batch,
                        index,
                        fallback: evidence.params.session_start_fallback,
                    },
                    evidence.params.clone(),
                    evidence.cursor,
                )
            })
        })
        .collect()
}

/// Hook operations await Store admission; synchronous provider launch code
/// retains its prompt instead when Store is fenced or busy.
pub(crate) async fn claim_for_lifecycle(
    store: &Arc<rsid_core::store_lock::Mutex<crate::store::Store>>,
    session_id: Uuid,
    params: &RecordProviderLifecycleEventParams,
    cursor: u64,
) -> Option<HookClaim> {
    with_store(store, |registry| {
        let claim = claim_part_in(
            registry,
            session_id,
            params.source.as_deref().unwrap_or("other"),
            params.session_start_part,
            params.session_start_fallback,
        )?;
        remember_evidence_in(registry, session_id, &claim, params, cursor).then_some(claim)
    })
    .await
}

pub(crate) async fn pending_evidence_with_store(
    store: &Arc<rsid_core::store_lock::Mutex<crate::store::Store>>,
    session_id: Uuid,
) -> Vec<(HookClaim, RecordProviderLifecycleEventParams, u64)> {
    with_store(store, |registry| pending_evidence_in(registry, session_id)).await
}

pub(crate) async fn confirm_part_with_store(
    store: &Arc<rsid_core::store_lock::Mutex<crate::store::Store>>,
    session_id: Uuid,
    claim: &HookClaim,
) -> Option<(usize, usize)> {
    with_store(store, |registry| {
        confirm_part_in(registry, session_id, claim)
    })
    .await
}

/// Confirm one served part from transcript evidence. Stale watchers cannot
/// complete a replacement launch or compact/clear batch. Only the final new
/// confirmation returns the summary for the single visible delivery row.
pub fn confirm_part(session_id: Uuid, claim: &HookClaim) -> Option<(usize, usize)> {
    confirm_part_in(&mut *registry(), session_id, claim)
}

fn confirm_part_in(
    registry: &mut dyn StateAccess,
    session_id: Uuid,
    claim: &HookClaim,
) -> Option<(usize, usize)> {
    let mut entry = registry.get(session_id)?;
    if entry.batch != claim.batch
        || entry.delivery == Delivery::Delivered
        || !entry.claimed.get(claim.index).copied().unwrap_or(false)
    {
        return None;
    }
    entry.confirmed[claim.index] = true;
    let complete = entry.confirmed.iter().all(|confirmed| *confirmed);
    if complete {
        entry.delivery = Delivery::Delivered;
    }
    let summary = (entry.text.chars().count(), entry.chunks.len());
    if registry.put(session_id, Some(entry)) && complete {
        Some(summary)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn daemon_restart_preserves_owed_parts_confirmation_and_batch_fences() {
        // Close and reopen the canonical Store between boots; the registry has no connection.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("restart.db");
        let session = Uuid::new_v4();
        {
            let store = crate::store::Store::open_test_fixture(&path).unwrap();
            store
                .insert_session(&rsid_store::test_support::test_session(
                    session,
                    directory.path().to_path_buf(),
                ))
                .unwrap();
        }
        let parts = vec!["A".repeat(6000), "B".repeat(6000)];
        let chunks = split_parts(&parts).unwrap();
        {
            let store = Arc::new(rsid_core::store_lock::Mutex::new(
                crate::store::Store::open_test_fixture(&path).unwrap(),
            ));
            let mut boot = Registry::persistent(&store);
            assert!(defer_chunks_in(
                &mut boot,
                session,
                parts.join("\n\n"),
                chunks.clone()
            ));
        }
        let stale;
        let compact;
        let clear;
        {
            let store = Arc::new(rsid_core::store_lock::Mutex::new(
                crate::store::Store::open_test_fixture(&path).unwrap(),
            ));
            let mut boot = Registry::persistent(&store);
            let first = claim_part_in(&mut boot, session, "startup", Some(1), false).unwrap();
            assert_eq!(first.text, chunks[0]);
            assert_eq!(confirm_part_in(&mut boot, session, &first), None);
            stale = claim_part_in(&mut boot, session, "startup", Some(2), false).unwrap();
            // A served reply and its pre-reply watermark survive the restart too.
            let mut entry = boot.get(session).unwrap();
            let params = RecordProviderLifecycleEventParams::from_hook_input(&serde_json::json!({
                "hook_event_name":"SessionStart", "source":"startup",
                "session_id":"thread", "transcript_path":"/thread.jsonl"
            }))
            .unwrap();
            entry.evidence[1] = Some(Evidence { params, cursor: 73 });
            assert!(boot.put(session, Some(entry)));
            boot.entries.clear();
            boot.order.clear();
            assert_eq!(
                boot.get(session).unwrap().chunks,
                chunks,
                "cache eviction preserves debt"
            );
        }
        {
            let store = Arc::new(rsid_core::store_lock::Mutex::new(
                crate::store::Store::open_test_fixture(&path).unwrap(),
            ));
            let mut boot = Registry::persistent(&store);
            let owed = boot.get(session).unwrap();
            assert_eq!(owed.confirmed, vec![true, false]);
            assert_eq!(owed.evidence[1].as_ref().unwrap().cursor, 73);
            assert_eq!(owed.batch, stale.batch);
            assert!(claim_part_in(&mut boot, session, "other", Some(1), true).is_none());
            let recovered = claim_part_in(&mut boot, session, "other", Some(2), true).unwrap();
            assert_eq!(recovered.text, chunks[1]);
            assert_eq!(
                confirm_part_in(&mut boot, session, &recovered),
                Some((12002, 2))
            );
        }
        {
            let store = Arc::new(rsid_core::store_lock::Mutex::new(
                crate::store::Store::open_test_fixture(&path).unwrap(),
            ));
            let mut boot = Registry::persistent(&store);
            assert_eq!(boot.get(session).unwrap().delivery, Delivery::Delivered);
            assert!(claim_part_in(&mut boot, session, "resume", Some(2), false).is_none());
            compact = claim_part_in(&mut boot, session, "compact", Some(1), false).unwrap();
            assert_ne!(compact.batch, stale.batch);
            assert_eq!(confirm_part_in(&mut boot, session, &stale), None);
        }
        {
            let store = Arc::new(rsid_core::store_lock::Mutex::new(
                crate::store::Store::open_test_fixture(&path).unwrap(),
            ));
            let mut boot = Registry::persistent(&store);
            assert_eq!(boot.get(session).unwrap().batch, compact.batch);
            clear = claim_part_in(&mut boot, session, "clear", Some(1), false).unwrap();
            assert_ne!(clear.batch, compact.batch);
            assert_eq!(confirm_part_in(&mut boot, session, &clear), None);
        }
        {
            let store = Arc::new(rsid_core::store_lock::Mutex::new(
                crate::store::Store::open_test_fixture(&path).unwrap(),
            ));
            let mut boot = Registry::persistent(&store);
            assert_eq!(boot.get(session).unwrap().batch, clear.batch);
            assert_eq!(boot.get(session).unwrap().confirmed, vec![true, false]);
            assert_eq!(confirm_part_in(&mut boot, session, &compact), None);
            let missing = claim_part_in(&mut boot, session, "other", Some(2), true).unwrap();
            assert_eq!(
                confirm_part_in(&mut boot, session, &missing),
                Some((12002, 2))
            );
            assert!(boot.put(session, None));
        }
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            crate::store::Store::open_test_fixture(&path).unwrap(),
        ));
        let boot = Registry::persistent(&store);
        assert!(
            boot.get(session).is_none(),
            "consumed session debt stays empty after restart"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn failed_durable_write_keeps_the_prompt_and_existing_debt() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("failed-write.db");
        let store = crate::store::Store::open_test_fixture(&path).unwrap();
        let session = Uuid::new_v4();
        store
            .insert_session(&rsid_store::test_support::test_session(
                session,
                directory.path().to_path_buf(),
            ))
            .unwrap();
        let store = Arc::new(rsid_core::store_lock::Mutex::new(store));
        let mut boot = Registry::persistent(&store);
        assert!(defer_chunks_in(
            &mut boot,
            session,
            "PREAMBLE".into(),
            vec!["PREAMBLE".into()]
        ));
        // A seat fence or another Store user must neither deadlock a launch
        // nor acknowledge a write that never happened.
        let guard = store.try_lock().unwrap();
        assert!(!defer_chunks_in(
            &mut boot,
            session,
            "NEW".into(),
            vec!["NEW".into()]
        ));
        assert!(!boot.put(session, None));
        drop(guard);
        boot.entries.clear();
        assert_eq!(boot.get(session).unwrap().text, "PREAMBLE");
        assert_eq!(boot.get(session).unwrap().delivery, Delivery::Owed);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn many_sessions_leave_daemon_settings_unchanged() {
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            crate::store::Store::open_in_memory().unwrap(),
        ));
        store
            .try_lock()
            .unwrap()
            .set_daemon_setting("operator-test", "preserved")
            .unwrap();
        let before = store
            .try_lock()
            .unwrap()
            .list_daemon_settings_with_prefix("")
            .unwrap();
        let mut registry = Registry::persistent(&store);
        for _ in 0..MAX_TRACKED_SESSIONS + 1 {
            let session = Uuid::new_v4();
            store
                .try_lock()
                .unwrap()
                .insert_session(&rsid_store::test_support::test_session(
                    session,
                    std::env::temp_dir(),
                ))
                .unwrap();
            assert!(defer_chunks_in(
                &mut registry,
                session,
                "PREAMBLE".into(),
                vec!["PREAMBLE".into()]
            ));
            let startup = claim_part_in(&mut registry, session, "startup", None, false).unwrap();
            assert_eq!(
                confirm_part_in(&mut registry, session, &startup),
                Some((8, 1))
            );
            let clear = claim_part_in(&mut registry, session, "clear", None, false).unwrap();
            assert_ne!(clear.batch, startup.batch);
            let fallback = claim_part_in(&mut registry, session, "other", None, true).unwrap();
            assert_eq!(
                confirm_part_in(&mut registry, session, &fallback),
                Some((8, 1))
            );
            assert!(registry.put(session, None));
            assert!(registry.get(session).is_none());
        }
        assert_eq!(
            store
                .try_lock()
                .unwrap()
                .list_daemon_settings_with_prefix("")
                .unwrap(),
            before
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn hook_waits_for_shared_store_and_commits_before_serving() {
        let store = crate::store::Store::open_in_memory().unwrap();
        let session = Uuid::new_v4();
        store
            .insert_session(&rsid_store::test_support::test_session(
                session,
                std::env::temp_dir(),
            ))
            .unwrap();
        let store = Arc::new(rsid_core::store_lock::Mutex::new(store));
        let mut boot = Registry::persistent(&store);
        assert!(defer_chunks_in(
            &mut boot,
            session,
            "PREAMBLE".into(),
            vec!["PREAMBLE".into()]
        ));
        let params = RecordProviderLifecycleEventParams::from_hook_input(&serde_json::json!({
            "hook_event_name": "SessionStart", "source": "startup",
        }))
        .unwrap();
        let guard = store.lock().await;
        let claim = claim_for_lifecycle(&store, session, &params, 73);
        tokio::pin!(claim);
        tokio::select! {
            biased;
            _ = &mut claim => panic!("hook must wait for Store admission"),
            _ = tokio::task::yield_now() => {}
        }
        drop(guard);
        let claim = tokio::time::timeout(std::time::Duration::from_secs(1), claim)
            .await
            .unwrap()
            .unwrap();
        boot.entries.clear();
        let state = boot.get(session).unwrap();
        assert_eq!(state.claimed, vec![true]);
        assert_eq!(state.evidence[0].as_ref().unwrap().cursor, 73);
        assert_eq!(claim.text, "PREAMBLE");
        assert_eq!(
            confirm_part_with_store(&store, session, &claim).await,
            Some((8, 1))
        );
        assert_eq!(boot.get(session).unwrap().delivery, Delivery::Delivered);
        forget(session);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn chunks_keep_whole_parts_and_include_labels_within_the_character_cap() {
        let parts = vec![
            "é".repeat(6000),
            "B".repeat(5000),
            "C".repeat(100),
            "D".repeat(5000),
        ];
        let chunks = split_parts(&parts).unwrap();
        assert_eq!(chunks.len(), 3);
        let bodies: Vec<_> = chunks
            .iter()
            .enumerate()
            .map(|(index, chunk)| {
                assert!(chunk.chars().count() <= MAX_SESSION_START_CONTEXT_CHARS);
                chunk
                    .strip_prefix(&format!("[RSI preamble part {}/3]\n\n", index + 1))
                    .unwrap()
            })
            .collect();
        assert_eq!(
            bodies,
            vec![
                parts[0].clone(),
                format!("{}\n\n{}", parts[1], parts[2]),
                parts[3].clone()
            ]
        );
        assert_eq!(bodies.join("\n\n"), parts.join("\n\n"));
        let many = split_parts(&vec!["x".repeat(9900); 11]).unwrap();
        assert_eq!(many.len(), 11);
        assert!(many[10].starts_with("[RSI preamble part 11/11]\n\n"));
        assert!(many.iter().all(|chunk| fits(chunk)));
        let budget =
            MAX_SESSION_START_CONTEXT_CHARS - "[RSI preamble part 1/2]\n\n".chars().count();
        let exact = split_parts(&["é".repeat(budget), "B".repeat(100)]).unwrap();
        assert_eq!(exact[0].chars().count(), MAX_SESSION_START_CONTEXT_CHARS);
        assert_eq!(
            split_parts(&["é".repeat(budget + 1), "B".repeat(100)]),
            None
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn an_indivisible_oversize_part_uses_the_prompt_fallback() {
        let session = Uuid::new_v4();
        let too_long = vec!["x".repeat(MAX_SESSION_START_CONTEXT_CHARS + 1)];
        assert_eq!(split_parts(&too_long), None);
        assert!(!defer_parts(session, &too_long));
        assert_eq!(claim_for_hook(session, "startup"), None);
        // A part that fits by itself still needs room for its multipart label.
        assert_eq!(
            split_parts(&["x".repeat(MAX_SESSION_START_CONTEXT_CHARS), "y".into()]),
            None
        );
        assert_eq!(
            split_parts(&["x".repeat(MAX_SESSION_START_CONTEXT_CHARS)])
                .unwrap()
                .len(),
            1
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn partial_claims_remain_owed_and_all_parts_complete_in_any_order() {
        let parts = vec!["A".repeat(6000), "B".repeat(6000), "C".repeat(6000)];
        let session = Uuid::new_v4();
        assert!(defer_parts(session, &parts));
        assert_eq!(part_count(session), 3);
        assert!(claim_part_for_hook(session, "startup", None).is_none());
        assert!(claim_part_for_hook(session, "startup", Some(0)).is_none());
        assert!(claim_part_for_hook(session, "startup", Some(4)).is_none());
        let stale = claim_part_for_hook(session, "startup", Some(2)).unwrap();
        assert!(confirm_part(session, &stale).is_none());
        assert!(claim_part_for_hook(session, "startup", Some(2)).is_none());
        assert_eq!(take_undelivered(session), Some(parts.join("\n\n")));
        assert!(defer_parts(session, &parts));
        assert!(confirm_part(session, &stale).is_none());
        for (part, complete) in [(3, false), (1, false), (2, true)] {
            let claim = claim_part_for_hook(session, "startup", Some(part)).unwrap();
            assert_eq!(confirm_part(session, &claim).is_some(), complete);
        }
        assert_eq!(take_undelivered(session), None);
        assert!(claim_part_for_hook(session, "resume", Some(1)).is_none());
        for source in ["compact", "compact", "clear"] {
            for (part, complete) in [(2, false), (1, false), (3, true)] {
                let claim = claim_part_for_hook(session, source, Some(part)).unwrap();
                assert_eq!(confirm_part(session, &claim).is_some(), complete);
                assert!(confirm_part(session, &claim).is_none());
            }
        }
        forget(session);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn late_evidence_cannot_complete_a_compaction_batch() {
        let session = Uuid::new_v4();
        assert!(defer_parts(session, &["A".repeat(6000), "B".repeat(6000)]));
        let old_one = claim_part_for_hook(session, "startup", Some(1)).unwrap();
        let old_two = claim_part_for_hook(session, "startup", Some(2)).unwrap();
        let new_one = claim_part_for_hook(session, "compact", Some(1)).unwrap();
        assert_eq!(confirm_part(session, &old_one), None);
        assert_eq!(confirm_part(session, &old_two), None);
        let new_two = claim_part_for_hook(session, "compact", Some(2)).unwrap();
        assert_eq!(confirm_part(session, &new_two), None);
        assert_eq!(confirm_part(session, &new_one), Some((12002, 2)));
        assert_eq!(take_undelivered(session), None);
        forget(session);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn an_owed_preamble_is_delivered_once_and_again_only_after_compaction() {
        let session = Uuid::new_v4();
        assert_eq!(claim_for_hook(session, "startup"), None);
        defer(session, "PREAMBLE".to_string());
        assert_eq!(
            claim_for_hook(session, "startup").as_deref(),
            Some("PREAMBLE")
        );
        assert_eq!(claim_for_hook(session, "resume"), None);
        assert_eq!(claim_for_hook(session, "startup"), None);
        let claim = claim_part_for_hook(session, "compact", None).unwrap();
        assert_eq!(claim.text, "PREAMBLE");
        assert_eq!(confirm_part(session, &claim), Some((8, 1)));
        assert_eq!(take_undelivered(session), None, "confirmed delivery");
        forget(session);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn an_undelivered_preamble_falls_back_once_and_the_hook_then_finds_nothing() {
        let session = Uuid::new_v4();
        defer(session, "PREAMBLE".to_string());
        assert_eq!(take_undelivered(session).as_deref(), Some("PREAMBLE"));
        assert_eq!(take_undelivered(session), None);
        assert_eq!(claim_for_hook(session, "resume"), None);
        assert_eq!(claim_for_hook(session, "compact"), None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn the_fit_follows_the_provider_cap() {
        assert!(fits("x"));
        assert!(fits(&"é".repeat(MAX_SESSION_START_CONTEXT_CHARS)));
        assert!(!fits(&"x".repeat(MAX_SESSION_START_CONTEXT_CHARS + 1)));
        assert!(!fits(" \n "));
    }
}
