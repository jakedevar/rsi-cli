//! #2766 (#2748 G15): the daemon side of the provider lifecycle hooks.
//!
//! A session's own lifecycle hook (`StopFailure`, `PreCompact`, installed only
//! when the operator setting `provider_lifecycle_hooks` names the event) calls
//! the hook-only verb `RecordProviderLifecycleEvent`. The daemon writes one
//! visible `System` row carrying `metadata.provider_health` (`kind`
//! `provider_stop_failure` or `compaction_imminent`), so the TUI shows it even
//! with system rows hidden and the baton/handoff logic can read it from the
//! transcript.
//!
//! The monitor owns the session's transcript sequence counter, so a row for a
//! session with a live monitor is queued on the same inbox the boundary-mail
//! operator deliveries use and written by the monitor; with no live monitor
//! there is no racing writer and the row is written here. Nothing in this path
//! can fail the hook: a row that cannot be written is logged and dropped.
//!
//! #2898: a `PreCompact` event also drives the handoff the context-cap logic
//! already uses for the session's role ([`request_compaction_handoff`]): a
//! session holding no coordinating seat passes the #1254 worker baton (one
//! durable mail with the baton protocol, one `worker_context_cap` notice for
//! its launcher, `trigger: pre_compact`); a coordinating seat records its #1005
//! context-cap rotation, which the idle-boundary pass carries out with a
//! daemon-written handoff. Both are one durable record per session, so a
//! repeated event (or several compactions) never sends a second baton or a
//! second rotation. A worker whose own cap the operator turned off (an
//! explicit zero, #2901) gets no baton, no notice and an empty handoff. The reply carries the instruction the hook prints for the
//! provider before it compacts. A failure is logged and leaves `handoff`
//! empty: it never fails the call, and the hook never blocks compaction.
//!
//! hub #2919: a `SessionStart` event claims the preamble the session's launch
//! deferred to the hook ([`crate::session_start_preamble`]); the reply carries it
//! as `additional_context`, which the hook prints as the provider's
//! `additionalContext`, and the row records the delivery. Nothing deferred (a
//! resumed turn, the default setting) writes no row and replies `skipped`.

use std::collections::HashMap;
use std::sync::Arc;

use rsi_common::provider_lifecycle_hooks::{
    COMPACTION_HANDOFF_SEAT_SUCCESSION, COMPACTION_HANDOFF_WORKER_BATON, LifecycleHookEvent,
    RecordProviderLifecycleEventParams,
};
use rsi_common::types::{ConversationEvent, EventType, PROVIDER_HEALTH_METADATA_KEY, Session};
use serde::Serialize;
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

use super::types::TrackedSession;
use crate::store::Store;

/// The reply to `RecordProviderLifecycleEvent`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecordProviderLifecycleEventResponse {
    /// `queued` (the live monitor writes it), `written`, `dropped`, or
    /// `skipped` (a `SessionStart` with no preamble owed: no row).
    pub outcome: &'static str,
    /// #2898: the handoff a `PreCompact` drove, when one applies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handoff: Option<CompactionHandoff>,
    /// hub #2919: the preamble a `SessionStart` hook delivers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub additional_context: Option<String>,
}

/// #2898: what the daemon did about an imminent compaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CompactionHandoff {
    /// `worker_baton` or `seat_succession`.
    pub action: &'static str,
    /// `true` when this event made the request; `false` when an earlier
    /// trigger (a cap crossing, an earlier compaction) already holds it, or
    /// when it was deferred.
    pub requested: bool,
    /// Why a seat succession was not requested now.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deferred: Option<&'static str>,
    /// The handoff instruction the hook gives the provider before it compacts.
    pub instruction: String,
}

/// The live facts of the compacting session.
pub(crate) struct CompactionSubject {
    pub session: Session,
    pub measured_tokens: u64,
    pub context_window: u64,
    /// The session's own automatic-rotation switch.
    pub rotation_enabled: bool,
}

const PRE_COMPACT_SEAT_PREFIX: &str = "[rsi daemon, #2898 PreCompact] Your provider is compacting the context of this coordinating seat.";

/// #2898: drive the role's existing handoff for an imminent compaction of
/// `subject`. `None` when nothing applies (a session with neither a seat nor
/// a launcher, as the context cap does).
pub(crate) fn request_compaction_handoff(
    store: &Store,
    runtime_config: &crate::config::RuntimeConfig,
    subject: &CompactionSubject,
    now: chrono::DateTime<chrono::Utc>,
) -> crate::error::Result<Option<CompactionHandoff>> {
    if let Some(seat) = super::context_cap::capped_seat(store, subject.session.id)? {
        return seat_succession(store, runtime_config, subject, seat, now).map(Some);
    }
    worker_baton(store, runtime_config, subject, now)
}

