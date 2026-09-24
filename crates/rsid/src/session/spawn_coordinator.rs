//! State machine + rate-limit guard for `/spawn_child` directives emitted by
//! Epic-lead sessions.
//!
//! Mirrors `RotationCoordinator` in shape: a small typed state machine,
//! `Idle → Detected → Validating → Spawning → Done | Rejected`. Unlike the
//! rotation coordinator, this one is **daemon-global** so the per-Epic token
//! bucket holds across every leaf session that emits directives.
//!
//! Wiring summary:
//! - `monitor::monitor_session` scans accumulated assistant text for
//!   `<docregblock>/spawn_child …</docregblock>` blocks.
//! - For each block it parses to a `SpawnDirective` and calls
//!   `SpawnCoordinator::handle(...)`.
//! - The coordinator validates lead-emitter identity + recursion depth +
//!   per-Epic token bucket against the `Store`, builds a `LaunchConfig`, and
//!   sends a `SpawnRequest` over the daemon-wide spawn channel.
//! - `main.rs` (parallel to the retry-loop) consumes `SpawnRequest`s and calls
//!   `session_manager.launch_session(...)`. After launch it emits
//!   `DaemonEvent::ChildSpawned`.
//!
//! TODO(future): both `RotationCoordinator` and `SpawnCoordinator` are
//! "single-owner state machine + side-effect Action enum + rate guard".
//! When a third coordinator joins the codebase, factor out a `Coordinator`
//! trait. Premature now.

use crate::claude::LaunchConfig;
use crate::model_control::hash_request_fingerprint;
use crate::store::Store;
use crate::store::agent_coordination::{AgentSpawnRequestRecord, ReserveAgentSpawnOutcome};
use rsi_common::agent_coordination::{AgentSpawnChildRequestV1, AgentSpawnStateV1};
use rsi_common::types::{SandboxSpec, Session, SessionKind};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock, mpsc};
use uuid::Uuid;

use super::spawn_directive::SpawnDirective;
use super::types::{CompletedSession, TrackedSession};

/// Maximum recursion depth from emitter back to the root via `parent_id`.
/// At `MAX_SPAWN_DEPTH = 5` an emitter five levels deep cannot spawn further.
pub const MAX_SPAWN_DEPTH: u32 = 5;

/// Hard cap on per-node topology iterations. A lead emitting iteration > 32
/// is rejected even if `TopologyNode.max_iterations` is higher or absent.
pub(crate) const MAX_ITERATIONS: u32 = 32;

/// Per-Epic token-bucket capacity (max bursts).
pub const TOKEN_BUCKET_CAPACITY: u32 = 10;

/// Per-Epic refill window — full bucket replenishes over this duration.
pub const TOKEN_BUCKET_REFILL_WINDOW: Duration = Duration::from_secs(5 * 60);

/// Idle bucket entries older than this are GC'd to bound memory.
pub const TOKEN_BUCKET_GC_AGE: Duration = Duration::from_secs(60 * 60);

/// Why a directive was rejected. All variants are non-fatal — log and drop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnRejectReason {
    /// Emitter is not currently the lead of any Epic, or the resolved Epic's
    /// `lead_session_id` does not point at the emitter.
    NotLead,
    /// Emitter is not a leaf kind (Group/Epic cannot emit /spawn_child).
    EmitterNotLeaf,
    /// Walking parent_id chain exceeded `MAX_SPAWN_DEPTH`.
    DepthLimitExceeded { depth: u32 },
    /// Per-Epic rate limit hit.
    RateLimited { epic_id: Uuid },
    /// This exact directive block has already produced a spawn request.
    DuplicateDirective,
    /// Emitter session row not found in store (race vs. delete).
    EmitterNotFound,
    /// Resolved Epic row not found (race vs. delete).
    EpicNotFound,
    /// Directive's `kind` is not a legal child of an Epic.
    IllegalChildKind { kind: SessionKind },
    /// Store I/O error during validation. Body kept short for log lines.
    StoreError(String),
    // ─── Phase 7 (P1.7): topology binding reject reasons ────────────────────
    /// Directive names a topology node not present in the Epic's effective topology.
    TopologyNodeNotFound { node_id: String },
    /// Directive kind does not match the topology node's declared kind.
    NodeKindMismatch {
        directive_kind: SessionKind,
        node_kind: SessionKind,
    },
    /// One or more prerequisite nodes have no Completed session under this Epic.
    PrereqsNotSatisfied { missing: Vec<String> },
    /// Iteration exceeds per-node max_iterations or the global MAX_ITERATIONS cap.
    IterationCapExceeded { iteration: u32, cap: u32 },
    /// Directive names a topology node but the Epic has no topology.
    NoTopologyOnEpic { epic_id: Uuid },
    /// topology_node + iteration already spawned for this (epic_id, node_id, iteration) tuple.
    DuplicateTopologyBinding { node_id: String, iteration: u32 },
    // ─── Orchestration cost guardrail (issue #2) ───────────────────────────
    /// The orchestration tier/effort escalation guardrail
    /// (`enforce_orchestration_tier_escalation`) refuses this child. Caught
    /// here, synchronously, instead of later inside `launch_session` where the
    /// failure was logged and dropped after the caller was told "enqueued".
    OrchestrationEscalationDenied {
        requested_tier: String,
        requested_effort: Option<String>,
        root_tier: Option<String>,
        root_effort: Option<String>,
        detail: String,
    },
    /// An explicit Pioneer model is absent from the valid locally cached
    /// account catalog. The cache miss case remains fail-open for first use.
    PioneerModelUnavailable { model: String },
    /// An explicit `AgentSpawnChild.effort` is outside the selected model's
    /// authoritative effort ladder (issue #243). Persisting it would make the
    /// child's recorded effort disagree with what the provider actually runs.
    UnsupportedChildEffort {
        model: String,
        effort: String,
        valid: &'static [&'static str],
    },
}

impl std::fmt::Display for SpawnRejectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotLead => write!(f, "emitter is not the Epic lead"),
            Self::EmitterNotLeaf => write!(f, "emitter is not a leaf-kind session"),
            Self::DepthLimitExceeded { depth } => {
                write!(
                    f,
                    "spawn depth limit exceeded ({depth} > {MAX_SPAWN_DEPTH})"
                )
            }
            Self::RateLimited { epic_id } => {
                write!(f, "per-Epic rate limit hit for {epic_id}")
            }
            Self::DuplicateDirective => write!(f, "duplicate spawn directive"),
            Self::EmitterNotFound => write!(f, "emitter session not found in store"),
            Self::EpicNotFound => write!(f, "epic session not found in store"),
            Self::IllegalChildKind { kind } => {
                write!(f, "illegal child kind for Epic: {kind:?}")
            }
            Self::StoreError(e) => write!(f, "store error: {e}"),
            Self::TopologyNodeNotFound { node_id } => {
                write!(f, "topology node not found: {node_id}")
            }
            Self::NodeKindMismatch {
                directive_kind,
                node_kind,
            } => {
                write!(
                    f,
                    "node kind mismatch: directive={directive_kind:?} node={node_kind:?}"
                )
            }
            Self::PrereqsNotSatisfied { missing } => {
                write!(f, "prereqs not satisfied: {missing:?}")
            }
            Self::IterationCapExceeded { iteration, cap } => {
                write!(f, "iteration cap exceeded: {iteration} > {cap}")
            }
            Self::NoTopologyOnEpic { epic_id } => {
                write!(f, "no topology on Epic {epic_id}")
            }
            Self::DuplicateTopologyBinding { node_id, iteration } => {
                write!(f, "duplicate topology binding: {node_id}@{iteration}")
            }
            Self::OrchestrationEscalationDenied {
                requested_tier,
                requested_effort,
                root_tier,
                root_effort,
                detail,
            } => write!(
                f,
                "orchestration escalation denied: requested tier={requested_tier} effort={} \
                 exceeds tree root tier={} effort={} ({detail})",
                requested_effort.as_deref().unwrap_or("-"),
                root_tier.as_deref().unwrap_or("-"),
                root_effort.as_deref().unwrap_or("-"),
            ),
            Self::PioneerModelUnavailable { model } => {
                write!(
                    f,
                    "Pioneer model is not available in the cached catalog: {model}"
                )
            }
            Self::UnsupportedChildEffort {
                model,
                effort,
                valid,
            } => write!(
                f,
                "unsupported effort '{effort}' for model '{model}' (valid: {})",
                valid.join(", ")
            ),
        }
    }
}

fn unavailable_explicit_pioneer_model(
    model: Option<&str>,
    cached_availability: Option<bool>,
) -> Option<SpawnRejectReason> {
    model
        .filter(|_| cached_availability == Some(false))
        .map(|model| SpawnRejectReason::PioneerModelUnavailable {
            model: model.to_string(),
        })
}

/// State of a single spawn attempt. Built and consumed by `handle` — not
/// persisted across calls (the bucket map is the only persistent state).
#[derive(Debug, Clone)]
pub enum SpawnState {
    Idle,
    Detected {
        emitter_id: Uuid,
    },
    Validating {
        emitter_id: Uuid,
    },
    Spawning {
        emitter_id: Uuid,
        epic_id: Uuid,
        directive: SpawnDirective,
        spawn_request_id: Uuid,
        child_session_id: Uuid,
        spawn_state: AgentSpawnStateV1,
        agent_role: Option<String>,
        epic_spawn_ordinal: u32,
        deduplicated: bool,
        enqueued: bool,
        safe_error_class: Option<String>,
    },
    Done {
        epic_id: Uuid,
        child_id: Uuid,
        kind: SessionKind,
    },
    Rejected {
        reason: SpawnRejectReason,
    },
}

/// A validated spawn request that the daemon main loop should turn into a
/// real `launch_session` call. Sent over `spawn_request_tx`.
#[derive(Debug)]
pub struct SpawnRequest {
    pub config: LaunchConfig,
    pub epic_id: Uuid,
    pub kind: SessionKind,
    pub spawn_request_id: Uuid,
    pub child_session_id: Uuid,
    pub owner_session_id: Uuid,
}

/// Bounded hint for the daemon-owned master-successor reconciler. The durable
/// reservation remains authoritative; this message contains no hierarchy,
/// lead, candidate, or credential data an agent could spoof.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SuccessorDispatchRequest {
    pub reservation_id: Uuid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SuccessorDispatchOutcome {
    Queued,
    AlreadyPending,
}

/// Token-bucket entry per Epic.
#[derive(Debug, Clone)]
struct TokenBucket {
    /// Floating-point token count for fractional refill.
    tokens: f64,
    /// Last time we refilled.
    last_refill: Instant,
    /// Last time the bucket was touched (for GC).
    last_touch: Instant,
}

impl TokenBucket {
    fn new(now: Instant) -> Self {
        Self {
            tokens: TOKEN_BUCKET_CAPACITY as f64,
            last_refill: now,
            last_touch: now,
        }
    }