fn worker_baton(
    store: &Store,
    runtime_config: &crate::config::RuntimeConfig,
    subject: &CompactionSubject,
    now: chrono::DateTime<chrono::Utc>,
) -> crate::error::Result<Option<CompactionHandoff>> {
    use crate::store::worker_baton::{WorkerBatonOutcome, baton_steps};
    let session = &subject.session;
    // #2901: the operator's explicit zero (global, provider or model) turns
    // the worker baton off, so a PreCompact passes none. A `None` cap that is
    // only a percentage of an unknown window is not a disable.
    if runtime_config.worker_context_cap_disabled(session.provider, session.model.as_deref()) {
        tracing::info!(session_id = %session.id, "PreCompact baton skipped: worker context cap is off");
        return Ok(None);
    }
    let cap = runtime_config
        .worker_context_cap(
            session.provider,
            session.model.as_deref(),
            subject.context_window,
        )
        .unwrap_or(subject.measured_tokens);
    let outcome =
        store.record_worker_pre_compact_baton(session.id, subject.measured_tokens, cap, now)?;
    if outcome == WorkerBatonOutcome::NoLauncher {
        return Ok(None);
    }
    let Some(record) = store.worker_baton(session.id)? else {
        return Ok(None);
    };
    Ok(Some(CompactionHandoff {
        action: COMPACTION_HANDOFF_WORKER_BATON,
        requested: outcome == WorkerBatonOutcome::Sent,
        deferred: None,
        instruction: format!(
            "[rsi daemon, #2898 baton] Your provider is compacting your context (PreCompact hook). Pass the baton now and start no new work:\n{}",
            baton_steps(&record)
        ),
    }))
}

fn seat_succession(
    store: &Store,
    runtime_config: &crate::config::RuntimeConfig,
    subject: &CompactionSubject,
    seat: rsi_common::daemon_handoff::CoordinatorSeatV1,
    now: chrono::DateTime<chrono::Utc>,
) -> crate::error::Result<CompactionHandoff> {
    let session = &subject.session;
    let rotation_on = subject.rotation_enabled
        && runtime_config
            .context_rotation_enabled
            .load(std::sync::atomic::Ordering::Relaxed);
    // The cap rotation clears a record whose cap is off, so none is written.
    let cap = rotation_on
        .then(|| {
            runtime_config.coordinator_context_cap(
                session.provider,
                session.model.as_deref(),
                subject.context_window,
            )
        })
        .flatten();
    let Some(cap) = cap else {
        return Ok(deferred_seat("cap_rotation_off"));
    };
    if let Some(reason) = store.automatic_rotation_protection(session.id)? {
        return Ok(deferred_seat(reason));
    }
    let requested = super::context_cap::record_cap_crossing(
        store,
        session.id,
        seat,
        subject.measured_tokens,
        cap,
        now,
    )?;
    Ok(CompactionHandoff {
        action: COMPACTION_HANDOFF_SEAT_SUCCESSION,
        requested,
        deferred: None,
        instruction: format!(
            "{PRE_COMPACT_SEAT_PREFIX} RSI requested this seat's context-cap succession: the daemon rotates the seat at its next idle boundary and writes the successor's handoff from daemon state. Keep the durable state current (Issues, ledger, wakes, reports up), start no new long work, and end your turn once the current step is done."
        ),
    })
}

fn deferred_seat(reason: &'static str) -> CompactionHandoff {
    CompactionHandoff {
        action: COMPACTION_HANDOFF_SEAT_SUCCESSION,
        requested: false,
        deferred: Some(reason),
        instruction: format!(
            "{PRE_COMPACT_SEAT_PREFIX} RSI did not request a succession now ({reason}). Keep the durable state current (Issues, ledger, wakes, reports up) so the next succession loses nothing."
        ),
    }
}

/// The compacting session's live facts: the tracked copy when a monitor
/// runs, else the stored row with no live measurement.
async fn compaction_subject(
    store: &Arc<Mutex<Store>>,
    active: &Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    session_id: Uuid,
) -> crate::error::Result<Option<CompactionSubject>> {
    if let Some(tracked) = active.read().await.get(&session_id) {
        let (measured_tokens, _, _, _, context_window) =
            super::monitor::live_context_state(tracked);
        return Ok(Some(CompactionSubject {
            session: tracked.session.clone(),
            measured_tokens,
            context_window,
            rotation_enabled: tracked.rotation.is_enabled(),
        }));
    }
    let Some(session) = store.lock().await.get_session(session_id)? else {
        return Ok(None);
    };
    let context_window =
        crate::provider_capabilities::resolved_context_budget_for_session(&session).active_tokens;
    Ok(Some(CompactionSubject {
        rotation_enabled: session.rotation_disabled_at.is_none(),
        session,
        measured_tokens: 0,
        context_window,
    }))
}

/// [`request_compaction_handoff`] for a `PreCompact`; any failure is logged
/// and yields no handoff (never an error to the hook).
async fn compaction_handoff(
    store: &Arc<Mutex<Store>>,
    active: &Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    runtime_config: &crate::config::RuntimeConfig,
    session_id: Uuid,
) -> Option<CompactionHandoff> {
    let subject = match compaction_subject(store, active, session_id).await {
        Ok(Some(subject)) => subject,
        Ok(None) => return None,
        Err(error) => {
            tracing::warn!(%session_id, %error, "PreCompact handoff skipped: session unreadable");
            return None;
        }
    };
    let decided = request_compaction_handoff(
        &*store.lock().await,
        runtime_config,
        &subject,
        chrono::Utc::now(),
    );
    match decided {
        Ok(handoff) => {
            if let Some(handoff) = &handoff {
                tracing::info!(%session_id, action = handoff.action, requested = handoff.requested, "PreCompact drove the session's handoff");
            }
            handoff
        }
        Err(error) => {
            tracing::warn!(%session_id, %error, "PreCompact handoff request failed; compaction proceeds");
            None
        }
    }
}

/// The transcript row for one lifecycle event, before it has a sequence.
#[must_use]
pub(crate) fn lifecycle_row(
    session_id: Uuid,
    params: &RecordProviderLifecycleEventParams,
) -> ConversationEvent {
    ConversationEvent {
        id: 0,
        session_id,
        sequence: 0,
        event_type: EventType::System,
        role: None,
        content: params.summary(),
        tool_name: None,
        tool_input: None,
        created_at: chrono::Utc::now(),
        offload_id: None,
        tool_use_id: None,
        metadata: Some(Box::new(serde_json::json!({
            PROVIDER_HEALTH_METADATA_KEY: params.health_metadata()
        }))),
    }
}

/// Record one lifecycle event for `session_id` (the token-resolved caller).
pub(crate) async fn record_provider_lifecycle_event(
    store: &Arc<Mutex<Store>>,
    active: &Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    event_bus: &Arc<crate::bus::EventBus>,
    runtime_config: &crate::config::RuntimeConfig,
    session_id: Uuid,
    params: RecordProviderLifecycleEventParams,
) -> RecordProviderLifecycleEventResponse {
    let params = params.normalized();
    if params.event == LifecycleHookEvent::SessionStart {
        return session_start(store, active, event_bus, session_id, &params).await;
    }
    let row = lifecycle_row(session_id, &params);
    let outcome = record_row(store, active, event_bus, session_id, row).await;
    if params.event != LifecycleHookEvent::PreCompact {
        return RecordProviderLifecycleEventResponse {
            outcome,
            handoff: None,
            additional_context: None,
        };
    }
    RecordProviderLifecycleEventResponse {
        outcome,
        handoff: compaction_handoff(store, active, runtime_config, session_id).await,
        additional_context: None,
    }
}

/// hub #2919: deliver the preamble the session's launch deferred to its
/// `SessionStart` hook, with one visible row naming the delivery.
async fn session_start(
    store: &Arc<Mutex<Store>>,
    active: &Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    event_bus: &Arc<crate::bus::EventBus>,
    session_id: Uuid,
    params: &RecordProviderLifecycleEventParams,
) -> RecordProviderLifecycleEventResponse {
    let source = params.source.as_deref().unwrap_or("other");
    let Some(preamble) = crate::session_start_preamble::claim_for_hook(session_id, source) else {
        return RecordProviderLifecycleEventResponse {
            outcome: "skipped",
            handoff: None,
            additional_context: None,
        };
    };
    let mut row = lifecycle_row(session_id, params);
    if let Some(health) = row
        .metadata
        .as_deref_mut()
        .and_then(|metadata| metadata.get_mut(PROVIDER_HEALTH_METADATA_KEY))
        .and_then(serde_json::Value::as_object_mut)
    {
        health.insert(
            "preamble_chars".to_string(),
            serde_json::json!(preamble.chars().count()),
        );
    }
    tracing::info!(%session_id, source, chars = preamble.chars().count(), "SessionStart hook delivers the preamble");
    RecordProviderLifecycleEventResponse {
        outcome: record_row(store, active, event_bus, session_id, row).await,
        handoff: None,
        additional_context: Some(preamble),
    }
}