    /// Refill the bucket based on elapsed wallclock time. Caps at capacity.
    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last_refill);
        if elapsed.is_zero() {
            return;
        }
        let refill_per_sec =
            TOKEN_BUCKET_CAPACITY as f64 / TOKEN_BUCKET_REFILL_WINDOW.as_secs_f64();
        self.tokens = (self.tokens + elapsed.as_secs_f64() * refill_per_sec)
            .min(TOKEN_BUCKET_CAPACITY as f64);
        self.last_refill = now;
    }

    /// Try to consume one token. Returns true on success.
    fn try_consume(&mut self, now: Instant) -> bool {
        self.refill(now);
        self.last_touch = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Daemon-global spawn coordinator. One instance per `SessionManager`.
/// All state — including the per-Epic bucket map — lives behind a `Mutex` so
/// `Arc<SpawnCoordinator>` clones can be shared with every monitor task.
pub struct SpawnCoordinator {
    /// Serializes durable idempotency lookup/reservation/enqueue. The Store is
    /// already single-writer; this closes the async gap around channel send.
    reservation_lock: Mutex<()>,
    /// Per-Epic token buckets. Bucket created lazily on first spawn attempt
    /// for that Epic, and GC'd lazily on subsequent calls if the entry is
    /// idle longer than `TOKEN_BUCKET_GC_AGE`.
    buckets: Mutex<HashMap<Uuid, TokenBucket>>,
    /// Directive blocks that already produced a spawn request. Key is
    /// `(emitter_session_id, hash(full_matched_docregblock))`.
    consumed: Mutex<HashMap<(Uuid, u64), Instant>>,
    /// Outbound channel for validated spawn requests. The receiver lives in
    /// `main.rs` and dispatches each request to `SessionManager::launch_session`.
    spawn_tx: mpsc::Sender<SpawnRequest>,
    /// Installed once by `SessionManager`; carries only durable reservation
    /// IDs to the successor reconciler.
    successor_tx: OnceLock<mpsc::Sender<SuccessorDispatchRequest>>,
    /// Coalesces status, startup, and backstop hints for the same durable row.
    successor_pending: Mutex<HashSet<Uuid>>,
    /// Optional clock override — non-None only in tests.
    #[cfg(test)]
    test_clock: Mutex<Option<Instant>>,
}

impl SpawnCoordinator {
    pub fn new(spawn_tx: mpsc::Sender<SpawnRequest>) -> Self {
        Self {
            reservation_lock: Mutex::new(()),
            buckets: Mutex::new(HashMap::new()),
            consumed: Mutex::new(HashMap::new()),
            spawn_tx,
            successor_tx: OnceLock::new(),
            successor_pending: Mutex::new(HashSet::new()),
            #[cfg(test)]
            test_clock: Mutex::new(None),
        }
    }

    pub(crate) fn install_successor_sender(
        &self,
        sender: mpsc::Sender<SuccessorDispatchRequest>,
    ) -> Result<(), &'static str> {
        self.successor_tx
            .set(sender)
            .map_err(|_| "successor_dispatch_sender_already_installed")
    }

    pub(crate) async fn dispatch_successor(
        &self,
        reservation_id: Uuid,
    ) -> Result<SuccessorDispatchOutcome, String> {
        let sender = self
            .successor_tx
            .get()
            .ok_or_else(|| "successor_dispatch_sender_unavailable".to_string())?;
        {
            let mut pending = self.successor_pending.lock().await;
            if !pending.insert(reservation_id) {
                return Ok(SuccessorDispatchOutcome::AlreadyPending);
            }
        }
        if let Err(error) = sender.try_send(SuccessorDispatchRequest { reservation_id }) {
            self.successor_pending.lock().await.remove(&reservation_id);
            return Err(format!("successor_dispatch_deferred:{error}"));
        }
        Ok(SuccessorDispatchOutcome::Queued)
    }

    #[doc(hidden)]
    pub async fn complete_successor_dispatch(&self, reservation_id: Uuid) {
        self.successor_pending.lock().await.remove(&reservation_id);
    }

    fn now(&self) -> Instant {
        #[cfg(test)]
        {
            // tokio::sync::Mutex is async; we can't blocking_lock in async ctx.
            // The override is only set/read inside test helpers so the unsafe
            // sync access happens via try_lock — never contested in tests.
            if let Ok(g) = self.test_clock.try_lock() {
                if let Some(i) = *g {
                    return i;
                }
            }
        }
        Instant::now()
    }

    /// Garbage-collect bucket entries whose `last_touch` is older than
    /// `TOKEN_BUCKET_GC_AGE`. Called opportunistically from `try_spawn` and
    /// limited so the GC scan is O(n) at most once per call.
    fn gc_buckets(buckets: &mut HashMap<Uuid, TokenBucket>, now: Instant) {
        buckets.retain(|_, b| now.saturating_duration_since(b.last_touch) < TOKEN_BUCKET_GC_AGE);
    }

    fn gc_consumed(consumed: &mut HashMap<(Uuid, u64), Instant>, now: Instant) {
        consumed.retain(|_, touched| now.saturating_duration_since(*touched) < TOKEN_BUCKET_GC_AGE);
    }

    /// Validate, rate-limit, and dispatch one spawn directive.
    ///
    /// Returns the terminal `SpawnState`. Caller should log any non-`Done`
    /// outcome at debug level (rejections are not user-facing errors).
    ///
    /// Note: this method does NOT actually launch the child. It enqueues a
    /// `SpawnRequest` on the spawn channel; the receiver in `main.rs` calls
    /// `launch_session` and emits `DaemonEvent::ChildSpawned`.
    pub async fn handle(
        &self,
        emitter_id: Uuid,
        block_hash: u64,
        directive: SpawnDirective,
        active: &Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
        completed: &Arc<RwLock<HashMap<Uuid, CompletedSession>>>,
        store: &Arc<tokio::sync::Mutex<Store>>,
    ) -> SpawnState {
        let request = AgentSpawnChildRequestV1 {
            kind: directive.kind,
            provider: directive.provider,
            model: directive.model.clone(),
            effort: directive.effort.clone(),
            agent_role: directive.agent_role.clone(),
            query: directive.query.clone(),
            topology_node: directive.topology_node.clone(),
            iteration: directive.iteration,
            tags: directive.tags.clone(),
            idempotency_key: format!("directive:{block_hash:016x}"),
        };
        self.handle_request(
            emitter_id,
            Some(block_hash),
            directive,
            request,
            active,
            completed,
            store,
            false,
        )
        .await
    }

    /// Token/native agent spawn path. Unlike directive detection, exact
    /// retries are keyed only by the caller-bound durable idempotency digest.
    pub async fn handle_agent(
        &self,
        emitter_id: Uuid,
        request: AgentSpawnChildRequestV1,
        active: &Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
        completed: &Arc<RwLock<HashMap<Uuid, CompletedSession>>>,
        store: &Arc<tokio::sync::Mutex<Store>>,
    ) -> SpawnState {
        let directive = SpawnDirective {
            kind: request.kind,
            provider: request.provider,
            model: request.model.clone(),
            effort: request.effort.clone(),
            agent_role: request.agent_role.clone(),
            query: request.query.clone(),
            topology_node: request.topology_node.clone(),
            iteration: request.iteration,
            tags: request.tags.clone(),
        };
        self.handle_request(
            emitter_id, None, directive, request, active, completed, store, false,
        )
        .await
    }

    /// Startup-only replay of durable reservations whose in-memory queue was
    /// lost with the prior daemon process. The normal retry path never sets
    /// this flag, so same-process exact retries remain response-only.
    pub async fn reconcile_incomplete(
        &self,
        active: &Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
        completed: &Arc<RwLock<HashMap<Uuid, CompletedSession>>>,
        store: &Arc<tokio::sync::Mutex<Store>>,
    ) -> crate::error::Result<usize> {
        let requests = store.lock().await.list_incomplete_agent_spawn_requests()?;
        let mut requeued = 0usize;
        for record in requests {
            let directive = SpawnDirective {
                kind: record.request.kind,
                provider: record.request.provider,
                model: record.request.model.clone(),
                effort: record.request.effort.clone(),
                agent_role: record.request.agent_role.clone(),
                query: record.request.query.clone(),
                topology_node: record.request.topology_node.clone(),
                iteration: record.request.iteration,
                tags: record.request.tags.clone(),
            };
            let state = self
                .handle_request(
                    record.owner_session_id,
                    None,
                    directive,
                    record.request,
                    active,
                    completed,
                    store,
                    true,
                )
                .await;
            match state {
                SpawnState::Spawning { enqueued: true, .. } => requeued += 1,
                SpawnState::Rejected { reason } => tracing::warn!(
                    target: "agent_coordination",
                    spawn_request_id = %record.spawn_request_id,
                    error = %reason,
                    "incomplete durable spawn reconciliation deferred"
                ),
                _ => {}
            }
        }
        tracing::info!(
            target: "agent_coordination",
            requeued_spawn_count = requeued,
            "incomplete durable spawn reconciliation complete"
        );
        Ok(requeued)
    }

    async fn handle_request(
        &self,
        emitter_id: Uuid,
        directive_block_hash: Option<u64>,
        directive: SpawnDirective,
        request: AgentSpawnChildRequestV1,
        active: &Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
        completed: &Arc<RwLock<HashMap<Uuid, CompletedSession>>>,
        store: &Arc<tokio::sync::Mutex<Store>>,
        startup_requeue: bool,
    ) -> SpawnState {
        let request = match request.normalized() {
            Ok(request) => request,
            Err(error) => {
                return SpawnState::Rejected {
                    reason: SpawnRejectReason::StoreError(error.to_string()),
                };
            }
        };
        let _reservation_guard = self.reservation_lock.lock().await;
        let now = self.now();
        if let Some(block_hash) = directive_block_hash {
            let mut consumed = self.consumed.lock().await;
            Self::gc_consumed(&mut consumed, now);
            if consumed.contains_key(&(emitter_id, block_hash)) {
                return SpawnState::Rejected {
                    reason: SpawnRejectReason::DuplicateDirective,
                };
            }
            consumed.insert((emitter_id, block_hash), now);
        }

        let idempotency_digest = hash_request_fingerprint(&[
            "agent-spawn-idempotency-v1",
            &emitter_id.to_string(),
            &request.idempotency_key,
        ]);
        let request_json = match serde_json::to_string(&request) {
            Ok(json) => json,
            Err(error) => {
                return SpawnState::Rejected {
                    reason: SpawnRejectReason::StoreError(error.to_string()),
                };
            }
        };
        let request_fingerprint = hash_request_fingerprint(&[
            "agent-spawn-request-v1",
            &emitter_id.to_string(),
            &request_json,
        ]);
        let mut replayed = false;
        let mut durable_request = match store
            .lock()
            .await
            .find_agent_spawn_request(emitter_id, &idempotency_digest)
        {
            Ok(Some(existing)) => {
                replayed = true;
                if existing.request_fingerprint != request_fingerprint
                    || existing.request != request
                {
                    return SpawnState::Rejected {
                        reason: SpawnRejectReason::StoreError(format!(
                            "agent_spawn_idempotency_conflict:{}",
                            existing.spawn_request_id
                        )),
                    };
                }
                if existing.state != AgentSpawnStateV1::Reserved && !startup_requeue {
                    return spawn_state_from_record(existing, directive, true, false);
                }
                Some(existing)
            }
            Ok(None) => None,
            Err(error) => {
                return SpawnState::Rejected {
                    reason: SpawnRejectReason::StoreError(error.to_string()),
                };
            }
        };

        // ── Validating ──
        // 1. Resolve the emitter from the durable authority plane. Runtime
        // caches may lag an irreversible successor baton commit, so they are
        // never consulted for lead identity or its owning hierarchy.
        let emitter = match store.lock().await.get_session(emitter_id) {
            Ok(Some(session)) => session,
            Ok(None) => {
                return SpawnState::Rejected {
                    reason: SpawnRejectReason::EmitterNotFound,
                };
            }
            Err(error) => {
                return SpawnState::Rejected {
                    reason: SpawnRejectReason::StoreError(error.to_string()),
                };
            }
        };

        if !rsi_common::is_leaf_kind(emitter.session_kind) {
            return SpawnState::Rejected {
                reason: SpawnRejectReason::EmitterNotLeaf,
            };
        }

        // 2. Walk parent chain to find the owning Epic AND verify depth.
        // We expect: emitter.parent_id -> Epic -> ... up the chain.
        let Some(parent_id) = emitter.parent_id else {
            return SpawnState::Rejected {
                reason: SpawnRejectReason::NotLead,
            };
        };

        // Walk depth: emitter is depth 1 (one parent up). Anything beyond
        // MAX_SPAWN_DEPTH (counting emitter and its ancestors) is rejected.
        let mut depth: u32 = 1;
        let mut cursor: Option<Uuid> = Some(parent_id);
        let mut maybe_epic_id: Option<Uuid> = None;
        while let Some(pid) = cursor {
            depth += 1;
            if depth > MAX_SPAWN_DEPTH {
                return SpawnState::Rejected {
                    reason: SpawnRejectReason::DepthLimitExceeded { depth },
                };
            }
            let parent = match store.lock().await.get_session(pid) {
                Ok(Some(session)) => session,
                Ok(None) => {
                    return SpawnState::Rejected {
                        reason: SpawnRejectReason::EpicNotFound,
                    };
                }
                Err(error) => {
                    return SpawnState::Rejected {
                        reason: SpawnRejectReason::StoreError(error.to_string()),
                    };
                }
            };
            // First Epic encountered ascending the chain is the owning Epic.
            if matches!(parent.session_kind, SessionKind::Epic) && maybe_epic_id.is_none() {
                maybe_epic_id = Some(parent.id);
                // Verify lead identity here while we have the row.
                if parent.lead_session_id != Some(emitter_id) {
                    return SpawnState::Rejected {
                        reason: SpawnRejectReason::NotLead,
                    };
                }
            }
            cursor = parent.parent_id;
        }

        let epic_id = match maybe_epic_id {
            Some(id) => id,
            None => {
                return SpawnState::Rejected {
                    reason: SpawnRejectReason::NotLead,
                };
            }
        };

        // 3. Validate the directive's kind is a legal Epic child.
        let legal = rsi_common::legal_children(Some(SessionKind::Epic));
        if !legal.contains(&directive.kind) {
            return SpawnState::Rejected {
                reason: SpawnRejectReason::IllegalChildKind {
                    kind: directive.kind,
                },
            };
        }

        // 3.5 — Topology binding (Phase 7, P1.7)
        // Resolve topology and bind the child if directive.topology_node is Some.
        // After this block, `bound_node_id` and `bound_iteration` hold the
        // final binding values (both None/0 for unbound spawns).
        let (bound_node_id, bound_iteration): (Option<String>, u32) = {
            // 3.5.a — fetch the epic session to walk effective_topology.
            let epic_session = match store.lock().await.get_session(epic_id) {
                Ok(Some(session)) => session,
                Ok(None) => {
                    return SpawnState::Rejected {
                        reason: SpawnRejectReason::EpicNotFound,
                    };
                }
                Err(error) => {
                    return SpawnState::Rejected {
                        reason: SpawnRejectReason::StoreError(error.to_string()),
                    };
                }
            };

            // Build a lightweight sessions_view from active + completed maps.
            let mut view: HashMap<Uuid, Session> = {
                let ar = active.read().await;
                let cr = completed.read().await;
                let mut m = HashMap::with_capacity(ar.len() + cr.len());
                for (id, ts) in ar.iter() {
                    m.insert(*id, ts.session.clone());
                }
                for (id, cs) in cr.iter() {
                    m.insert(*id, cs.session.clone());
                }
                m
            };

            // Include epic_session in view so effective_topology can walk it.
            view.insert(epic_session.id, epic_session.clone());

            use crate::session::hierarchy_ops::effective_topology_with_override;
            let topology_id = effective_topology_with_override(&epic_session, &view);

            // 3.5.b — branch on (topology_id, directive.topology_node)
            match (&topology_id, &directive.topology_node) {
                (None, None) => {
                    // Unbound spawn: Epic has no topology and directive omits node.
                    tracing::debug!(
                        epic_id = %epic_id,
                        "unbound spawn: Epic has no topology and directive omits node"
                    );
                    (None, 0)
                }
                (None, Some(_)) => {
                    // Directive names a node but Epic has no topology.
                    return SpawnState::Rejected {
                        reason: SpawnRejectReason::NoTopologyOnEpic { epic_id },
                    };
                }
                (Some(tid), None) => {
                    // Unbound spawn under a topology-having Epic — allowed (mixed mode).
                    tracing::debug!(
                        epic_id = %epic_id,
                        topology_id = %tid,
                        "unbound spawn under topology Epic"
                    );
                    (None, 0)
                }
                (Some(topology_id), Some(node_id)) => {
                    let topology_id = *topology_id;
                    let node_id = node_id.clone();

                    // 3.5.1 — Load topology definition.
                    let topology = {
                        let g = store.lock().await;
                        g.get_topology(topology_id)
                            .map_err(|e| e.to_string())
                            .and_then(|opt| {
                                opt.ok_or_else(|| {
                                    format!(
                                        "topology {topology_id} not found — Epic {epic_id} workflow_id dangling"
                                    )
                                })
                            })
                    };
                    let topology = match topology {
                        Ok(t) => t,
                        Err(e) => {
                            return SpawnState::Rejected {
                                reason: SpawnRejectReason::StoreError(e),
                            };
                        }
                    };

                    // 3.5.2 — Find node in topology definition.
                    let topo_node = topology
                        .definition
                        .nodes
                        .iter()
                        .find(|n| n.id == node_id)
                        .cloned();
                    let topo_node = match topo_node {
                        Some(n) => n,
                        None => {
                            return SpawnState::Rejected {
                                reason: SpawnRejectReason::TopologyNodeNotFound {
                                    node_id: node_id.clone(),
                                },
                            };
                        }
                    };

                    // 3.5.3 — Kind check.
                    if topo_node.kind != directive.kind {
                        return SpawnState::Rejected {
                            reason: SpawnRejectReason::NodeKindMismatch {
                                directive_kind: directive.kind,
                                node_kind: topo_node.kind,
                            },
                        };
                    }

                    // 3.5.4 — Prereq verification.
                    let all_children: Vec<Session> = view
                        .values()
                        .filter(|s| s.parent_id == Some(epic_id))
                        .cloned()
                        .collect();
                    let child_refs: Vec<&Session> = all_children.iter().collect();
                    let missing: Vec<String> = topology
                        .definition
                        .edges
                        .iter()
                        .filter(|e| e.to == node_id)
                        .map(|e| e.from.clone())
                        .filter(|prereq| {
                            !crate::session::hierarchy_ops::prereq_satisfied(
                                &child_refs,
                                epic_id,
                                prereq,
                            )
                        })
                        .collect();
                    if !missing.is_empty() {
                        return SpawnState::Rejected {
                            reason: SpawnRejectReason::PrereqsNotSatisfied { missing },
                        };
                    }

                    // 3.5.5 — Iteration computation.
                    let iteration = match directive.iteration {
                        Some(n) => n,
                        None => {
                            let max_iter = crate::session::hierarchy_ops::max_iter_for_node(
                                store, epic_id, &node_id,
                            )
                            .await
                            .map_err(|e| e.to_string());
                            match max_iter {
                                Ok(m) => m + 1,
                                Err(e) => {
                                    return SpawnState::Rejected {
                                        reason: SpawnRejectReason::StoreError(e),
                                    };
                                }
                            }
                        }
                    };

                    // 3.5.6 — Iteration cap enforcement.
                    let node_cap = topo_node.max_iterations.unwrap_or(MAX_ITERATIONS);
                    let effective_cap = node_cap.min(MAX_ITERATIONS);
                    if iteration > effective_cap {
                        return SpawnState::Rejected {
                            reason: SpawnRejectReason::IterationCapExceeded {
                                iteration,
                                cap: effective_cap,
                            },
                        };
                    }

                    // 3.5.7 — Compound uniqueness check.
                    let existing = {
                        let g = store.lock().await;
                        g.conn
                            .query_row(
                                "SELECT COUNT(*) FROM sessions \
                                 WHERE parent_id = ?1 \
                                   AND topology_node_id = ?2 \
                                   AND topology_iteration = ?3",
                                rusqlite::params![epic_id.to_string(), node_id, iteration as i64],
                                |row| row.get::<_, i64>(0),
                            )
                            .unwrap_or(0)
                    };
                    if existing > 0 {
                        return SpawnState::Rejected {
                            reason: SpawnRejectReason::DuplicateTopologyBinding {
                                node_id: node_id.clone(),
                                iteration,
                            },
                        };
                    }

                    (Some(node_id), iteration)
                }
            }
        };

        // 3.6 — Orchestration tier/effort escalation precheck (issue #2).
        //
        // launch_session -> admit_invocation -> Store::admit_model_invocation
        // runs enforce_orchestration_tier_escalation on this very request
        // later — but that happens in main.rs's fire-and-forget spawn consumer,
        // long after this method answered the RPC caller with `Enqueued`, and
        // main.rs only error!()-logs the failure. Ask the SAME authority now,
        // synchronously, so a policy denial reaches the caller in its own
        // response. This does NOT replace the check inside admit_invocation:
        // that one stays authoritative and also guards three other
        // Orchestration purposes that never pass through this coordinator.
        //
        // An explicit cross-provider spawn must not inherit an incompatible
        // model or effort from the emitter. With no requested provider, retain
        // the established same-provider inheritance behavior exactly. A
        // cross-provider request without an explicit model bypasses the
        // project model default too, so this precheck and the final launch use
        // the same target-provider native default. Same-provider requests
        // retain the historical project-default behavior.
        let child_provider = directive.provider.unwrap_or(emitter.provider);
        let inherit_emitter_defaults = child_provider == emitter.provider;
        let skip_project_model_default = !inherit_emitter_defaults && directive.model.is_none();
        let child_model = directive.model.clone().or_else(|| {
            inherit_emitter_defaults
                .then(|| emitter.model.clone())
                .flatten()
        });
        let child_effort = directive.effort.clone().or_else(|| {
            let mut inherited = inherit_emitter_defaults
                .then(|| emitter.effort.clone())
                .flatten();
            // An inherited effort belongs to the EMITTER's session record, not
            // to this child's request. A lead whose own row already persisted an
            // undefined effort (issue #243, live session 5faae71c) must still be
            // able to spawn: reject only what the caller actually asked for, and
            // drop an inherited value only when RSI has AUTHORITATIVE knowledge
            // that the child model cannot use it.
            //
            // `reconcile_effort` is deliberately not used here: its
            // `effort_ladder` substitutes a legacy fallback and yields an empty
            // ladder for unrecognized IDs, which would silently strip a valid
            // inherited effort from every custom model. Unknown capability keeps
            // the inherited value exactly as before and leaves the CLI as the
            // validator.
            if let Some(model) = child_model.as_deref()
                && let Some(ladder) = rsi_common::model_utils::known_effort_ladder(model)
                && inherited
                    .as_deref()
                    .is_some_and(|effort| !ladder.contains(&effort))
            {
                inherited = None;
            }
            inherited
        });
        // An EXPLICIT effort is the caller's own claim, so it fails closed: a
        // child must never be persisted at an effort the model cannot run, or
        // `Session.effort` becomes unsound for downstream comparison (#243).
        // The check is authoritative-only: `known_effort_ladder` returns None
        // for custom/future model IDs, and enforcement fails open there so the
        // provider CLI remains the validator (mirrors the Codex/Claude launch
        // gates, which preserve unknown models).
        if let (Some(model), Some(effort)) = (child_model.as_deref(), directive.effort.as_deref())
            && let Some(ladder) = rsi_common::model_utils::known_effort_ladder(model)
            && !ladder.contains(&effort)
        {
            tracing::warn!(
                emitter_id = %emitter_id,
                epic_id = %epic_id,
                model,
                effort,
                "spawn_child rejected: effort is outside the model's ladder"
            );
            return SpawnState::Rejected {
                reason: SpawnRejectReason::UnsupportedChildEffort {
                    model: model.to_string(),
                    effort: effort.to_string(),
                    valid: ladder,
                },
            };
        }
        if child_provider == rsi_common::types::SessionProvider::Pioneer
            && directive.model.is_some()
        {
            let requested_model = crate::pioneer::pioneer_launch_model(child_model.as_deref());
            if let Some(reason) = unavailable_explicit_pioneer_model(
                Some(requested_model),
                crate::pioneer::cached_pioneer_model_is_available(requested_model),
            ) {
                return SpawnState::Rejected { reason };
            }
        }
        // Type annotation is load-bearing: it is the only consumer of the
        // `pub(crate) use` added in store/mod.rs (drop both together or keep both).
        let escalation: crate::error::Result<Option<crate::store::OrchestrationEscalationDenial>> = {
            let g = store.lock().await;
            g.session_model_invocation_id(emitter_id)
                .and_then(|parent_invocation_id| {
                    g.preview_orchestration_escalation(
                        rsi_common::model_control::ModelInvocationPurpose::AgentSpawnChild,
                        super::launch::admission_model_tier(child_provider, child_model.as_deref()),
                        child_effort.as_deref(),
                        parent_invocation_id,
                    )
                })
        };
        match escalation {
            Ok(None) => {}
            Ok(Some(denial)) => {
                tracing::warn!(
                    emitter_id = %emitter_id,
                    epic_id = %epic_id,
                    kind = ?directive.kind,
                    requested_tier = %denial.requested_tier,
                    requested_effort = denial.requested_effort.as_deref().unwrap_or("-"),
                    root_tier = denial.root_tier.as_deref().unwrap_or("-"),
                    root_effort = denial.root_effort.as_deref().unwrap_or("-"),
                    detail = %denial.detail,
                    "spawn_child rejected: orchestration tier/effort escalation denied"
                );
                return SpawnState::Rejected {
                    reason: SpawnRejectReason::OrchestrationEscalationDenied {
                        requested_tier: denial.requested_tier,
                        requested_effort: denial.requested_effort,
                        root_tier: denial.root_tier,
                        root_effort: denial.root_effort,
                        detail: denial.detail,
                    },
                };
            }
            // Lineage itself is unresolvable (cycle / depth / missing ancestor /
            // conflicting roots). Deterministic: admit_model_invocation runs the
            // identical resolution and will fail the same way, so surfacing it
            // here costs nothing and closes another silent-drop path.
            Err(crate::error::DaemonError::PolicyDenied(msg)) => {
                return SpawnState::Rejected {
                    reason: SpawnRejectReason::StoreError(format!(
                        "escalation precheck lineage: {msg}"
                    )),
                };
            }
            // Transient I/O or parse failure. FAIL OPEN — this pre-check exists
            // for visibility, not authority; it must never invent a denial the
            // real admission gate would not produce.
            Err(other) => {
                tracing::warn!(
                    emitter_id = %emitter_id,
                    epic_id = %epic_id,
                    error = %other,
                    "spawn_child: escalation precheck unavailable; deferring to admission gate"
                );
            }
        }

        // 4. Per-Epic rate limit. Exact durable replays never consume a
        // second token; only the first reservation is admitted here.
        if durable_request.is_none() {
            let allowed = {
                let mut buckets = self.buckets.lock().await;
                Self::gc_buckets(&mut buckets, now);
                let bucket = buckets
                    .entry(epic_id)
                    .or_insert_with(|| TokenBucket::new(now));
                bucket.try_consume(now)
            };
            if !allowed {
                return SpawnState::Rejected {
                    reason: SpawnRejectReason::RateLimited { epic_id },
                };
            }
            let spawn_request_id = Uuid::new_v4();
            let child_session_id = Uuid::new_v4();
            let reservation_result = {
                let guard = store.lock().await;
                guard.reserve_agent_spawn_request(
                    emitter_id,
                    &idempotency_digest,
                    &request_fingerprint,
                    &request,
                    epic_id,
                    spawn_request_id,
                    child_session_id,
                )
            };
            durable_request = match reservation_result {
                Ok(ReserveAgentSpawnOutcome::Reserved(record)) => Some(record),
                Ok(ReserveAgentSpawnOutcome::Replayed(record)) => {
                    replayed = true;
                    Some(record)
                }
                Err(crate::error::DaemonError::PolicyDenied(message))
                    if message == "agent_spawn_owner_is_not_current_epic_lead" =>
                {
                    let mut buckets = self.buckets.lock().await;
                    if let Some(bucket) = buckets.get_mut(&epic_id) {
                        bucket.tokens = (bucket.tokens + 1.0).min(TOKEN_BUCKET_CAPACITY as f64);
                    }
                    return SpawnState::Rejected {
                        reason: SpawnRejectReason::NotLead,
                    };
                }
                Err(error) => {
                    let mut buckets = self.buckets.lock().await;
                    if let Some(bucket) = buckets.get_mut(&epic_id) {
                        bucket.tokens = (bucket.tokens + 1.0).min(TOKEN_BUCKET_CAPACITY as f64);
                    }
                    return SpawnState::Rejected {
                        reason: SpawnRejectReason::StoreError(error.to_string()),
                    };
                }
            };
        }
        let durable_request = match durable_request {
            Some(record) if record.epic_id == epic_id && record.kind == directive.kind => record,
            Some(record) => {
                return SpawnState::Rejected {
                    reason: SpawnRejectReason::StoreError(format!(
                        "agent spawn replay scope mismatch:{}",
                        record.spawn_request_id
                    )),
                };
            }
            None => {
                return SpawnState::Rejected {
                    reason: SpawnRejectReason::StoreError(
                        "agent spawn reservation missing after admission".into(),
                    ),
                };
            }
        };

        // ── Spawning ──
        // The Epic is only the ownership parent. Runtime launch context
        // inherits from the lead emitter so children stay in the same repo and
        // sandbox policy. Provider/model/effort inherit only when the spawn
        // request leaves provider unset; an explicit provider selects that
        // child backend. Topology is NOT copied:
        // children spawn with `workflow_id = None` and consumers derive on
        // read by walking `parent_id` to the Epic via
        // `hierarchy_ops::effective_topology` (P1.3 — kill the per-session
        // workflow_id copy).
        let config = LaunchConfig {
            query: directive.query.clone(),
            title: None,
            agent_role: durable_request.request.agent_role.clone(),
            epic_spawn_ordinal: Some(durable_request.epic_spawn_ordinal),
            working_dir: Some(emitter.working_dir.clone()),
            provider: Some(child_provider),
            model: child_model,
            configured_context_window: None,
            max_turns: None,
            system_prompt: None,
            resume_session_id: None,
            session_kind: Some(directive.kind),
            project_id: emitter.project_id,
            rsi_session_id: None,
            rsi_socket: None,
            rsi_session_token: None,
            continued_from: None,
            openai_base_url: None,
            openai_api_key: None,
            conversation_history: None,
            workflow_id: None,
            workflow_id_override: None,
            max_retries: None,
            group_id: None,
            parent_id: Some(epic_id),
            effort: child_effort,
            issue_identifier: None,
            issue_url: None,
            issue_tracker_id: None,
            scheduled_job_id: None,
            model_invocation_owner: None,
            model_invocation_dedup_key: Some(format!(
                "agent.spawn_child:{}",
                durable_request.spawn_request_id
            )),
            model_invocation_request_fingerprint: Some(request_fingerprint.clone()),
            skip_project_model_default,
            model_invocation_purpose:
                rsi_common::model_control::ModelInvocationPurpose::AgentSpawnChild,
            sandbox: sandbox_spec_from_session(&emitter),
            cargo_target_dir: None,
            execution_scratch: None,
            // RSI-006: spawn coordinator inherits eval status from emitter.
            // Production lead emitters carry false; eval-driven epics propagate.
            is_eval: emitter.is_eval,
            skip_context_pipeline: emitter.is_eval,
            capability_class: None,
            // P1.7 D3: tag override semantics — directive.tags replaces emitter tags;
            // None inherits via tags_for(). Both paths produce validated Vec<String>.
            tags: if let Some(ref override_tags) = directive.tags {
                override_tags.clone()
            } else {
                self.tags_for(active, completed, store, emitter_id).await
            },
            // P1.7: binding block populates these from bound_node_id/bound_iteration.
            topology_node_id: bound_node_id,
            topology_iteration: bound_iteration,
            closure_selector: None,
        };

        let req = SpawnRequest {
            config,
            epic_id,
            kind: directive.kind,
            spawn_request_id: durable_request.spawn_request_id,
            child_session_id: durable_request.child_session_id,
            owner_session_id: emitter_id,
        };

        if let Err(error) = store
            .lock()
            .await
            .mark_agent_spawn_queued(durable_request.spawn_request_id)
        {
            return SpawnState::Rejected {
                reason: SpawnRejectReason::StoreError(format!(
                    "durable queued settlement failed before enqueue: {error}"
                )),
            };
        }

        if let Err(e) = self.spawn_tx.send(req).await {
            // The spawn-handler task has gone away (daemon shutting down?).
            // Refund the token so a retry after restart isn't unfairly throttled.
            {
                let mut buckets = self.buckets.lock().await;
                if let Some(b) = buckets.get_mut(&epic_id) {
                    b.tokens = (b.tokens + 1.0).min(TOKEN_BUCKET_CAPACITY as f64);
                }
            }
            let error_class = "spawn_channel_closed";
            if let Err(error) = store
                .lock()
                .await
                .mark_agent_spawn_failed(durable_request.spawn_request_id, error_class)
            {
                tracing::error!(
                    spawn_request_id = %durable_request.spawn_request_id,
                    error = %error,
                    "failed to settle closed spawn channel"
                );
            }
            return SpawnState::Rejected {
                reason: SpawnRejectReason::StoreError(format!("spawn channel closed: {e}")),
            };
        }

        SpawnState::Spawning {
            emitter_id,
            epic_id,
            directive,
            spawn_request_id: durable_request.spawn_request_id,
            child_session_id: durable_request.child_session_id,
            spawn_state: AgentSpawnStateV1::Queued,
            agent_role: durable_request.request.agent_role.clone(),
            epic_spawn_ordinal: durable_request.epic_spawn_ordinal,
            deduplicated: replayed,
            enqueued: true,
            safe_error_class: None,
        }
    }

    /// Returns the tag set for `id`, checking active sessions first, then
    /// completed, then the store. Returns empty vec on miss (non-fatal).
    async fn tags_for(
        &self,
        active: &Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
        completed: &Arc<RwLock<HashMap<Uuid, CompletedSession>>>,
        store: &Arc<tokio::sync::Mutex<Store>>,
        id: Uuid,
    ) -> Vec<String> {
        if let Some(tracked) = active.read().await.get(&id) {
            return tracked.session.tags.clone();
        }
        if let Some(cs) = completed.read().await.get(&id) {
            return cs.session.tags.clone();
        }
        // Fallback: store lookup
        let guard = store.lock().await;
        guard
            .get_session(id)
            .map(|opt| opt.map(|s| s.tags).unwrap_or_default())
            .unwrap_or_default()
    }

    /// TEST-ONLY: advance the synthetic clock used by token-bucket logic.
    #[cfg(test)]
    pub(crate) async fn test_set_now(&self, t: Instant) {
        let mut g = self.test_clock.lock().await;
        *g = Some(t);
    }
}