async fn record_row(
    store: &Arc<Mutex<Store>>,
    active: &Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    event_bus: &Arc<crate::bus::EventBus>,
    session_id: Uuid,
    row: ConversationEvent,
) -> &'static str {
    let inbox = active
        .read()
        .await
        .get(&session_id)
        .map(|tracked| tracked.operator_inbox.clone());
    if let Some(inbox) = inbox {
        inbox.push_lifecycle(row);
        return "queued";
    }
    let sequence = super::queries::next_transcript_sequence(store, session_id, &[]).await;
    match write_row(store, event_bus, row, sequence).await {
        Some(_) => "written",
        None => "dropped",
    }
}

/// Monitor side: write each queued lifecycle row with the monitor's own
/// sequence counter. A row that cannot be written keeps its number free.
pub(crate) async fn drain_lifecycle_rows(
    rows: Vec<ConversationEvent>,
    store: &Arc<Mutex<Store>>,
    active: &Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    event_bus: &Arc<crate::bus::EventBus>,
    session_id: Uuid,
    sequence: &mut i32,
) {
    for row in rows {
        let Some(next) = sequence.checked_add(1) else {
            tracing::error!(%session_id, "transcript sequence exhausted; lifecycle row dropped");
            return;
        };
        let Some(event) = write_row(store, event_bus, row, next).await else {
            continue;
        };
        *sequence = next;
        if let Some(tracked) = active.write().await.get_mut(&session_id) {
            tracked.events.push(event);
        }
    }
}

async fn write_row(
    store: &Arc<Mutex<Store>>,
    event_bus: &Arc<crate::bus::EventBus>,
    mut row: ConversationEvent,
    sequence: i32,
) -> Option<ConversationEvent> {
    row.sequence = sequence;
    let session_id = row.session_id;
    match store.lock().await.insert_event(&row) {
        Ok(id) => row.id = id,
        Err(error) => {
            tracing::warn!(%session_id, %error, "provider lifecycle row could not be recorded");
            return None;
        }
    }
    event_bus.publish(crate::bus::DaemonEvent::ConversationEvent {
        session_id,
        event: row.clone(),
    });
    Some(row)
}

#[cfg(test)]
mod tests {
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    use super::*;
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    use crate::session::agent_message_delivery::tests::tracked;
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    use crate::session::agent_verbs::tests::test_session;
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    use rsi_common::provider_lifecycle_hooks::{
        KIND_COMPACTION_IMMINENT, KIND_PROVIDER_STOP_FAILURE, LifecycleHookEvent,
    };

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    fn params(event: &str, extra: serde_json::Value) -> RecordProviderLifecycleEventParams {
        let mut input = serde_json::json!({"hook_event_name": event});
        if let (Some(input), Some(extra)) = (input.as_object_mut(), extra.as_object()) {
            input.extend(extra.clone());
        }
        RecordProviderLifecycleEventParams::from_hook_input(&input).expect("lifecycle input")
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    struct Fixture {
        store: Arc<Mutex<Store>>,
        active: Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
        bus: Arc<crate::bus::EventBus>,
        session: Uuid,
    }

    /// A stored session; `live` also tracks it as running (a live monitor).
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    fn fixture(live: bool) -> Fixture {
        let store = Store::open_in_memory().expect("store");
        let session = Uuid::new_v4();
        let row = test_session(session, std::path::PathBuf::from("/tmp"));
        store.insert_session(&row).expect("session");
        let mut map = HashMap::new();
        if live {
            map.insert(session, tracked(row));
        }
        Fixture {
            store: Arc::new(Mutex::new(store)),
            active: Arc::new(RwLock::new(map)),
            bus: Arc::new(crate::bus::EventBus::new(16)),
            session,
        }
    }

    /// Rotation on, a 500k-token coordinator cap.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    fn runtime() -> Arc<crate::config::RuntimeConfig> {
        use std::sync::atomic::Ordering;
        let config = crate::config::RuntimeConfig::from_config(&crate::config::Config::from_env());
        config
            .context_rotation_enabled
            .store(true, Ordering::Relaxed);
        config
            .coordinator_context_cap_tokens
            .store(500_000, Ordering::Relaxed);
        config
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    async fn stored_kinds(fixture: &Fixture) -> Vec<(i32, String)> {
        fixture
            .store
            .lock()
            .await
            .load_events(fixture.session)
            .expect("events")
            .into_iter()
            .filter_map(|event| {
                let kind = event.metadata.as_ref()?[PROVIDER_HEALTH_METADATA_KEY]["kind"]
                    .as_str()?
                    .to_string();
                Some((event.sequence, kind))
            })
            .collect()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn a_stop_failure_becomes_a_visible_typed_provider_error_row() {
        let session = Uuid::new_v4();
        let row = lifecycle_row(
            session,
            &params(
                "StopFailure",
                serde_json::json!({"error": "overloaded", "error_details": "529"}),
            ),
        );
        assert_eq!(row.session_id, session);
        assert_eq!(row.event_type, EventType::System);
        assert!(row.is_provider_health_notice());
        let health = &row.metadata.as_ref().unwrap()[PROVIDER_HEALTH_METADATA_KEY];
        assert_eq!(health["kind"], KIND_PROVIDER_STOP_FAILURE);
        assert_eq!(health["error"], "overloaded");
        assert_eq!(health["error_details"], "529");
        assert_eq!(health["hook_event"], "StopFailure");
        assert!(row.content.contains("overloaded"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn a_pre_compact_becomes_a_visible_compaction_imminent_row() {
        let row = lifecycle_row(
            Uuid::new_v4(),
            &params("PreCompact", serde_json::json!({"trigger": "auto"})),
        );
        assert!(row.is_provider_health_notice());
        let health = &row.metadata.as_ref().unwrap()[PROVIDER_HEALTH_METADATA_KEY];
        assert_eq!(health["kind"], KIND_COMPACTION_IMMINENT);
        assert_eq!(health["trigger"], "auto");
        assert_eq!(health["hook_event"], LifecycleHookEvent::PreCompact.name());
    }

    /// hub #2919: a `SessionStart` claims the preamble the launch deferred:
    /// the reply carries it, the hook prints it as the provider's
    /// `additionalContext`, and one visible row records the delivery. A later
    /// resume, and a session nothing was deferred for, get nothing and no row.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn a_session_start_delivers_the_deferred_preamble_as_hook_context() {
        use rsi_common::provider_lifecycle_hooks::{
            KIND_SESSION_START_CONTEXT, render_session_start_output,
        };
        let fixture = fixture(false);
        let config = runtime();
        let start = |source: &str| params("SessionStart", serde_json::json!({"source": source}));
        let skipped = record_provider_lifecycle_event(
            &fixture.store,
            &fixture.active,
            &fixture.bus,
            &config,
            fixture.session,
            start("startup"),
        )
        .await;
        assert_eq!(skipped.outcome, "skipped");
        assert_eq!(skipped.additional_context, None);
        assert!(stored_kinds(&fixture).await.is_empty());

        crate::session_start_preamble::defer(fixture.session, "ROUTER\n\nPREAMBLE".to_string());
        let reply = record_provider_lifecycle_event(
            &fixture.store,
            &fixture.active,
            &fixture.bus,
            &config,
            fixture.session,
            start("startup"),
        )
        .await;
        assert_eq!(reply.outcome, "written");
        assert_eq!(
            reply.additional_context.as_deref(),
            Some("ROUTER\n\nPREAMBLE")
        );
        let wire = rsi_common::rpc::RpcResponse::success(
            Some(serde_json::json!(1)),
            serde_json::to_value(&reply).unwrap(),
        );
        let printed: serde_json::Value =
            serde_json::from_str(&render_session_start_output(&wire).expect("hook output"))
                .unwrap();
        assert_eq!(
            printed["hookSpecificOutput"],
            serde_json::json!({
                "hookEventName": "SessionStart",
                "additionalContext": "ROUTER\n\nPREAMBLE",
            })
        );
        let events = fixture
            .store
            .lock()
            .await
            .load_events(fixture.session)
            .unwrap();
        let health = &events[0].metadata.as_ref().unwrap()[PROVIDER_HEALTH_METADATA_KEY];
        assert_eq!(health["kind"], KIND_SESSION_START_CONTEXT);
        assert_eq!(health["session_source"], "startup");
        assert_eq!(health["preamble_chars"], 16);
        assert!(events[0].is_provider_health_notice());

        let resumed = record_provider_lifecycle_event(
            &fixture.store,
            &fixture.active,
            &fixture.bus,
            &config,
            fixture.session,
            start("resume"),
        )
        .await;
        assert_eq!(resumed.outcome, "skipped");
        assert_eq!(resumed.additional_context, None);
        assert_eq!(stored_kinds(&fixture).await.len(), 1);
        crate::session_start_preamble::forget(fixture.session);
    }

    /// With no live monitor the row is written at once after the stored rows.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn without_a_live_monitor_the_row_is_written_and_published() {
        let fixture = fixture(false);
        let mut events = fixture.bus.subscribe();
        let reply = record_provider_lifecycle_event(
            &fixture.store,
            &fixture.active,
            &fixture.bus,
            &runtime(),
            fixture.session,
            params("StopFailure", serde_json::json!({"error": "rate_limit"})),
        )
        .await;
        assert_eq!(reply.outcome, "written");
        assert_eq!(
            stored_kinds(&fixture).await,
            [(0, KIND_PROVIDER_STOP_FAILURE.to_string())]
        );
        assert!(events.try_recv().is_ok(), "the row is published");
    }

    /// With a live monitor the row waits on the session's inbox and is written
    /// with the monitor's own sequence counter when it drains.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn with_a_live_monitor_the_row_is_queued_and_numbered_by_the_monitor() {
        let fixture = fixture(true);
        let reply = record_provider_lifecycle_event(
            &fixture.store,
            &fixture.active,
            &fixture.bus,
            &runtime(),
            fixture.session,
            params("PreCompact", serde_json::json!({"trigger": "auto"})),
        )
        .await;
        assert_eq!(reply.outcome, "queued");
        assert!(stored_kinds(&fixture).await.is_empty(), "not written yet");
        let inbox = fixture
            .active
            .read()
            .await
            .get(&fixture.session)
            .map(|tracked| tracked.operator_inbox.clone())
            .expect("inbox");
        let mut sequence = 41;
        crate::session::boundary_mail::drain_operator_transcript_inbox(
            &inbox,
            &fixture.store,
            &fixture.active,
            &fixture.bus,
            fixture.session,
            &mut sequence,
        )
        .await;
        assert_eq!(sequence, 42);
        assert_eq!(
            stored_kinds(&fixture).await,
            [(42, KIND_COMPACTION_IMMINENT.to_string())]
        );
        let tracked_events = fixture.active.read().await[&fixture.session].events.len();
        assert_eq!(tracked_events, 1);
    }
    /// An Epic in a project, its lead (a coordinating seat) and one worker
    /// child the lead launched; both tracked live (rotation on).
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    struct EpicFixture {
        store: Arc<Mutex<Store>>,
        active: Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
        bus: Arc<crate::bus::EventBus>,
        lead: Uuid,
        worker: Uuid,
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    fn epic_fixture() -> EpicFixture {
        use rsi_common::types::{Project, SessionKind, SessionStatus};
        let store = Store::open_in_memory().expect("store");
        let project = Uuid::new_v4();
        store
            .insert_project(&Project {
                id: project,
                name: format!("precompact {project}"),
                path: None,
                description: None,
                color: Project::DEFAULT_COLOR.into(),
                context_files: None,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            })
            .expect("project");
        let row = |edit: &dyn Fn(&mut Session)| {
            let mut row = test_session(Uuid::new_v4(), std::path::PathBuf::from("/tmp/2898"));
            row.project_id = Some(project);
            row.status = SessionStatus::Running;
            edit(&mut row);
            store.insert_session(&row).expect("session");
            row
        };
        let lead = row(&|_| {});
        let epic = row(&|epic| {
            epic.session_kind = SessionKind::Epic;
            epic.lead_session_id = Some(lead.id);
        });
        let worker = row(&|worker| worker.parent_id = Some(epic.id));
        let mut map = HashMap::new();
        for session in [lead.clone(), worker.clone()] {
            let mut live = tracked(session.clone());
            live.rotation.set_enabled(true);
            map.insert(session.id, live);
        }
        EpicFixture {
            store: Arc::new(Mutex::new(store)),
            active: Arc::new(RwLock::new(map)),
            bus: Arc::new(crate::bus::EventBus::new(16)),
            lead: lead.id,
            worker: worker.id,
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    async fn pre_compact(
        fixture: &EpicFixture,
        config: &crate::config::RuntimeConfig,
        session: Uuid,
    ) -> RecordProviderLifecycleEventResponse {
        record_provider_lifecycle_event(
            &fixture.store,
            &fixture.active,
            &fixture.bus,
            config,
            session,
            params("PreCompact", serde_json::json!({"trigger": "auto"})),
        )
        .await
    }

    /// #290: the PreCompact hook reply and the baton mail carry the one
    /// shared `baton_steps` instruction, not two copies of the text.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn the_hook_reply_and_the_baton_mail_carry_the_same_instruction() {
        use crate::store::worker_baton::baton_steps;
        let fixture = epic_fixture();
        let config = runtime();
        let reply = pre_compact(&fixture, &config, fixture.worker).await;
        let instruction = reply.handoff.expect("a worker handoff").instruction;
        let mut record = fixture
            .store
            .lock()
            .await
            .worker_baton(fixture.worker)
            .expect("read")
            .expect("the baton record");
        let mail = mail_to(&fixture, fixture.worker).await;
        assert_eq!(mail.len(), 1);
        let steps = baton_steps(&record);
        assert!(instruction.contains(&steps), "{instruction}");
        assert!(mail[0].1.contains(&steps), "{}", mail[0].1);

        // Bound to an Issue, the one instruction is the handoff-only request.
        record.launcher.issue_display_number = Some(290);
        let steps = baton_steps(&record);
        assert!(
            steps.contains(
                "`AgentUpdateIssue {display_number: 290, handoff: \"<complete handoff>\", expected_row_version: <any positive>, idempotency_key: \"<unique>\"}` and no other field"
            ),
            "{steps}"
        );
        assert!(steps.contains("your `Friction:` line"), "{steps}");
    }

    /// Agent mail payloads addressed to `target`, with their owner.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    async fn mail_to(fixture: &EpicFixture, target: Uuid) -> Vec<(String, String)> {
        let store = fixture.store.lock().await;
        let mut statement = store
            .conn
            .prepare(
                "SELECT owner_session_id, payload FROM agent_messages WHERE target_session_id=?1",
            )
            .expect("prepare");
        statement
            .query_map([target.to_string()], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("query")
            .collect::<std::result::Result<_, _>>()
            .expect("rows")
    }

    /// #2898 end to end: a worker's PreCompact passes the #1254 baton (one
    /// durable mail with the baton protocol, one `worker_context_cap` notice
    /// to its launcher, trigger `pre_compact`) and the reply carries the
    /// instruction the hook prints. A second event for the same worker sends
    /// nothing new.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn a_worker_pre_compact_passes_the_baton_once_and_returns_the_instruction() {
        use crate::store::worker_baton::{WorkerBatonState, WorkerBatonTrigger};
        let fixture = epic_fixture();
        let config = runtime();
        let reply = pre_compact(&fixture, &config, fixture.worker).await;
        assert_eq!(reply.outcome, "queued", "the notice row is still queued");
        let handoff = reply.handoff.expect("a worker handoff");
        assert_eq!(handoff.action, COMPACTION_HANDOFF_WORKER_BATON);
        assert!(handoff.requested);
        assert!(
            handoff
                .instruction
                .contains("first line is `PIPELINE HANDOFF — BATON <sha>`"),
            "{}",
            handoff.instruction
        );
        assert!(handoff.instruction.contains("Commit your work"));

        let record = fixture
            .store
            .lock()
            .await
            .worker_baton(fixture.worker)
            .expect("read")
            .expect("the baton record");
        assert_eq!(record.state, WorkerBatonState::Sent);
        assert_eq!(record.trigger, WorkerBatonTrigger::PreCompact);
        assert_eq!(record.launcher.launcher, fixture.lead);
        let baton = mail_to(&fixture, fixture.worker).await;
        assert_eq!(baton.len(), 1);
        assert_eq!(baton[0].0, fixture.lead.to_string());
        assert!(baton[0].1.contains("#2898 baton"), "{}", baton[0].1);
        assert!(baton[0].1.contains("PIPELINE HANDOFF — BATON <sha>"));
        let notice = mail_to(&fixture, fixture.lead).await;
        assert_eq!(notice.len(), 1);
        assert!(
            notice[0]
                .1
                .contains("\"record_kind\":\"worker_context_cap\"")
                && notice[0].1.contains("\"trigger\":\"pre_compact\""),
            "{}",
            notice[0].1
        );

        // The same compaction reported again (or a later one): no second baton.
        let again = pre_compact(&fixture, &config, fixture.worker)
            .await
            .handoff
            .expect("the instruction is repeated");
        assert!(!again.requested);
        assert_eq!(again.instruction, handoff.instruction);
        assert_eq!(mail_to(&fixture, fixture.worker).await.len(), 1);
        assert_eq!(mail_to(&fixture, fixture.lead).await.len(), 1);
    }

    /// #2901: an operator's explicit zero worker cap (global, provider or
    /// model) turns the baton off: a worker's PreCompact records no baton,
    /// sends no mail and no `worker_context_cap` notice and its handoff stays
    /// empty, while the compaction notice row is still written.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn an_explicitly_disabled_worker_cap_passes_no_baton() {
        for level in ["global", "provider", "model"] {
            let fixture = epic_fixture();
            let config = runtime();
            let worker = fixture
                .store
                .lock()
                .await
                .get_session(fixture.worker)
                .expect("read")
                .expect("worker");
            let provider = serde_json::to_value(worker.provider).expect("provider");
            let provider = provider.as_str().expect("provider name");
            let (field, key) = match level {
                "global" => ("worker_context_cap_tokens".to_string(), None),
                "provider" => (
                    format!("worker_context_cap.{provider}"),
                    Some(provider.to_string()),
                ),
                _ => {
                    let model = worker.model.clone().unwrap_or_else(|| "model-2901".into());
                    (
                        format!("worker_context_cap.{provider}/{model}"),
                        Some(model),
                    )
                }
            };
            if level == "model" {
                // The override applies to the worker's own (live) model.
                fixture
                    .active
                    .write()
                    .await
                    .get_mut(&fixture.worker)
                    .expect("live")
                    .session
                    .model = key.clone();
            }
            config
                .update_field(&field, &serde_json::json!(0))
                .expect("explicit zero");
            let reply = pre_compact(&fixture, &config, fixture.worker).await;
            assert_eq!(
                reply.outcome, "queued",
                "{level}: the notice row is still queued"
            );
            assert_eq!(reply.handoff, None, "{level}: the handoff stays empty");
            assert!(
                fixture
                    .store
                    .lock()
                    .await
                    .worker_baton(fixture.worker)
                    .expect("read")
                    .is_none(),
                "{level}: no baton record"
            );
            assert_eq!(mail_to(&fixture, fixture.worker).await.len(), 0, "{level}");
            assert_eq!(
                mail_to(&fixture, fixture.lead).await.len(),
                0,
                "{level}: no notice"
            );
        }
    }

    /// #2898 end to end: a coordinating seat's PreCompact records its #1005
    /// context-cap rotation (due at its next idle boundary) once.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn a_seat_pre_compact_requests_its_context_cap_succession_once() {
        use super::super::context_cap::{CapState, cap_rotation};
        let fixture = epic_fixture();
        let config = runtime();
        let handoff = pre_compact(&fixture, &config, fixture.lead)
            .await
            .handoff
            .expect("a seat handoff");
        assert_eq!(handoff.action, COMPACTION_HANDOFF_SEAT_SUCCESSION);
        assert!(handoff.requested);
        assert_eq!(handoff.deferred, None);
        assert!(handoff.instruction.contains("context-cap succession"));
        let record = cap_rotation(&*fixture.store.lock().await, fixture.lead)
            .expect("read")
            .expect("the cap rotation record");
        assert_eq!(record.state, CapState::Due);
        assert_eq!(record.cap_tokens, 500_000);
        assert!(
            fixture
                .store
                .lock()
                .await
                .worker_baton(fixture.lead)
                .expect("read")
                .is_none(),
            "a seat is never told to pass the worker baton"
        );
        let again = pre_compact(&fixture, &config, fixture.lead)
            .await
            .handoff
            .expect("a seat handoff");
        assert!(!again.requested, "one compaction, one succession");
    }

    /// #2898: with cap rotation off the seat's request is not written (the
    /// idle-boundary pass would clear it); the seat is still told.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn a_seat_with_cap_rotation_off_is_deferred_not_recorded() {
        let fixture = epic_fixture();
        let config = runtime();
        config
            .context_rotation_enabled
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let handoff = pre_compact(&fixture, &config, fixture.lead)
            .await
            .handoff
            .expect("a seat handoff");
        assert_eq!(handoff.deferred, Some("cap_rotation_off"));
        assert!(!handoff.requested);
        assert!(
            super::super::context_cap::cap_rotation(&*fixture.store.lock().await, fixture.lead)
                .expect("read")
                .is_none()
        );
    }