/// H1-04 (F-011): every agent-spawned child forks its own independent root.
/// The spec requests a worktree allocation unconditionally — inheriting the
/// emitter's concrete sandbox kind when it names one and defaulting to
/// GitWorktree for an ordinary emitter. The fork SOURCE is not chosen here:
/// `launch_agent_child` authenticates the emitter's custody and captures its
/// exact clean HEAD OID, so the child never forks from whatever the shared
/// canonical checkout happens to hold (the Issue #38 state-loss class).
pub(crate) fn sandbox_spec_from_session(session: &Session) -> Option<SandboxSpec> {
    Some(SandboxSpec {
        kind: Some(match session.sandbox_kind {
            Some(kind) if kind != rsi_common::types::SandboxKind::None => kind,
            _ => rsi_common::types::SandboxKind::GitWorktree,
        }),
        branch: None,
    })
}

fn spawn_state_from_record(
    record: AgentSpawnRequestRecord,
    directive: SpawnDirective,
    deduplicated: bool,
    enqueued: bool,
) -> SpawnState {
    SpawnState::Spawning {
        emitter_id: record.owner_session_id,
        epic_id: record.epic_id,
        directive,
        spawn_request_id: record.spawn_request_id,
        child_session_id: record.child_session_id,
        spawn_state: record.state,
        agent_role: record.request.agent_role.clone(),
        epic_spawn_ordinal: record.epic_spawn_ordinal,
        deduplicated,
        enqueued,
        safe_error_class: record.safe_error_class,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use rsi_common::types::{Session, SessionKind, SessionProvider, SessionStatus};
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn now_ts() -> chrono::DateTime<chrono::Utc> {
        chrono::Utc::now()
    }

    fn mk_session(
        id: Uuid,
        kind: SessionKind,
        parent_id: Option<Uuid>,
        lead: Option<Uuid>,
    ) -> Session {
        Session {
            context_fill_pct: None,
            id,
            provider: SessionProvider::Claude,
            claude_session_id: None,
            query: String::new(),
            title: Some(format!("test-{id}")),
            agent_role: None,
            epic_spawn_ordinal: None,
            description: None,
            short_summary: None,
            pending_question: None,
            pending_archive: false,
            working_dir: PathBuf::from("/tmp"),
            git_branch: None,
            status: SessionStatus::Running,
            project_id: None,
            pinned_at: None,
            testing_needed_at: None,
            rotation_disabled_at: None,
            session_kind: kind,
            created_at: now_ts(),
            updated_at: now_ts(),
            cost_usd: None,
            duration_ms: None,
            num_turns: None,
            model: Some("claude-sonnet-5".to_string()),
            input_tokens: None,
            output_tokens: None,
            context_window: None,
            resolved_context_budget: None,
            total_input_tokens: None,
            total_output_tokens: None,
            total_cache_creation_tokens: None,
            total_cache_read_tokens: None,
            stop_reason: None,
            continued_from: None,
            context_usage_confidence: rsi_common::types::ContextUsageConfidence::Missing,
            daemon_input_tokens: None,
            daemon_output_tokens: None,
            handoff_filepath: None,
            active_task: None,
            group_id: None,
            pipeline_artifact: None,
            workflow_id: None,
            workflow_id_override: None,
            rotation_depth: 0,
            retry_attempt: None,
            max_retries: None,
            effort: None,
            issue_identifier: None,
            issue_url: None,
            issue_tracker_id: None,
            scheduled_job_id: None,
            rating: None,
            harness_version_hash: None,
            test_passed: None,
            clippy_passed: None,
            turn_count: None,
            retry_count: None,
            approval_wait_ms: None,
            work_time_ms: None,
            approval_started_at: None,
            sandbox_kind: None,
            sandbox_root: None,
            sandbox_branch: None,
            sandbox_cleanup_state: None,
            tag: String::new(),
            tags: Vec::new(),
            parent_id,
            lead_session_id: lead,
            is_eval: false,
            capability_class: None,
            topology_node_id: None,
            topology_iteration: 0,
            provider_cli_version: None,
            provider_capabilities: Vec::new(),
            thinking_tokens: None,
            service_tier: None,
            cache_creation_1h_tokens: None,
            cache_creation_5m_tokens: None,
            permission_denial_count: None,
            subagent_stats_json: None,
            queued_turn_count: None,
            terminal_reason: None,
        }
    }

    fn open_store() -> (Arc<tokio::sync::Mutex<Store>>, TempDir) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("rsi.db");
        let store = Store::open(&path).expect("open store");
        (Arc::new(tokio::sync::Mutex::new(store)), dir)
    }

    async fn insert(store: &Arc<tokio::sync::Mutex<Store>>, s: &Session) {
        let g = store.lock().await;
        g.insert_session(s).expect("insert session");
    }

    fn directive(kind: SessionKind) -> SpawnDirective {
        SpawnDirective {
            kind,
            provider: None,
            model: None,
            effort: None,
            query: "do thing".to_string(),
            agent_role: None,
            topology_node: None,
            iteration: None,
            tags: None,
        }
    }

    fn empty_runtime() -> (
        Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
        Arc<RwLock<HashMap<Uuid, CompletedSession>>>,
    ) {
        (
            Arc::new(RwLock::new(HashMap::new())),
            Arc::new(RwLock::new(HashMap::new())),
        )
    }

    async fn handle_for_test(
        coord: &SpawnCoordinator,
        emitter_id: Uuid,
        block_hash: u64,
        directive: SpawnDirective,
        store: &Arc<tokio::sync::Mutex<Store>>,
    ) -> SpawnState {
        let (active, completed) = empty_runtime();
        coord
            .handle(
                emitter_id, block_hash, directive, &active, &completed, store,
            )
            .await
    }

    #[tokio::test]
    async fn non_lead_emitter_silently_dropped() {
        let (store, _td) = open_store();
        let (tx, mut rx) = mpsc::channel(8);
        let coord = SpawnCoordinator::new(tx);

        let epic_id = Uuid::new_v4();
        let emitter_id = Uuid::new_v4();
        let other_id = Uuid::new_v4();

        // Epic has lead = other_id, NOT emitter_id.
        insert(
            &store,
            &mk_session(epic_id, SessionKind::Epic, None, Some(other_id)),
        )
        .await;
        insert(
            &store,
            &mk_session(emitter_id, SessionKind::Task, Some(epic_id), None),
        )
        .await;

        let state =
            handle_for_test(&coord, emitter_id, 1, directive(SessionKind::Task), &store).await;
        assert!(matches!(
            state,
            SpawnState::Rejected {
                reason: SpawnRejectReason::NotLead
            }
        ));
        assert!(rx.try_recv().is_err(), "no spawn request should be sent");
    }

    #[test]
    fn explicit_pioneer_model_is_rejected_only_when_cached_catalog_excludes_it() {
        assert!(unavailable_explicit_pioneer_model(Some("vendor/model"), Some(true)).is_none());
        assert!(unavailable_explicit_pioneer_model(Some("vendor/model"), None).is_none());
        assert!(unavailable_explicit_pioneer_model(None, Some(false)).is_none());
        assert!(matches!(
            unavailable_explicit_pioneer_model(Some("missing/model"), Some(false)),
            Some(SpawnRejectReason::PioneerModelUnavailable { model }) if model == "missing/model"
        ));
    }

    /// Seed a tree-root `model_invocations` row and point a session at it.
    /// Raw SQL because `Session`/`insert_session` carry no `model_invocation_id`
    /// field — it is a raw column reachable only via
    /// `Store::session_model_invocation_id`. Column list mirrors the
    /// `model_invocations` schema; string spellings mirror the `parse_*`
    /// helpers in `store/model_control.rs`.
    async fn seed_root_invocation(
        store: &Arc<tokio::sync::Mutex<Store>>,
        session_id: Uuid,
        model_tier: &str,
        effort: &str,
    ) -> Uuid {
        let invocation_id = Uuid::new_v4();
        let g = store.lock().await;
        g.conn
            .execute(
                "INSERT INTO model_invocations
                   (id, purpose, invocation_kind, foreground, paid_risk,
                    admission_status, status, provider, model, backend,
                    model_tier, effort, trigger_source, session_id, created_at)
                 VALUES (?1, 'agent.spawn_child', 'orchestration', 'foreground',
                         'paid_capable', 'admitted', 'running', 'Claude',
                         'claude-sonnet-5', 'Claude', ?2, ?3, 'test', ?4, ?5)",
                rusqlite::params![
                    invocation_id.to_string(),
                    model_tier,
                    effort,
                    session_id.to_string(),
                    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                ],
            )
            .expect("seed root invocation");
        g.conn
            .execute(
                "UPDATE sessions SET model_invocation_id = ?1 WHERE id = ?2",
                rusqlite::params![invocation_id.to_string(), session_id.to_string()],
            )
            .expect("point session at root invocation");
        invocation_id
    }

    #[tokio::test]
    async fn escalating_effort_spawn_is_rejected_not_enqueued() {
        let (store, _td) = open_store();
        let (tx, mut rx) = mpsc::channel(8);
        let coord = SpawnCoordinator::new(tx);

        let epic_id = Uuid::new_v4();
        let emitter_id = Uuid::new_v4();
        insert(
            &store,
            &mk_session(epic_id, SessionKind::Epic, None, Some(emitter_id)),
        )
        .await;
        insert(
            &store,
            &mk_session(emitter_id, SessionKind::Task, Some(epic_id), None),
        )
        .await;

        // Tree root recorded at premium/high — the shape issue #2 reproduces on.
        seed_root_invocation(&store, emitter_id, "premium", "high").await;

        // mk_session's model is "claude-sonnet-5" => classify_model_tier == Premium,
        // so tier ranks EQUAL and only the effort escalation can fire.
        let mut d = directive(SessionKind::Task);
        d.effort = Some("xhigh".to_string());

        let state = handle_for_test(&coord, emitter_id, 1, d, &store).await;

        match state {
            SpawnState::Rejected {
                reason:
                    SpawnRejectReason::OrchestrationEscalationDenied {
                        requested_tier,
                        requested_effort,
                        root_tier,
                        root_effort,
                        detail,
                    },
            } => {
                assert_eq!(requested_tier, "premium");
                assert_eq!(requested_effort.as_deref(), Some("xhigh"));
                assert_eq!(root_tier.as_deref(), Some("premium"));
                assert_eq!(root_effort.as_deref(), Some("high"));
                assert!(
                    detail.contains("effort escalation denied"),
                    "detail: {detail}"
                );
            }
            other => panic!("expected OrchestrationEscalationDenied, got {other:?}"),
        }
        // The ticket's Enqueued -> Rejected assertion: nothing reached the
        // spawn channel, so `agent_spawn_child` cannot report `Enqueued`.
        assert!(rx.try_recv().is_err(), "no spawn request should be sent");
    }

    /// Issue #243: an explicit effort outside the selected model's ladder must
    /// be refused at the spawn boundary instead of being persisted.
    #[tokio::test]
    async fn explicit_effort_outside_model_ladder_is_rejected_not_enqueued() {
        let (store, _td) = open_store();
        let (tx, mut rx) = mpsc::channel(8);
        let coord = SpawnCoordinator::new(tx);

        let epic_id = Uuid::new_v4();
        let emitter_id = Uuid::new_v4();
        insert(
            &store,
            &mk_session(epic_id, SessionKind::Epic, None, Some(emitter_id)),
        )
        .await;
        insert(
            &store,
            &mk_session(emitter_id, SessionKind::Task, Some(epic_id), None),
        )
        .await;

        // The emitter's model is claude-sonnet-5, whose ladder has no `ultra`.
        let mut d = directive(SessionKind::Task);
        d.effort = Some("ultra".to_string());

        let state = handle_for_test(&coord, emitter_id, 7, d, &store).await;

        match state {
            SpawnState::Rejected {
                reason:
                    SpawnRejectReason::UnsupportedChildEffort {
                        ref model,
                        ref effort,
                        valid,
                    },
            } => {
                assert_eq!(model, "claude-sonnet-5");
                assert_eq!(effort, "ultra");
                assert!(!valid.contains(&"ultra"), "ladder must not contain ultra");
                assert!(valid.contains(&"xhigh"));
            }
            other => panic!("expected UnsupportedChildEffort, got {other:?}"),
        }
        assert!(
            rx.try_recv().is_err(),
            "a rejected effort must never reach the spawn channel"
        );
    }

    /// The guard must not over-reject: a supported explicit effort still spawns.
    #[tokio::test]
    async fn explicit_supported_effort_still_enqueues() {
        let (store, _td) = open_store();
        let (tx, mut rx) = mpsc::channel(8);
        let coord = SpawnCoordinator::new(tx);

        let epic_id = Uuid::new_v4();
        let emitter_id = Uuid::new_v4();
        insert(
            &store,
            &mk_session(epic_id, SessionKind::Epic, None, Some(emitter_id)),
        )
        .await;
        insert(
            &store,
            &mk_session(emitter_id, SessionKind::Task, Some(epic_id), None),
        )
        .await;
        seed_root_invocation(&store, emitter_id, "premium", "high").await;

        let mut d = directive(SessionKind::Task);
        d.effort = Some("medium".to_string());

        let state = handle_for_test(&coord, emitter_id, 8, d, &store).await;
        assert!(
            matches!(state, SpawnState::Spawning { .. }),
            "supported effort must still enqueue, got {state:?}"
        );
        let sent = rx.try_recv().expect("spawn request is sent");
        assert_eq!(sent.config.effort.as_deref(), Some("medium"));
    }

    /// Fail-open: a model RSI has no authoritative ladder for keeps the CLI as
    /// the validator, so an effort we cannot refute is not rejected.
    #[tokio::test]
    async fn effort_for_model_without_authoritative_ladder_is_not_rejected() {
        let (store, _td) = open_store();
        let (tx, mut rx) = mpsc::channel(8);
        let coord = SpawnCoordinator::new(tx);

        let epic_id = Uuid::new_v4();
        let emitter_id = Uuid::new_v4();
        insert(
            &store,
            &mk_session(epic_id, SessionKind::Epic, None, Some(emitter_id)),
        )
        .await;
        insert(
            &store,
            &mk_session(emitter_id, SessionKind::Task, Some(epic_id), None),
        )
        .await;

        // Haiku exposes no authoritative ladder to RSI, so enforcement must
        // fail open rather than invent a rejection.
        assert!(
            rsi_common::model_utils::known_effort_ladder("claude-haiku-5").is_none(),
            "precondition: haiku has no authoritative ladder"
        );
        let mut d = directive(SessionKind::Task);
        d.model = Some("claude-haiku-5".to_string());
        d.effort = Some("ultra".to_string());

        let state = handle_for_test(&coord, emitter_id, 9, d, &store).await;
        assert!(
            matches!(state, SpawnState::Spawning { .. }),
            "unknown-capability model must fail open, got {state:?}"
        );
        let sent = rx.try_recv().expect("spawn request is sent");
        assert_eq!(sent.config.effort.as_deref(), Some("ultra"));
    }

    /// An effort INHERITED from the emitter is not the caller's claim. A lead
    /// whose own row already persisted an undefined effort (#243, session
    /// 5faae71c) must still be able to spawn: the unusable inherited value is
    /// dropped for the child rather than rejecting the spawn.
    #[tokio::test]
    async fn inherited_effort_unsupported_by_child_model_is_dropped_not_rejected() {
        let (store, _td) = open_store();
        let (tx, mut rx) = mpsc::channel(8);
        let coord = SpawnCoordinator::new(tx);

        let epic_id = Uuid::new_v4();
        let emitter_id = Uuid::new_v4();
        insert(
            &store,
            &mk_session(epic_id, SessionKind::Epic, None, Some(emitter_id)),
        )
        .await;
        let mut emitter = mk_session(emitter_id, SessionKind::Task, Some(epic_id), None);
        emitter.effort = Some("ultra".to_string());
        insert(&store, &emitter).await;
        seed_root_invocation(&store, emitter_id, "premium", "high").await;

        // No explicit effort: the child inherits the emitter's unusable value.
        let d = directive(SessionKind::Task);

        let state = handle_for_test(&coord, emitter_id, 10, d, &store).await;
        assert!(
            matches!(state, SpawnState::Spawning { .. }),
            "inherited effort must not reject the spawn, got {state:?}"
        );
        let sent = rx.try_recv().expect("spawn request is sent");
        assert_eq!(
            sent.config.effort, None,
            "an inherited effort the child model cannot use is dropped, not persisted"
        );
    }

    #[tokio::test]
    async fn non_escalating_effort_spawn_still_enqueues() {
        let (store, _td) = open_store();
        let (tx, mut rx) = mpsc::channel(8);
        let coord = SpawnCoordinator::new(tx);

        let epic_id = Uuid::new_v4();
        let emitter_id = Uuid::new_v4();
        insert(
            &store,
            &mk_session(epic_id, SessionKind::Epic, None, Some(emitter_id)),
        )
        .await;
        insert(
            &store,
            &mk_session(emitter_id, SessionKind::Task, Some(epic_id), None),
        )
        .await;

        // Root already at xhigh — a request for xhigh is equal rank, not an
        // escalation, so the precheck must not over-block it.
        seed_root_invocation(&store, emitter_id, "premium", "xhigh").await;

        let mut d = directive(SessionKind::Task);
        d.effort = Some("xhigh".to_string());

        let state = handle_for_test(&coord, emitter_id, 1, d, &store).await;
        assert!(
            matches!(state, SpawnState::Spawning { .. }),
            "expected Spawning, got {state:?}"
        );
        let sent = rx.try_recv().expect("spawn request should be enqueued");
        assert_eq!(sent.config.effort.as_deref(), Some("xhigh"));
    }

    #[test]
    fn escalation_reject_reason_renders_actionable_payload() {
        let reason = SpawnRejectReason::OrchestrationEscalationDenied {
            requested_tier: "premium".to_string(),
            requested_effort: Some("xhigh".to_string()),
            root_tier: Some("premium".to_string()),
            root_effort: Some("high".to_string()),
            detail: "orchestration effort escalation denied by tree root effort Some(\"high\")"
                .to_string(),
        };

        let debug = format!("{reason:?}");
        assert!(
            debug.contains("OrchestrationEscalationDenied"),
            "debug: {debug}"
        );
        assert!(debug.contains("xhigh"), "debug: {debug}");
        assert!(debug.contains("high"), "debug: {debug}");

        let display = format!("{reason}");
        assert!(
            display.contains("orchestration escalation denied"),
            "display: {display}"
        );
        assert!(display.contains("xhigh"), "display: {display}");
        assert!(display.contains("high"), "display: {display}");
    }

    #[tokio::test]
    async fn escalation_precheck_missing_lineage_row_is_rejected_not_dropped() {
        let (store, _td) = open_store();
        let (tx, mut rx) = mpsc::channel(8);
        let coord = SpawnCoordinator::new(tx);

        let epic_id = Uuid::new_v4();
        let emitter_id = Uuid::new_v4();
        insert(
            &store,
            &mk_session(epic_id, SessionKind::Epic, None, Some(emitter_id)),
        )
        .await;
        insert(
            &store,
            &mk_session(emitter_id, SessionKind::Task, Some(epic_id), None),
        )
        .await;

        // Point at a model_invocation_id with no matching row — lineage is
        // unresolvable (missing ancestor), which is a deterministic
        // PolicyDenied both here and inside admit_model_invocation.
        {
            let g = store.lock().await;
            g.conn
                .execute(
                    "UPDATE sessions SET model_invocation_id = ?1 WHERE id = ?2",
                    rusqlite::params![Uuid::new_v4().to_string(), emitter_id.to_string()],
                )
                .expect("point session at missing invocation row");
        }

        let state =
            handle_for_test(&coord, emitter_id, 1, directive(SessionKind::Task), &store).await;
        match state {
            SpawnState::Rejected {
                reason: SpawnRejectReason::StoreError(msg),
            } => {
                assert!(
                    msg.starts_with("escalation precheck lineage:"),
                    "msg: {msg}"
                );
            }
            other => panic!("expected StoreError, got {other:?}"),
        }
        assert!(rx.try_recv().is_err(), "no spawn request should be sent");
    }

    #[tokio::test]
    async fn valid_directive_launches_child_with_correct_parent_id() {
        let (store, _td) = open_store();
        let (tx, mut rx) = mpsc::channel(8);
        let coord = SpawnCoordinator::new(tx);

        let epic_id = Uuid::new_v4();
        let emitter_id = Uuid::new_v4();
        // Epic.lead == emitter_id  → emitter IS the lead.
        insert(
            &store,
            &mk_session(epic_id, SessionKind::Epic, None, Some(emitter_id)),
        )
        .await;
        insert(
            &store,
            &mk_session(emitter_id, SessionKind::Task, Some(epic_id), None),
        )
        .await;

        let state =
            handle_for_test(&coord, emitter_id, 2, directive(SessionKind::Task), &store).await;
        match state {
            SpawnState::Spawning {
                epic_id: sid,
                directive: d,
                ..
            } => {
                assert_eq!(sid, epic_id);
                assert_eq!(d.kind, SessionKind::Task);
            }
            other => panic!("expected Spawning, got {other:?}"),
        }

        // The request landed on the spawn channel with parent_id = epic_id.
        let req = rx.try_recv().expect("spawn request enqueued");
        assert_eq!(req.config.parent_id, Some(epic_id));
        assert_eq!(req.epic_id, epic_id);
        assert_eq!(req.kind, SessionKind::Task);
    }

    #[tokio::test]
    async fn child_launch_config_prompt_is_query_body_only_before_launch_assembly() {
        let (store, _td) = open_store();
        let (tx, mut rx) = mpsc::channel(8);
        let coord = SpawnCoordinator::new(tx);

        let epic_id = Uuid::new_v4();
        let emitter_id = Uuid::new_v4();
        insert(
            &store,
            &mk_session(epic_id, SessionKind::Epic, None, Some(emitter_id)),
        )
        .await;

        let mut lead = mk_session(emitter_id, SessionKind::Task, Some(epic_id), None);
        lead.provider = SessionProvider::CodexAppServer;
        insert(&store, &lead).await;

        let mut directive = directive(SessionKind::Task);
        directive.query = "Only the extracted QUERY body.".to_string();

        let state = handle_for_test(&coord, emitter_id, 3, directive, &store).await;
        assert!(matches!(state, SpawnState::Spawning { .. }));

        let req = rx.try_recv().expect("spawn request enqueued");
        assert_eq!(req.config.provider, Some(SessionProvider::CodexAppServer));
        assert_eq!(req.config.query, "Only the extracted QUERY body.");
        assert!(req.config.system_prompt.is_none());
    }

    #[tokio::test]
    async fn agent_spawn_child_uses_requested_cross_provider_and_persists_it() {
        let (store, _td) = open_store();
        let (tx, mut rx) = mpsc::channel(8);
        let coord = SpawnCoordinator::new(tx);
        let (active, completed) = empty_runtime();
        let epic_id = Uuid::new_v4();
        let emitter_id = Uuid::new_v4();
        insert(
            &store,
            &mk_session(epic_id, SessionKind::Epic, None, Some(emitter_id)),
        )
        .await;
        let mut lead = mk_session(emitter_id, SessionKind::Task, Some(epic_id), None);
        lead.provider = SessionProvider::Codex;
        lead.model = Some("gpt-5-codex".to_string());
        lead.effort = Some("xhigh".to_string());
        insert(&store, &lead).await;

        let request = AgentSpawnChildRequestV1 {
            provider: Some(SessionProvider::Claude),
            model: Some("claude-sonnet-5".to_string()),
            effort: Some("high".to_string()),
            ..agent_request("cross-provider", "review this change")
        };
        let state = coord
            .handle_agent(emitter_id, request, &active, &completed, &store)
            .await;
        let spawn_request_id = match state {
            SpawnState::Spawning {
                spawn_request_id, ..
            } => spawn_request_id,
            other => panic!("expected queued cross-provider spawn, got {other:?}"),
        };

        let queued = rx.try_recv().expect("cross-provider request enqueued");
        assert_eq!(queued.config.provider, Some(SessionProvider::Claude));
        assert_eq!(queued.config.model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(queued.config.effort.as_deref(), Some("high"));

        let durable = store
            .lock()
            .await
            .get_agent_spawn_request(spawn_request_id)
            .expect("read durable request")
            .expect("request exists");
        assert_eq!(durable.request.provider, Some(SessionProvider::Claude));
        assert_eq!(durable.request.model.as_deref(), Some("claude-sonnet-5"));
    }

    #[tokio::test]
    async fn directive_spawn_uses_requested_cross_provider() {
        let (store, _td) = open_store();
        let (tx, mut rx) = mpsc::channel(8);
        let coord = SpawnCoordinator::new(tx);
        let epic_id = Uuid::new_v4();
        let emitter_id = Uuid::new_v4();
        insert(
            &store,
            &mk_session(epic_id, SessionKind::Epic, None, Some(emitter_id)),
        )
        .await;
        let mut lead = mk_session(emitter_id, SessionKind::Task, Some(epic_id), None);
        lead.provider = SessionProvider::Codex;
        lead.model = Some("gpt-5-codex".to_string());
        insert(&store, &lead).await;

        let directive = SpawnDirective::parse(
            "<docregblock>\n/spawn_child kind=Task provider=Claude model=claude-sonnet-5\nQUERY:\nreview this change\n</docregblock>",
        )
        .expect("valid directive")
        .expect("spawn directive");
        let state = handle_for_test(&coord, emitter_id, 4, directive, &store).await;
        assert!(matches!(state, SpawnState::Spawning { .. }));

        let queued = rx.try_recv().expect("cross-provider directive enqueued");
        assert_eq!(queued.config.provider, Some(SessionProvider::Claude));
        assert_eq!(queued.config.model.as_deref(), Some("claude-sonnet-5"));
    }

    #[tokio::test]
    async fn docregblock_spawn_enqueues_query_body_only_for_claude_and_antigravity() {
        let block = "<docregblock>\n\
/spawn_child kind=Bug tags=e2e\n\
QUERY:\n\
worker prompt body\n\
</docregblock>";

        for (idx, provider) in [SessionProvider::Claude, SessionProvider::Antigravity]
            .into_iter()
            .enumerate()
        {
            let (store, _td) = open_store();
            let (tx, mut rx) = mpsc::channel(8);
            let coord = SpawnCoordinator::new(tx);

            let epic_id = Uuid::new_v4();
            let emitter_id = Uuid::new_v4();
            insert(
                &store,
                &mk_session(epic_id, SessionKind::Epic, None, Some(emitter_id)),
            )
            .await;

            let mut lead = mk_session(emitter_id, SessionKind::Task, Some(epic_id), None);
            lead.provider = provider;
            insert(&store, &lead).await;

            let directive = SpawnDirective::parse(block).unwrap().unwrap();
            let state =
                handle_for_test(&coord, emitter_id, 30_000 + idx as u64, directive, &store).await;
            assert!(matches!(state, SpawnState::Spawning { .. }));

            let req = rx.try_recv().expect("spawn request enqueued");
            assert_eq!(req.config.provider, Some(provider));
            assert_eq!(req.config.query, "worker prompt body");
            assert_eq!(req.config.system_prompt, None);
            assert!(!req.config.query.contains("<docregblock>"));
            assert!(!req.config.query.contains("/spawn_child"));
            assert!(!req.config.query.contains("QUERY:"));
        }
    }

    #[tokio::test]
    async fn duplicate_directive_block_spawns_once() {
        let (store, _td) = open_store();
        let (tx, mut rx) = mpsc::channel(8);
        let coord = SpawnCoordinator::new(tx);

        let epic_id = Uuid::new_v4();
        let emitter_id = Uuid::new_v4();
        insert(
            &store,
            &mk_session(epic_id, SessionKind::Epic, None, Some(emitter_id)),
        )
        .await;
        insert(
            &store,
            &mk_session(emitter_id, SessionKind::Task, Some(epic_id), None),
        )
        .await;

        let first =
            handle_for_test(&coord, emitter_id, 42, directive(SessionKind::Task), &store).await;
        assert!(matches!(first, SpawnState::Spawning { .. }));

        let second =
            handle_for_test(&coord, emitter_id, 42, directive(SessionKind::Task), &store).await;
        assert!(matches!(
            second,
            SpawnState::Rejected {
                reason: SpawnRejectReason::DuplicateDirective
            }
        ));

        assert!(rx.try_recv().is_ok(), "first request should be enqueued");
        assert!(rx.try_recv().is_err(), "duplicate must not enqueue");
    }

    /// H1-04 (F-011): both spawn funnels — the directive fallback and the
    /// AgentSpawnChild verb — request a fork sandbox for the child even when
    /// the emitter is ordinary (unsandboxed). The fork SOURCE is then
    /// resolved by `launch_agent_child` from the authenticated emitter
    /// custody; neither funnel can reach the canonical rev-parse.
    #[tokio::test]
    async fn h1_v83_spawn_child_fork_custody_directive_and_agent_paths_request_fork_spec() {
        let (store, _td) = open_store();
        let (tx, mut rx) = mpsc::channel(8);
        let coord = SpawnCoordinator::new(tx);

        let epic_id = Uuid::new_v4();
        let emitter_id = Uuid::new_v4();
        let epic = mk_session(epic_id, SessionKind::Epic, None, Some(emitter_id));
        // Ordinary emitter: no sandbox tuple at all.
        let lead = mk_session(emitter_id, SessionKind::Task, Some(epic_id), None);
        assert!(lead.sandbox_kind.is_none());
        insert(&store, &epic).await;
        insert(&store, &lead).await;

        let state =
            handle_for_test(&coord, emitter_id, 71, directive(SessionKind::Task), &store).await;
        assert!(matches!(state, SpawnState::Spawning { .. }));
        let directive_req = rx.try_recv().expect("directive spawn request enqueued");
        let spec = directive_req
            .config
            .sandbox
            .expect("directive path must request a fork sandbox");
        assert_eq!(spec.kind, Some(rsi_common::types::SandboxKind::GitWorktree));

        let (active, completed) = empty_runtime();
        let agent_request = AgentSpawnChildRequestV1 {
            kind: SessionKind::Task,
            provider: None,
            model: None,
            effort: None,
            query: "agent verb fork".to_string(),
            agent_role: None,
            topology_node: None,
            iteration: None,
            tags: None,
            idempotency_key: "h1-7g-agent-fork".to_string(),
        };
        let state = coord
            .handle_agent(emitter_id, agent_request, &active, &completed, &store)
            .await;
        assert!(matches!(state, SpawnState::Spawning { .. }));
        let agent_req = rx.try_recv().expect("agent spawn request enqueued");
        let spec = agent_req
            .config
            .sandbox
            .expect("agent verb path must request a fork sandbox");
        assert_eq!(spec.kind, Some(rsi_common::types::SandboxKind::GitWorktree));
    }

    /// Renamed from `child_inherits_launch_context_from_lead_not_epic` (Q2 in
    /// the P1.3 plan). The runtime-context inheritance from the lead (cwd,
    /// provider, model, effort, project, sandbox) is unchanged. The
    /// `workflow_id` topology is no longer copied — it now lives on the Epic
    /// and is resolved via `effective_topology_with_override` on read.
    #[tokio::test]
    async fn child_inherits_runtime_context_from_lead() {
        let (store, _td) = open_store();
        let (tx, mut rx) = mpsc::channel(8);
        let coord = SpawnCoordinator::new(tx);

        let epic_id = Uuid::new_v4();
        let emitter_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();
        let workflow_id = Uuid::new_v4();

        // P1.3: topology lives on the Epic, not the lead.
        let mut epic = mk_session(epic_id, SessionKind::Epic, None, Some(emitter_id));
        epic.provider = SessionProvider::Claude;
        epic.model = Some("epic-model".to_string());
        epic.working_dir = PathBuf::from("/tmp/epic");
        epic.project_id = None;
        epic.workflow_id = Some(workflow_id);

        let mut lead = mk_session(emitter_id, SessionKind::Task, Some(epic_id), None);
        lead.provider = SessionProvider::Codex;
        lead.model = Some("lead-model".to_string());
        lead.effort = Some("high".to_string());
        lead.working_dir = PathBuf::from("/tmp/lead");
        lead.project_id = Some(project_id);
        lead.workflow_id = None; // no longer the topology source
        lead.sandbox_kind = Some(rsi_common::types::SandboxKind::GitWorktree);

        insert(&store, &epic).await;
        insert(&store, &lead).await;

        let state =
            handle_for_test(&coord, emitter_id, 43, directive(SessionKind::Task), &store).await;
        assert!(matches!(state, SpawnState::Spawning { .. }));

        let req = rx.try_recv().expect("spawn request enqueued");
        // Runtime context inherits from lead (working_dir, provider, model,
        // effort, project, sandbox).
        assert_eq!(req.config.working_dir, Some(PathBuf::from("/tmp/lead")));
        assert_eq!(req.config.provider, Some(SessionProvider::Codex));
        assert_eq!(req.config.model.as_deref(), Some("lead-model"));
        assert_eq!(req.config.effort.as_deref(), Some("high"));
        assert_eq!(req.config.project_id, Some(project_id));
        assert_eq!(req.config.parent_id, Some(epic_id));
        assert!(req.config.sandbox.is_some());

        // P1.3: topology is NOT copied onto the spawned child.
        assert_eq!(req.config.workflow_id, None);
        assert_eq!(req.config.workflow_id_override, None);

        // Verify Epic-rooted derivation: build a SessionsView from the
        // in-memory Epic + lead + (synthetic) child and confirm the child's
        // effective topology resolves to the Epic's workflow_id.
        let mut view: std::collections::HashMap<Uuid, rsi_common::types::Session> =
            std::collections::HashMap::new();
        view.insert(epic.id, epic.clone());
        view.insert(lead.id, lead.clone());
        let child = rsi_common::types::Session {
            parent_id: Some(epic_id),
            workflow_id: None,
            workflow_id_override: None,
            ..mk_session(Uuid::new_v4(), SessionKind::Task, Some(epic_id), None)
        };
        view.insert(child.id, child.clone());
        assert_eq!(
            crate::session::hierarchy_ops::effective_topology(&child, &view),
            Some(workflow_id),
            "child must derive topology from Epic ancestor"
        );
    }

    /// New sibling test: explicitly exercises the Epic-rooted derive-on-read
    /// path. Where `child_inherits_runtime_context_from_lead` confirms the
    /// kill-the-copy invariant on the spawn config, this test confirms that
    /// the persisted child's `workflow_id_override` stays `None` and the
    /// `effective_topology` walk reaches the Epic over the parent_id chain.
    #[tokio::test]
    async fn child_inherits_topology_from_epic_via_effective_topology() {
        let (store, _td) = open_store();
        let (tx, mut rx) = mpsc::channel(8);
        let coord = SpawnCoordinator::new(tx);

        let epic_id = Uuid::new_v4();
        let emitter_id = Uuid::new_v4();
        let workflow_id = Uuid::new_v4();

        // Epic carries the topology; lead does NOT (per new derivation rule).
        let mut epic = mk_session(epic_id, SessionKind::Epic, None, Some(emitter_id));
        epic.workflow_id = Some(workflow_id);

        let mut lead = mk_session(emitter_id, SessionKind::Task, Some(epic_id), None);
        lead.workflow_id = None;

        insert(&store, &epic).await;
        insert(&store, &lead).await;

        let state =
            handle_for_test(&coord, emitter_id, 44, directive(SessionKind::Task), &store).await;
        assert!(matches!(state, SpawnState::Spawning { .. }));

        let req = rx.try_recv().expect("spawn request enqueued");

        // The spawned child's persisted workflow_id is None — the Epic owns
        // the topology, the child derives via effective_topology.
        assert_eq!(req.config.workflow_id, None);
        assert_eq!(req.config.workflow_id_override, None);
        assert_eq!(req.config.parent_id, Some(epic_id));

        // Build a SessionsView and verify derive-on-read resolution.
        let mut view: std::collections::HashMap<Uuid, rsi_common::types::Session> =
            std::collections::HashMap::new();
        view.insert(epic.id, epic.clone());
        view.insert(lead.id, lead.clone());
        let child = rsi_common::types::Session {
            parent_id: Some(epic_id),
            workflow_id: None,
            workflow_id_override: None,
            ..mk_session(Uuid::new_v4(), SessionKind::Task, Some(epic_id), None)
        };
        view.insert(child.id, child.clone());

        assert_eq!(
            crate::session::hierarchy_ops::effective_topology(&child, &view),
            Some(workflow_id),
            "child must derive topology from Epic ancestor (no copy on spawn)"
        );
    }

    #[tokio::test]
    async fn depth_limit_enforced() {
        // Build a chain: root <- epic <- a <- b <- c <- emitter
        // Walk depth from emitter goes: emitter(1) -> c(2) -> b(3) -> a(4) -> epic(5) -> root(6)
        // With MAX_SPAWN_DEPTH = 5, depth=6 should reject.
        let (store, _td) = open_store();
        let (tx, _rx) = mpsc::channel(8);
        let coord = SpawnCoordinator::new(tx);

        let root = Uuid::new_v4();
        let epic = Uuid::new_v4();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();
        let emitter = Uuid::new_v4();

        insert(&store, &mk_session(root, SessionKind::Group, None, None)).await;
        insert(
            &store,
            &mk_session(epic, SessionKind::Epic, Some(root), Some(emitter)),
        )
        .await;
        // Note: Tasks-of-Tasks isn't legal containment per legal_children, but
        // the depth check predates kind validation. We're stress-testing the
        // recursion guard by walking past the Epic up through extra ancestors.
        insert(&store, &mk_session(a, SessionKind::Task, Some(epic), None)).await;
        insert(&store, &mk_session(b, SessionKind::Task, Some(a), None)).await;
        insert(&store, &mk_session(c, SessionKind::Task, Some(b), None)).await;
        insert(
            &store,
            &mk_session(emitter, SessionKind::Task, Some(c), None),
        )
        .await;

        let state = handle_for_test(&coord, emitter, 3, directive(SessionKind::Task), &store).await;
        match state {
            SpawnState::Rejected {
                reason: SpawnRejectReason::DepthLimitExceeded { depth },
            } => {
                assert!(depth > MAX_SPAWN_DEPTH);
            }
            other => panic!("expected DepthLimitExceeded, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn token_bucket_caps_burst() {
        let (store, _td) = open_store();
        let (tx, mut rx) = mpsc::channel(64);
        let coord = SpawnCoordinator::new(tx);

        let epic_id = Uuid::new_v4();
        let emitter_id = Uuid::new_v4();
        insert(
            &store,
            &mk_session(epic_id, SessionKind::Epic, None, Some(emitter_id)),
        )
        .await;
        insert(
            &store,
            &mk_session(emitter_id, SessionKind::Task, Some(epic_id), None),
        )
        .await;

        // Pin the clock so refill is zero across calls.
        coord.test_set_now(Instant::now()).await;
        let (active, completed) = empty_runtime();

        // Drain the bucket: TOKEN_BUCKET_CAPACITY successful spawns.
        for i in 0..TOKEN_BUCKET_CAPACITY {
            let st = coord
                .handle(
                    emitter_id,
                    100 + u64::from(i),
                    directive(SessionKind::Task),
                    &active,
                    &completed,
                    &store,
                )
                .await;
            match st {
                SpawnState::Spawning { .. } => {}
                other => panic!("call {i} expected Spawning, got {other:?}"),
            }
        }

        // The (capacity+1)th must be rate-limited.
        let st = coord
            .handle(
                emitter_id,
                200,
                directive(SessionKind::Task),
                &active,
                &completed,
                &store,
            )
            .await;
        match st {
            SpawnState::Rejected {
                reason: SpawnRejectReason::RateLimited { epic_id: r },
            } => assert_eq!(r, epic_id),
            other => panic!("expected RateLimited, got {other:?}"),
        }

        // And exactly TOKEN_BUCKET_CAPACITY requests landed on the channel.
        let mut count = 0;
        while rx.try_recv().is_ok() {
            count += 1;
        }
        assert_eq!(count, TOKEN_BUCKET_CAPACITY as usize);
    }

    #[tokio::test]
    async fn emitter_with_no_parent_rejected_as_not_lead() {
        let (store, _td) = open_store();
        let (tx, mut rx) = mpsc::channel(8);
        let coord = SpawnCoordinator::new(tx);

        let emitter_id = Uuid::new_v4();
        // Top-level Standard with no parent → not under any Epic.
        insert(
            &store,
            &mk_session(emitter_id, SessionKind::Standard, None, None),
        )
        .await;

        let st = handle_for_test(&coord, emitter_id, 4, directive(SessionKind::Task), &store).await;
        assert!(matches!(
            st,
            SpawnState::Rejected {
                reason: SpawnRejectReason::NotLead
            }
        ));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn container_emitter_rejected() {
        let (store, _td) = open_store();
        let (tx, _rx) = mpsc::channel(8);
        let coord = SpawnCoordinator::new(tx);

        let group_id = Uuid::new_v4();
        let epic_id = Uuid::new_v4();
        insert(
            &store,
            &mk_session(group_id, SessionKind::Group, None, None),
        )
        .await;
        // An Epic emitting /spawn_child is a misuse — daemon never spawns
        // an Epic provider so it can't legitimately emit text. Reject.
        insert(
            &store,
            &mk_session(epic_id, SessionKind::Epic, Some(group_id), None),
        )
        .await;

        let st = handle_for_test(&coord, epic_id, 5, directive(SessionKind::Task), &store).await;
        assert!(matches!(
            st,
            SpawnState::Rejected {
                reason: SpawnRejectReason::EmitterNotLeaf
            }
        ));
    }

    #[tokio::test]
    async fn missing_emitter_rejected() {
        let (store, _td) = open_store();
        let (tx, _rx) = mpsc::channel(8);
        let coord = SpawnCoordinator::new(tx);

        let phantom = Uuid::new_v4();
        let st = handle_for_test(&coord, phantom, 6, directive(SessionKind::Task), &store).await;
        assert!(matches!(
            st,
            SpawnState::Rejected {
                reason: SpawnRejectReason::EmitterNotFound
            }
        ));
    }

    // ─── P1.6: spawn coordinator tag inheritance ─────────────────────────

    /// AC4i: Spawn coordinator reads emitter's tag set and populates
    /// LaunchConfig.tags. When the emitter carries ["ci", "infra"],
    /// the child's LaunchConfig.tags must match.
    #[tokio::test]
    async fn child_inherits_emitter_tags() {
        let (store, _td) = open_store();
        let (tx, mut rx) = mpsc::channel(8);
        let coord = SpawnCoordinator::new(tx);

        let epic_id = Uuid::new_v4();
        let emitter_id = Uuid::new_v4();

        let mut epic = mk_session(epic_id, SessionKind::Epic, None, Some(emitter_id));
        epic.tags = vec!["epic-tag".to_string()];

        let mut lead = mk_session(emitter_id, SessionKind::Task, Some(epic_id), None);
        // Emitter carries two tags — child must inherit them.
        lead.tags = vec!["ci".to_string(), "infra".to_string()];

        insert(&store, &epic).await;
        insert(&store, &lead).await;

        // Also insert into session_tags — tags_for() reads from the join table via
        // get_session's hydrate_tags path, so insert_session alone is not enough.
        {
            let g = store.lock().await;
            for tag in &lead.tags {
                g.conn
                    .execute(
                        "INSERT OR IGNORE INTO session_tags (session_id, tag) VALUES (?1, ?2)",
                        rusqlite::params![emitter_id.to_string(), tag],
                    )
                    .expect("insert tag");
            }
        }

        let state =
            handle_for_test(&coord, emitter_id, 43, directive(SessionKind::Task), &store).await;
        assert!(matches!(state, SpawnState::Spawning { .. }));

        let req = rx.try_recv().expect("spawn request enqueued");
        // tags_for() reads from the store (session_tags join table).
        // The emitter's tags are ["ci", "infra"] — child must carry them.
        assert_eq!(req.config.tags, vec!["ci", "infra"]);
    }

    // ─── Phase 7 (P1.7) topology binding tests ──────────────────────────────

    /// Build a minimal Topology for use in coordinator tests.
    fn mk_topology(
        id: Uuid,
        nodes: Vec<rsi_common::types::TopologyNode>,
        edges: Vec<rsi_common::types::TopologyEdge>,
    ) -> rsi_common::types::Topology {
        rsi_common::types::Topology {
            id,
            name: format!("topo-{id}"),
            definition: rsi_common::types::TopologyDefinition {
                nodes,
                edges,
                until: None,
            },
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    /// Build a topology node with given id and kind.
    fn mk_node(id: &str, kind: SessionKind) -> rsi_common::types::TopologyNode {
        rsi_common::types::TopologyNode {
            id: id.to_string(),
            label: id.to_string(),
            kind,
            max_iterations: None,
            prereqs: vec![],
            on_failure: None,
            params: std::collections::HashMap::new(),
        }
    }

    /// Build a topology edge from → to.
    fn mk_edge(from: &str, to: &str) -> rsi_common::types::TopologyEdge {
        rsi_common::types::TopologyEdge {
            from: from.to_string(),
            to: to.to_string(),
            loop_edge: false,
        }
    }

    async fn handle_with_maps(
        coord: &SpawnCoordinator,
        emitter_id: Uuid,
        block_hash: u64,
        directive: SpawnDirective,
        active: &Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
        completed: &Arc<RwLock<HashMap<Uuid, CompletedSession>>>,
        store: &Arc<tokio::sync::Mutex<Store>>,
    ) -> SpawnState {
        coord
            .handle(emitter_id, block_hash, directive, active, completed, store)
            .await
    }

    /// Setup: epic + lead in store, epic has workflow_id = topology_id.
    async fn setup_topology_test(
        store: &Arc<tokio::sync::Mutex<Store>>,
        topology_id: Uuid,
        nodes: Vec<rsi_common::types::TopologyNode>,
        edges: Vec<rsi_common::types::TopologyEdge>,
    ) -> (Uuid, Uuid) {
        let epic_id = Uuid::new_v4();
        let emitter_id = Uuid::new_v4();
        let mut epic = mk_session(epic_id, SessionKind::Epic, None, Some(emitter_id));
        epic.workflow_id = Some(topology_id);
        let lead = mk_session(emitter_id, SessionKind::Task, Some(epic_id), None);

        let topology = mk_topology(topology_id, nodes, edges);
        let g = store.lock().await;
        g.insert_topology(&topology).expect("insert topology");
        g.insert_session(&epic).expect("insert epic");
        g.insert_session(&lead).expect("insert lead");
        drop(g);

        (epic_id, emitter_id)
    }

    #[tokio::test]
    async fn binds_node_and_auto_increments_iteration() {
        // No prior sessions → auto-increment: 0 + 1 = 1.
        let (store, _td) = open_store();
        let (tx, mut rx) = mpsc::channel(8);
        let coord = SpawnCoordinator::new(tx);
        let topology_id = Uuid::new_v4();
        let (_, emitter_id) = setup_topology_test(
            &store,
            topology_id,
            vec![mk_node("plan_v1", SessionKind::Task)],
            vec![],
        )
        .await;

        let mut d = directive(SessionKind::Task);
        d.topology_node = Some("plan_v1".to_string());
        // No explicit iteration → auto-increment.
        let state = handle_for_test(&coord, emitter_id, 200, d, &store).await;
        assert!(matches!(state, SpawnState::Spawning { .. }));
        let req = rx.try_recv().expect("spawn request enqueued");
        assert_eq!(req.config.topology_node_id, Some("plan_v1".to_string()));
        assert_eq!(req.config.topology_iteration, 1);
    }

    #[tokio::test]
    async fn binds_node_with_explicit_iteration() {
        let (store, _td) = open_store();
        let (tx, mut rx) = mpsc::channel(8);
        let coord = SpawnCoordinator::new(tx);
        let topology_id = Uuid::new_v4();
        let (_, emitter_id) = setup_topology_test(
            &store,
            topology_id,
            vec![mk_node("plan_v1", SessionKind::Task)],
            vec![],
        )
        .await;

        let mut d = directive(SessionKind::Task);
        d.topology_node = Some("plan_v1".to_string());
        d.iteration = Some(5);
        let state = handle_for_test(&coord, emitter_id, 201, d, &store).await;
        assert!(matches!(state, SpawnState::Spawning { .. }));
        let req = rx.try_recv().expect("spawn request enqueued");
        assert_eq!(req.config.topology_iteration, 5);
    }

    #[tokio::test]
    async fn rejects_kind_mismatch() {
        let (store, _td) = open_store();
        let (tx, _rx) = mpsc::channel(8);
        let coord = SpawnCoordinator::new(tx);
        let topology_id = Uuid::new_v4();
        let (_, emitter_id) = setup_topology_test(
            &store,
            topology_id,
            vec![mk_node("plan_v1", SessionKind::Research)],
            vec![],
        )
        .await;

        let mut d = directive(SessionKind::Task); // mismatch: node is Research
        d.topology_node = Some("plan_v1".to_string());
        let state = handle_for_test(&coord, emitter_id, 202, d, &store).await;
        assert!(
            matches!(
                state,
                SpawnState::Rejected {
                    reason: SpawnRejectReason::NodeKindMismatch { .. }
                }
            ),
            "expected NodeKindMismatch, got {state:?}"
        );
    }

    #[tokio::test]
    async fn rejects_unsatisfied_prereqs() {
        let (store, _td) = open_store();
        let (tx, _rx) = mpsc::channel(8);
        let coord = SpawnCoordinator::new(tx);
        let topology_id = Uuid::new_v4();
        // Edge: plan_v1 → impl_v1; impl_v1 requires plan_v1 to be Completed.
        let (_, emitter_id) = setup_topology_test(
            &store,
            topology_id,
            vec![
                mk_node("plan_v1", SessionKind::Task),
                mk_node("impl_v1", SessionKind::Task),
            ],
            vec![mk_edge("plan_v1", "impl_v1")],
        )
        .await;

        // Try to spawn impl_v1 without any plan_v1 Completed.
        let mut d = directive(SessionKind::Task);
        d.topology_node = Some("impl_v1".to_string());
        let state = handle_for_test(&coord, emitter_id, 203, d, &store).await;
        match &state {
            SpawnState::Rejected {
                reason: SpawnRejectReason::PrereqsNotSatisfied { missing },
            } => {
                assert!(missing.contains(&"plan_v1".to_string()));
            }
            other => panic!("expected PrereqsNotSatisfied, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_iteration_cap_global() {
        let (store, _td) = open_store();
        let (tx, _rx) = mpsc::channel(8);
        let coord = SpawnCoordinator::new(tx);
        let topology_id = Uuid::new_v4();
        let (_, emitter_id) = setup_topology_test(
            &store,
            topology_id,
            vec![mk_node("plan_v1", SessionKind::Task)],
            vec![],
        )
        .await;

        let mut d = directive(SessionKind::Task);
        d.topology_node = Some("plan_v1".to_string());
        d.iteration = Some(33); // > MAX_ITERATIONS = 32
        let state = handle_for_test(&coord, emitter_id, 204, d, &store).await;
        match &state {
            SpawnState::Rejected {
                reason: SpawnRejectReason::IterationCapExceeded { iteration, cap },
            } => {
                assert_eq!(*iteration, 33);
                assert_eq!(*cap, MAX_ITERATIONS);
            }
            other => panic!("expected IterationCapExceeded, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_iteration_cap_node() {
        let (store, _td) = open_store();
        let (tx, _rx) = mpsc::channel(8);
        let coord = SpawnCoordinator::new(tx);
        let topology_id = Uuid::new_v4();
        let mut node = mk_node("plan_v1", SessionKind::Task);
        node.max_iterations = Some(2); // node-level cap of 2
        let (_, emitter_id) = setup_topology_test(&store, topology_id, vec![node], vec![]).await;

        let mut d = directive(SessionKind::Task);
        d.topology_node = Some("plan_v1".to_string());
        d.iteration = Some(3); // > node cap of 2
        let state = handle_for_test(&coord, emitter_id, 205, d, &store).await;
        match &state {
            SpawnState::Rejected {
                reason: SpawnRejectReason::IterationCapExceeded { iteration, cap },
            } => {
                assert_eq!(*iteration, 3);
                assert_eq!(*cap, 2);
            }
            other => panic!("expected IterationCapExceeded(cap=2), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unbound_spawn_no_topology() {
        // Epic has no topology (workflow_id = None), directive has no node → unbound spawn.
        let (store, _td) = open_store();
        let (tx, mut rx) = mpsc::channel(8);
        let coord = SpawnCoordinator::new(tx);

        let epic_id = Uuid::new_v4();
        let emitter_id = Uuid::new_v4();
        let epic = mk_session(epic_id, SessionKind::Epic, None, Some(emitter_id));
        let lead = mk_session(emitter_id, SessionKind::Task, Some(epic_id), None);
        insert(&store, &epic).await;
        insert(&store, &lead).await;

        let d = directive(SessionKind::Task); // no topology_node
        let state = handle_for_test(&coord, emitter_id, 206, d, &store).await;
        assert!(matches!(state, SpawnState::Spawning { .. }));
        let req = rx.try_recv().expect("spawn request enqueued");
        assert_eq!(req.config.topology_node_id, None);
        assert_eq!(req.config.topology_iteration, 0);
    }

    #[tokio::test]
    async fn rejects_node_on_topologyless_epic() {
        // Epic has no topology, directive names a node → NoTopologyOnEpic.
        let (store, _td) = open_store();
        let (tx, _rx) = mpsc::channel(8);
        let coord = SpawnCoordinator::new(tx);

        let epic_id = Uuid::new_v4();
        let emitter_id = Uuid::new_v4();
        let epic = mk_session(epic_id, SessionKind::Epic, None, Some(emitter_id));
        let lead = mk_session(emitter_id, SessionKind::Task, Some(epic_id), None);
        insert(&store, &epic).await;
        insert(&store, &lead).await;

        let mut d = directive(SessionKind::Task);
        d.topology_node = Some("plan_v1".to_string());
        let state = handle_for_test(&coord, emitter_id, 207, d, &store).await;
        assert!(
            matches!(
                state,
                SpawnState::Rejected {
                    reason: SpawnRejectReason::NoTopologyOnEpic { .. }
                }
            ),
            "expected NoTopologyOnEpic, got {state:?}"
        );
    }

    #[tokio::test]
    async fn node_absent_from_topology_rejected() {
        // Epic has a topology but the directive names a node not in it.
        let (store, _td) = open_store();
        let (tx, _rx) = mpsc::channel(8);
        let coord = SpawnCoordinator::new(tx);
        let topology_id = Uuid::new_v4();
        let (_, emitter_id) = setup_topology_test(
            &store,
            topology_id,
            vec![mk_node("plan_v1", SessionKind::Task)],
            vec![],
        )
        .await;

        let mut d = directive(SessionKind::Task);
        d.topology_node = Some("nonexistent_node".to_string());
        let state = handle_for_test(&coord, emitter_id, 208, d, &store).await;
        assert!(
            matches!(
                state,
                SpawnState::Rejected {
                    reason: SpawnRejectReason::TopologyNodeNotFound { .. }
                }
            ),
            "expected TopologyNodeNotFound, got {state:?}"
        );
    }

    fn agent_request(key: &str, query: &str) -> AgentSpawnChildRequestV1 {
        AgentSpawnChildRequestV1 {
            kind: SessionKind::Task,
            provider: None,
            model: None,
            effort: None,
            query: query.to_string(),
            agent_role: None,
            topology_node: None,
            iteration: None,
            tags: None,
            idempotency_key: key.to_string(),
        }
    }

    #[tokio::test]
    async fn agent_spawn_child_exact_replay_returns_reserved_ids_without_reenqueue() {
        let (store, _td) = open_store();
        let (tx, mut rx) = mpsc::channel(8);
        let coord = SpawnCoordinator::new(tx);
        let (active, completed) = empty_runtime();
        let epic_id = Uuid::new_v4();
        let lead_id = Uuid::new_v4();
        insert(
            &store,
            &mk_session(epic_id, SessionKind::Epic, None, Some(lead_id)),
        )
        .await;
        insert(
            &store,
            &mk_session(lead_id, SessionKind::Task, Some(epic_id), None),
        )
        .await;

        let mut request = agent_request("stable-key", "work");
        request.agent_role = Some("  Principal   Reviewer  ".into());
        let first = coord
            .handle_agent(lead_id, request.clone(), &active, &completed, &store)
            .await;
        let second = coord
            .handle_agent(lead_id, request, &active, &completed, &store)
            .await;
        let ids = |state: SpawnState| match state {
            SpawnState::Spawning {
                spawn_request_id,
                child_session_id,
                agent_role,
                epic_spawn_ordinal,
                deduplicated,
                ..
            } => (
                spawn_request_id,
                child_session_id,
                agent_role,
                epic_spawn_ordinal,
                deduplicated,
            ),
            other => panic!("unexpected spawn state: {other:?}"),
        };
        let first_ids = ids(first);
        let second_ids = ids(second);
        assert_eq!((first_ids.0, first_ids.1), (second_ids.0, second_ids.1));
        assert_eq!(first_ids.2.as_deref(), Some("Principal Reviewer"));
        assert_eq!(first_ids.2, second_ids.2);
        assert_eq!(first_ids.3, 1);
        assert_eq!(first_ids.3, second_ids.3);
        assert!(!first_ids.4);
        assert!(second_ids.4);
        let queued = rx.try_recv().expect("first request enqueued");
        assert_eq!(queued.spawn_request_id, first_ids.0);
        assert_eq!(queued.child_session_id, first_ids.1);
        assert!(rx.try_recv().is_err(), "exact retry must not re-enqueue");

        let durable = store
            .lock()
            .await
            .get_agent_spawn_request(first_ids.0)
            .expect("read request")
            .expect("request row");
        assert_eq!(durable.child_session_id, first_ids.1);
        assert_eq!(durable.state, AgentSpawnStateV1::Queued);
        assert_eq!(
            durable.request.agent_role.as_deref(),
            Some("Principal Reviewer")
        );
        assert_eq!(durable.epic_spawn_ordinal, 1);
    }

    #[tokio::test]
    async fn agent_spawn_child_changed_replay_conflicts() {
        let (store, _td) = open_store();
        let (tx, _rx) = mpsc::channel(8);
        let coord = SpawnCoordinator::new(tx);
        let (active, completed) = empty_runtime();
        let epic_id = Uuid::new_v4();
        let lead_id = Uuid::new_v4();
        insert(
            &store,
            &mk_session(epic_id, SessionKind::Epic, None, Some(lead_id)),
        )
        .await;
        insert(
            &store,
            &mk_session(lead_id, SessionKind::Task, Some(epic_id), None),
        )
        .await;
        assert!(matches!(
            coord
                .handle_agent(
                    lead_id,
                    agent_request("stable-key", "original"),
                    &active,
                    &completed,
                    &store,
                )
                .await,
            SpawnState::Spawning { .. }
        ));

        let mut changed_role = agent_request("stable-key", "original");
        changed_role.agent_role = Some("Planner".into());
        let changed = coord
            .handle_agent(lead_id, changed_role, &active, &completed, &store)
            .await;
        assert!(matches!(
            changed,
            SpawnState::Rejected {
                reason: SpawnRejectReason::StoreError(ref message)
            } if message.contains("agent_spawn_idempotency_conflict")
        ));
        let changed = coord
            .handle_agent(
                lead_id,
                agent_request("stable-key", "changed"),
                &active,
                &completed,
                &store,
            )
            .await;
        assert!(matches!(
            changed,
            SpawnState::Rejected {
                reason: SpawnRejectReason::StoreError(ref message)
            } if message.contains("agent_spawn_idempotency_conflict")
        ));
    }

    #[tokio::test]
    async fn agent_spawn_child_provider_changed_replay_conflicts() {
        let (store, _td) = open_store();
        let (tx, _rx) = mpsc::channel(8);
        let coord = SpawnCoordinator::new(tx);
        let (active, completed) = empty_runtime();
        let epic_id = Uuid::new_v4();
        let lead_id = Uuid::new_v4();
        insert(
            &store,
            &mk_session(epic_id, SessionKind::Epic, None, Some(lead_id)),
        )
        .await;
        insert(
            &store,
            &mk_session(lead_id, SessionKind::Task, Some(epic_id), None),
        )
        .await;
        assert!(matches!(
            coord
                .handle_agent(
                    lead_id,
                    agent_request("stable-key", "original"),
                    &active,
                    &completed,
                    &store,
                )
                .await,
            SpawnState::Spawning { .. }
        ));

        let changed = AgentSpawnChildRequestV1 {
            provider: Some(SessionProvider::Codex),
            ..agent_request("stable-key", "original")
        };
        let state = coord
            .handle_agent(lead_id, changed, &active, &completed, &store)
            .await;
        assert!(matches!(
            state,
            SpawnState::Rejected {
                reason: SpawnRejectReason::StoreError(ref message)
            } if message.contains("agent_spawn_idempotency_conflict")
        ));
    }

    #[tokio::test]
    async fn agent_spawn_child_launch_queue_failure_settles_typed_failed_replay() {
        let (store, _td) = open_store();
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let coord = SpawnCoordinator::new(tx);
        let (active, completed) = empty_runtime();
        let epic_id = Uuid::new_v4();
        let lead_id = Uuid::new_v4();
        insert(
            &store,
            &mk_session(epic_id, SessionKind::Epic, None, Some(lead_id)),
        )
        .await;
        insert(
            &store,
            &mk_session(lead_id, SessionKind::Task, Some(epic_id), None),
        )
        .await;
        let first = tokio::time::timeout(
            Duration::from_secs(5),
            coord.handle_agent(
                lead_id,
                agent_request("closed-channel", "work"),
                &active,
                &completed,
                &store,
            ),
        )
        .await
        .expect("closed channel settles without a bucket/store deadlock");
        assert!(matches!(first, SpawnState::Rejected { .. }));

        let replay = coord
            .handle_agent(
                lead_id,
                agent_request("closed-channel", "work"),
                &active,
                &completed,
                &store,
            )
            .await;
        assert!(matches!(
            replay,
            SpawnState::Spawning {
                spawn_state: AgentSpawnStateV1::Failed,
                deduplicated: true,
                enqueued: false,
                safe_error_class: Some(ref error),
                ..
            } if error == "spawn_channel_closed"
        ));
    }

    #[tokio::test]
    async fn agent_spawn_reservation_error_refunds_token_without_store_lock_deadlock() {
        let (store, _td) = open_store();
        let (tx, _rx) = mpsc::channel(TOKEN_BUCKET_CAPACITY as usize);
        let coord = SpawnCoordinator::new(tx);
        let (active, completed) = empty_runtime();
        let epic_id = Uuid::new_v4();
        let lead_id = Uuid::new_v4();
        insert(
            &store,
            &mk_session(epic_id, SessionKind::Epic, None, Some(lead_id)),
        )
        .await;
        insert(
            &store,
            &mk_session(lead_id, SessionKind::Task, Some(epic_id), None),
        )
        .await;
        store
            .lock()
            .await
            .conn
            .execute_batch(
                "CREATE TRIGGER test_reject_agent_spawn_reservation
                 BEFORE INSERT ON agent_spawn_requests
                 BEGIN SELECT RAISE(ABORT,'injected reservation failure'); END;",
            )
            .expect("install reservation failure trigger");

        let failed = tokio::time::timeout(
            Duration::from_secs(5),
            coord.handle_agent(
                lead_id,
                agent_request("reservation-failure", "work"),
                &active,
                &completed,
                &store,
            ),
        )
        .await
        .expect("reservation failure settles without a store/bucket deadlock");
        assert!(matches!(
            failed,
            SpawnState::Rejected {
                reason: SpawnRejectReason::StoreError(ref message)
            } if message.contains("injected reservation failure")
        ));
        store
            .lock()
            .await
            .conn
            .execute_batch("DROP TRIGGER test_reject_agent_spawn_reservation;")
            .expect("remove reservation failure trigger");

        for ordinal in 0..TOKEN_BUCKET_CAPACITY {
            let state = coord
                .handle_agent(
                    lead_id,
                    agent_request(&format!("after-failure-{ordinal}"), "work"),
                    &active,
                    &completed,
                    &store,
                )
                .await;
            assert!(
                matches!(state, SpawnState::Spawning { enqueued: true, .. }),
                "failed reservation must refund its token; spawn {ordinal}: {state:?}"
            );
        }
    }

    #[tokio::test]
    async fn agent_spawn_child_persists_queued_before_main_loop_publication() {
        let (store, _td) = open_store();
        let (tx, mut rx) = mpsc::channel(8);
        let coord = SpawnCoordinator::new(tx);
        let (active, completed) = empty_runtime();
        let epic_id = Uuid::new_v4();
        let lead_id = Uuid::new_v4();
        insert(
            &store,
            &mk_session(epic_id, SessionKind::Epic, None, Some(lead_id)),
        )
        .await;
        insert(
            &store,
            &mk_session(lead_id, SessionKind::Task, Some(epic_id), None),
        )
        .await;
        store
            .lock()
            .await
            .conn
            .execute_batch(
                "CREATE TRIGGER test_reject_agent_spawn_queued
                 BEFORE UPDATE OF state ON agent_spawn_requests
                 WHEN NEW.state='queued'
                 BEGIN SELECT RAISE(ABORT,'test queued settlement failure'); END;",
            )
            .expect("install queued settlement failpoint");

        let state = coord
            .handle_agent(
                lead_id,
                agent_request("queued-before-publish", "work"),
                &active,
                &completed,
                &store,
            )
            .await;
        assert!(matches!(
            state,
            SpawnState::Rejected {
                reason: SpawnRejectReason::StoreError(ref message)
            } if message.contains("durable queued settlement failed before enqueue")
        ));
        assert!(
            rx.try_recv().is_err(),
            "failed queued settlement must not be observable by the main loop"
        );

        store
            .lock()
            .await
            .conn
            .execute_batch("DROP TRIGGER test_reject_agent_spawn_queued;")
            .expect("remove queued settlement failpoint");
        assert_eq!(
            coord
                .reconcile_incomplete(&active, &completed, &store)
                .await
                .expect("restart reconciliation"),
            1
        );
        let request = rx.try_recv().expect("one restart enqueue");
        let durable = store
            .lock()
            .await
            .get_agent_spawn_request(request.spawn_request_id)
            .expect("load request")
            .expect("request exists");
        assert_eq!(durable.state, AgentSpawnStateV1::Queued);
        assert!(rx.try_recv().is_err(), "restart must enqueue exactly once");
    }

    #[tokio::test]
    async fn agent_spawn_child_restart_requeues_incomplete_reservation_once() {
        let (store, _td) = open_store();
        let (first_tx, mut first_rx) = mpsc::channel(8);
        let first_coord = SpawnCoordinator::new(first_tx);
        let (active, completed) = empty_runtime();
        let epic_id = Uuid::new_v4();
        let lead_id = Uuid::new_v4();
        insert(
            &store,
            &mk_session(epic_id, SessionKind::Epic, None, Some(lead_id)),
        )
        .await;
        insert(
            &store,
            &mk_session(lead_id, SessionKind::Task, Some(epic_id), None),
        )
        .await;
        let request = AgentSpawnChildRequestV1 {
            provider: Some(SessionProvider::Claude),
            model: Some("claude-sonnet-5".to_string()),
            effort: Some("high".to_string()),
            ..agent_request("restart-requeue", "work")
        };
        let state = first_coord
            .handle_agent(lead_id, request.clone(), &active, &completed, &store)
            .await;
        let SpawnState::Spawning {
            spawn_request_id,
            child_session_id,
            ..
        } = state
        else {
            panic!("expected queued durable spawn")
        };
        let prior = first_rx.try_recv().expect("prior in-memory request");
        assert_eq!(prior.config.provider, Some(SessionProvider::Claude));
        drop(first_coord);
        drop(first_rx);

        let (restart_tx, mut restart_rx) = mpsc::channel(8);
        let restart_coord = SpawnCoordinator::new(restart_tx);
        assert_eq!(
            restart_coord
                .reconcile_incomplete(&active, &completed, &store)
                .await
                .expect("startup reconciliation"),
            1
        );
        let requeued = restart_rx.try_recv().expect("requeued after restart");
        assert_eq!(requeued.spawn_request_id, spawn_request_id);
        assert_eq!(requeued.child_session_id, child_session_id);
        assert_eq!(requeued.config.provider, Some(SessionProvider::Claude));
        assert_eq!(requeued.config.model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(requeued.config.effort.as_deref(), Some("high"));
        assert!(restart_rx.try_recv().is_err());

        let replay = restart_coord
            .handle_agent(lead_id, request, &active, &completed, &store)
            .await;
        assert!(matches!(
            replay,
            SpawnState::Spawning {
                spawn_request_id: replay_id,
                child_session_id: replay_child_id,
                enqueued: false,
                deduplicated: true,
                ..
            } if replay_id == spawn_request_id && replay_child_id == child_session_id
        ));
        assert!(restart_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn agent_spawn_child_restart_accepts_legacy_request_json_fingerprint() {
        let (store, _td) = open_store();
        let (tx, mut rx) = mpsc::channel(8);
        let coord = SpawnCoordinator::new(tx);
        let (active, completed) = empty_runtime();
        let epic_id = Uuid::new_v4();
        let lead_id = Uuid::new_v4();
        insert(
            &store,
            &mk_session(epic_id, SessionKind::Epic, None, Some(lead_id)),
        )
        .await;
        insert(
            &store,
            &mk_session(lead_id, SessionKind::Task, Some(epic_id), None),
        )
        .await;

        // This is the exact request JSON and fingerprint shape persisted before
        // `provider` existed. Reconciliation must preserve it so a restart does
        // not turn a valid reservation into an idempotency conflict.
        let legacy_json = r#"{"kind":"Task","model":null,"effort":null,"query":"legacy work","topology_node":null,"iteration":null,"tags":null,"idempotency_key":"legacy-key"}"#;
        let legacy_request: AgentSpawnChildRequestV1 =
            serde_json::from_str(legacy_json).expect("parse pre-provider request JSON");
        assert_eq!(legacy_request.provider, None);
        assert_eq!(
            serde_json::to_string(&legacy_request).expect("serialize legacy request"),
            legacy_json
        );
        let idempotency_digest = hash_request_fingerprint(&[
            "agent-spawn-idempotency-v1",
            &lead_id.to_string(),
            &legacy_request.idempotency_key,
        ]);
        let legacy_fingerprint = hash_request_fingerprint(&[
            "agent-spawn-request-v1",
            &lead_id.to_string(),
            legacy_json,
        ]);
        let spawn_request_id = Uuid::new_v4();
        let child_session_id = Uuid::new_v4();
        let outcome = store
            .lock()
            .await
            .reserve_agent_spawn_request(
                lead_id,
                &idempotency_digest,
                &legacy_fingerprint,
                &legacy_request,
                epic_id,
                spawn_request_id,
                child_session_id,
            )
            .expect("persist legacy reservation");
        assert!(matches!(outcome, ReserveAgentSpawnOutcome::Reserved(_)));

        assert_eq!(
            coord
                .reconcile_incomplete(&active, &completed, &store)
                .await
                .expect("reconcile legacy reservation"),
            1
        );
        let queued = rx.try_recv().expect("reconciliation queues legacy child");
        assert_eq!(queued.spawn_request_id, spawn_request_id);
        assert_eq!(queued.child_session_id, child_session_id);

        let replay = coord
            .handle_agent(lead_id, legacy_request, &active, &completed, &store)
            .await;
        assert!(matches!(
            replay,
            SpawnState::Spawning {
                spawn_request_id: replay_id,
                child_session_id: replay_child_id,
                deduplicated: true,
                enqueued: false,
                ..
            } if replay_id == spawn_request_id && replay_child_id == child_session_id
        ));
        assert!(rx.try_recv().is_err(), "legacy replay must not re-enqueue");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn agent_spawn_child_concurrent_identical_requests_enqueue_once() {
        let (store, _td) = open_store();
        let (tx, mut rx) = mpsc::channel(8);
        let coord = Arc::new(SpawnCoordinator::new(tx));
        let (active, completed) = empty_runtime();
        let epic_id = Uuid::new_v4();
        let lead_id = Uuid::new_v4();
        insert(
            &store,
            &mk_session(epic_id, SessionKind::Epic, None, Some(lead_id)),
        )
        .await;
        insert(
            &store,
            &mk_session(lead_id, SessionKind::Task, Some(epic_id), None),
        )
        .await;

        let mut tasks = Vec::new();
        for _ in 0..8 {
            let coord = Arc::clone(&coord);
            let active = Arc::clone(&active);
            let completed = Arc::clone(&completed);
            let store = Arc::clone(&store);
            tasks.push(tokio::spawn(async move {
                coord
                    .handle_agent(
                        lead_id,
                        agent_request("concurrent-key", "work"),
                        &active,
                        &completed,
                        &store,
                    )
                    .await
            }));
        }
        let mut ids = Vec::new();
        for task in tasks {
            match task.await.expect("join") {
                SpawnState::Spawning {
                    spawn_request_id,
                    child_session_id,
                    ..
                } => ids.push((spawn_request_id, child_session_id)),
                other => panic!("unexpected spawn state: {other:?}"),
            }
        }
        let expected_ids = ids[0];
        assert!(ids.iter().all(|candidate| *candidate == expected_ids));
        assert_eq!(
            rx.try_recv().expect("one queued request").spawn_request_id,
            ids[0].0
        );
        assert!(
            rx.try_recv().is_err(),
            "concurrent exact retries must deduplicate"
        );
    }

    #[tokio::test]
    async fn successor_dispatch_hints_are_bounded_and_coalesced() {
        let (spawn_tx, _spawn_rx) = mpsc::channel(1);
        let coordinator = SpawnCoordinator::new(spawn_tx);
        let (successor_tx, mut successor_rx) = mpsc::channel(1);
        coordinator
            .install_successor_sender(successor_tx)
            .expect("install successor sender");
        let reservation_id = Uuid::new_v4();

        coordinator
            .dispatch_successor(reservation_id)
            .await
            .expect("first hint fits bounded channel");
        coordinator
            .dispatch_successor(reservation_id)
            .await
            .expect("duplicate hint coalesces");
        assert_eq!(
            successor_rx
                .recv()
                .await
                .expect("receive coalesced hint")
                .reservation_id,
            reservation_id
        );
        assert!(successor_rx.try_recv().is_err());

        coordinator
            .complete_successor_dispatch(reservation_id)
            .await;
        coordinator
            .dispatch_successor(reservation_id)
            .await
            .expect("completed identity can be retried by backstop");
        assert_eq!(
            successor_rx
                .recv()
                .await
                .expect("receive retry hint")
                .reservation_id,
            reservation_id
        );
    }
}