    /// #2898 nonblocking: a handoff that cannot be requested (the store
    /// refuses the write) leaves `handoff` empty, the notice row is still
    /// recorded and the call never errors; a StopFailure drives no handoff.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn a_failed_handoff_request_never_fails_the_event() {
        let fixture = epic_fixture();
        let config = runtime();
        fixture
            .store
            .lock()
            .await
            .conn
            .execute_batch(
                "CREATE TEMP TRIGGER refuse_baton BEFORE INSERT ON daemon_settings
                 BEGIN SELECT RAISE(ABORT, 'injected store failure'); END;",
            )
            .expect("trigger");
        let reply = pre_compact(&fixture, &config, fixture.worker).await;
        assert_eq!(reply.outcome, "queued");
        assert_eq!(reply.handoff, None);
        assert!(mail_to(&fixture, fixture.worker).await.is_empty());

        let stop = record_provider_lifecycle_event(
            &fixture.store,
            &fixture.active,
            &fixture.bus,
            &config,
            fixture.worker,
            params("StopFailure", serde_json::json!({"error": "overloaded"})),
        )
        .await;
        assert_eq!(stop.handoff, None);
        let wire = serde_json::to_value(&stop).expect("json");
        assert_eq!(wire, serde_json::json!({"outcome": "queued"}));
    }

    /// #2898: a session with neither a seat nor a launcher (an operator
    /// session) gets no handoff, as the context cap gives it none.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn a_session_without_seat_or_launcher_gets_no_handoff() {
        let fixture = fixture(true);
        let reply = record_provider_lifecycle_event(
            &fixture.store,
            &fixture.active,
            &fixture.bus,
            &runtime(),
            fixture.session,
            params("PreCompact", serde_json::json!({"trigger": "manual"})),
        )
        .await;
        assert_eq!(reply.outcome, "queued");
        assert_eq!(reply.handoff, None);
    }
}
