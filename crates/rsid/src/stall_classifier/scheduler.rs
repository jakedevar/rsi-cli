//! Classifier scheduler: sibling tokio task that receives session IDs on a
//! bounded `mpsc::Receiver<Uuid>` (signaled by the stall detector), builds
//! the classification input, prompts the LLM, parses the verdict, applies
//! the confidence floor, publishes a telemetry event, and (for nudging
//! verdicts) hands off to the nudge consumer loop.
//!
//! Phase 4 wired the LLM call, JSON parse, verdict gating, and the
//! `update_tracked_after_classification` writer that mutates the active
//! map's classifier-state fields.
//!
//! The LLM call goes through a `ClassifierCompleter` trait so test code can
//! inject deterministic verdicts. Production callers use
//! `StallClassifierLlmClient` which implements the trait.

use crate::bus::{DaemonEvent, EventBus};
use crate::config::{Config, RuntimeConfig};
use crate::error::{DaemonError, Result};
use crate::model_control::registry::RuntimeExecutionRoute;
use crate::model_control::{
    AdmissionDecision, ModelAdmissionRequest, ModelExecutionCapability, classify_error_class,
    completion_with_wall_time, hash_request_fingerprint, settle_result,
};
use crate::session::types::TrackedSession;
use crate::store::Store;
use async_trait::async_trait;
use rsi_common::model_control::{InvocationOwner, ModelInvocationPurpose, ModelUsageConfidence};
use rsi_common::types::SessionProvider;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::sync::{Mutex, RwLock, mpsc};
use uuid::Uuid;

use super::input::build_classification_input;
use super::llm_client::StallClassifierLlmClient;
use super::prompts::{CLASSIFIER_SYSTEM_PROMPT, build_user_prompt};
use super::types::{ClassifierVerdict, NudgeAction, Verdict};

/// Sibling-task-scoped configuration. Constructed from `Config` at boot; the
/// runtime-mutable knobs come from `RuntimeConfig` and are re-read inside
/// the loop body on every received UUID so a `UpdateDaemonConfig` RPC takes
/// effect without a daemon restart.
#[derive(Debug, Clone)]
pub struct ClassifierConfig {
    /// Per-event default; cap on the count of classifications per session
    /// lifetime. Mirrored from `Config::stall_classifier_max_per_session`.
    pub max_per_session: u32,
    /// Per-event default; minimum gap (seconds) between two classifications
    /// of the same session. Mirrored from
    /// `Config::stall_classifier_cooldown_secs`.
    pub cooldown_secs: u64,
    /// Excerpt event limit (event count); passed to the Phase 3 input
    /// builder.
    pub excerpt_event_limit: usize,
    /// Excerpt per-event character cap; passed to the Phase 3 input
    /// builder.
    pub excerpt_char_cap: usize,
}

impl ClassifierConfig {
    pub fn from_config(c: &Config) -> Self {
        Self {
            max_per_session: c.stall_classifier_max_per_session,
            cooldown_secs: c.stall_classifier_cooldown_secs,
            // Defaults from the plan §Phase 3 helper signature.
            excerpt_event_limit: 30,
            excerpt_char_cap: 500,
        }
    }
}

/// Abstraction over the LLM completion call so tests can inject deterministic
/// verdicts without standing up an HTTP server. Production wiring uses
/// `StallClassifierLlmClient`; tests use a mock.
#[async_trait]
pub(crate) trait ClassifierCompleter: Send + Sync {
    async fn complete(
        &self,
        system_prompt: &str,
        user_prompt: &str,
        execution: ModelExecutionCapability,
    ) -> Result<String>;

    fn invocation_target(&self) -> crate::memory::llm::MemoryLlmTarget;
}

#[async_trait]
impl ClassifierCompleter for StallClassifierLlmClient {
    async fn complete(
        &self,
        system_prompt: &str,
        user_prompt: &str,
        execution: ModelExecutionCapability,
    ) -> Result<String> {
        StallClassifierLlmClient::complete(self, system_prompt, user_prompt, execution).await
    }

    fn invocation_target(&self) -> crate::memory::llm::MemoryLlmTarget {
        crate::memory::llm::MemoryLlmTarget {
            provider: SessionProvider::Harness,
            model: self.model.clone(),
            base_url: Some(self.api_url.clone()),
            api_key: self.api_key.clone(),
        }
    }
}

/// Pure-fn verdict-to-action gating. `StalledContinue` / `StalledCheckTeam`
/// only flip to `Continue` when (a) confidence ≥ floor AND (b) the model
/// actually produced a non-empty nudge prompt. Everything else falls
/// through to `NotifyOnly`.
pub(crate) fn decide_action(v: &ClassifierVerdict, floor: f64) -> NudgeAction {
    match v.verdict {
        Verdict::Finished | Verdict::NeedsUser => NudgeAction::NotifyOnly { verdict: v.verdict },
        Verdict::StalledContinue | Verdict::StalledCheckTeam => {
            let prompt_ok = v
                .nudge_prompt
                .as_deref()
                .map(|p| !p.trim().is_empty())
                .unwrap_or(false);
            if v.confidence < floor || !prompt_ok {
                NudgeAction::NotifyOnly { verdict: v.verdict }
            } else {
                NudgeAction::Continue {
                    verdict: v.verdict,
                    prompt: v.nudge_prompt.clone().unwrap_or_default(),
                }
            }
        }
    }
}

/// Write the classifier-state fields on the `TrackedSession`:
///   - `last_classified_at = now`
///   - `classification_count += 1`
///   - `last_verdict = Some(verdict)`
///
/// Silently no-ops if the session has since left the active map (terminal
/// status, deletion, etc.). Returns the new `classification_count` if the
/// session was found, else `None`.
pub(crate) async fn update_tracked_after_classification(
    active: &Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    session_id: Uuid,
    verdict: &ClassifierVerdict,
) -> Option<u32> {
    let mut guard = active.write().await;
    let tracked = guard.get_mut(&session_id)?;
    tracked.last_classified_at = Some(chrono::Utc::now());
    tracked.classification_count = tracked.classification_count.saturating_add(1);
    tracked.last_verdict = Some(verdict.verdict);
    Some(tracked.classification_count)
}

/// Single classification step. Extracted so tests can drive the pipeline
/// end-to-end without spinning a tokio task / mpsc receiver.
///
/// Returns the chosen `NudgeAction` on success. Errors propagate from the
/// input builder (session not in active map) or the LLM client (HTTP /
/// timeout). JSON parse failures are NOT errors — they downgrade to
/// `NotifyOnly { verdict: NeedsUser }` and emit a warn log per plan §D3.
pub(crate) async fn classify_once<C: ClassifierCompleter + ?Sized>(
    session_id: Uuid,
    active: &Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    store: &Arc<Mutex<Store>>,
    event_bus: &Arc<EventBus>,
    llm: &C,
    config: &ClassifierConfig,
    floor: f64,
) -> Result<NudgeAction> {
    let input = build_classification_input(
        session_id,
        active,
        store,
        config.excerpt_event_limit,
        config.excerpt_char_cap,
    )
    .await?;

    let user_prompt = build_user_prompt(&input);
    let target = llm.invocation_target();
    let provider_label = crate::memory::llm::provider_label(&target)?;
    let backend_label = crate::memory::llm::backend_label(&target)?.to_string();
    let admission_request = ModelAdmissionRequest {
        purpose: ModelInvocationPurpose::StallClassifier,
        provider: Some(provider_label.clone()),
        model: Some(target.model.clone()),
        backend: Some(backend_label.clone()),
        effort: None,
        trigger: "stall_classifier".to_string(),
        owner: InvocationOwner {
            session_id: Some(session_id),
            ..InvocationOwner::default()
        },
        dedup_key: Some(crate::model_control::stable_dedup_key(
            "stall-classifier",
            &[&session_id.to_string(), &target.model, &user_prompt],
        )),
        request_fingerprint: Some(hash_request_fingerprint(&[
            &target.model,
            target.base_url.as_deref().unwrap_or(""),
            &user_prompt,
        ])),
        parent_invocation_id: None,
        retry_of_invocation_id: None,
        expected_usage: Some(crate::model_control::explicit_expected_usage(
            ModelInvocationPurpose::StallClassifier,
            Some(provider_label.as_str()),
            Some(backend_label.as_str()),
            Some(target.model.as_str()),
        )),
        baseline_input_tokens: 0,
        baseline_output_tokens: 0,
        baseline_cache_creation_tokens: 0,
        baseline_cache_read_tokens: 0,
        baseline_reasoning_tokens: 0,
        baseline_embedding_input_count: 0,
        baseline_wall_time_ms: 0,
    };
    let permit =
        match crate::model_control::admit_invocation(store, admission_request, event_bus).await? {
            AdmissionDecision::Admitted(permit) => permit,
            AdmissionDecision::Duplicate { invocation_id } => {
                return Err(DaemonError::PolicyDenied(format!(
                    "duplicate stall classifier invocation suppressed ({invocation_id})"
                )));
            }
        };
    let started_at = std::time::Instant::now();
    let llm_result = match permit.claim_model_execution(RuntimeExecutionRoute::StallClassifierHttp)
    {
        Ok(execution) => {
            llm.complete(CLASSIFIER_SYSTEM_PROMPT, &user_prompt, execution)
                .await
        }
        Err(error) => Err(error),
    };
    let completion = match &llm_result {
        Ok(_) => completion_with_wall_time(started_at, None, ModelUsageConfidence::Partial),
        Err(error) => completion_with_wall_time(
            started_at,
            Some(classify_error_class(error)),
            ModelUsageConfidence::Partial,
        ),
    };
    let raw = settle_result(
        store,
        &permit,
        completion,
        llm_result,
        "Stall classifier",
        event_bus,
    )
    .await?;

    let parsed = match parse_verdict_payload(&raw) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                raw = %raw,
                "Stall classifier: JSON parse failed; emitting NeedsUser telemetry"
            );
            // Fabricate a low-confidence NeedsUser verdict so the
            // downstream pipeline still records the classification
            // attempt (and the cooldown takes effect, preventing
            // immediate retry on persistent garbage).
            ClassifierVerdict {
                verdict: Verdict::NeedsUser,
                confidence: 0.0,
                nudge_prompt: None,
                reasoning: format!("parse_failed: {e}"),
            }
        }
    };

    let action = decide_action(&parsed, floor);

    // Best-effort mutation of TrackedSession; ignore "session gone"
    // because the verdict is still a valid telemetry signal.
    let _ = update_tracked_after_classification(active, session_id, &parsed).await;

    event_bus.publish(DaemonEvent::SessionClassified {
        session_id,
        verdict: parsed.verdict,
        confidence: parsed.confidence,
        idle_secs: input.idle_secs,
        action_taken: action.label().to_string(),
    });

    Ok(action)
}

/// JSON-parse the LLM payload into a `ClassifierVerdict`. Tolerates code
/// fences / leading or trailing whitespace by stripping the outer
/// "```json...```" wrapper before deserializing.
fn parse_verdict_payload(raw: &str) -> Result<ClassifierVerdict> {
    let trimmed = strip_code_fence(raw.trim());
    serde_json::from_str::<ClassifierVerdict>(trimmed)
        .map_err(|e| DaemonError::Process(format!("classifier verdict parse error: {e}")))
}

fn strip_code_fence(s: &str) -> &str {
    let inner = s
        .strip_prefix("```json")
        .or_else(|| s.strip_prefix("```"))
        .unwrap_or(s);
    inner.strip_suffix("```").unwrap_or(inner).trim()
}

/// Spawn the classifier sibling task. The returned `JoinHandle` is held by
/// `main.rs` so the task is dropped (and the loop exits) when the daemon
/// shuts down.
pub fn spawn_classifier(
    store: Arc<Mutex<Store>>,
    active: Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    event_bus: Arc<EventBus>,
    llm: StallClassifierLlmClient,
    config: ClassifierConfig,
    runtime_config: Arc<RuntimeConfig>,
    mut rx: mpsc::Receiver<Uuid>,
    nudge_tx: mpsc::Sender<(Uuid, NudgeAction)>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!("Stall classifier scheduler spawned");
        while let Some(session_id) = rx.recv().await {
            // Re-read on every receive so a runtime disable takes effect
            // immediately.
            let enabled = runtime_config
                .stall_classifier_enabled
                .load(Ordering::Relaxed);
            if !enabled {
                tracing::debug!(
                    session_id = %session_id,
                    "Stall classifier: disabled at receive time; dropping signal"
                );
                continue;
            }
            let floor = *runtime_config.stall_classifier_confidence_floor.read();
            match classify_once(
                session_id, &active, &store, &event_bus, &llm, &config, floor,
            )
            .await
            {
                Ok(action) => {
                    tracing::info!(
                        session_id = %session_id,
                        action = action.label(),
                        "Stall classifier: classification complete"
                    );
                    if matches!(action, NudgeAction::Continue { .. }) {
                        if let Err(e) = nudge_tx.try_send((session_id, action)) {
                            tracing::warn!(
                                session_id = %session_id,
                                error = ?e,
                                "Stall classifier: nudge channel send failed"
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        session_id = %session_id,
                        error = %e,
                        "Stall classifier: classify_once failed"
                    );
                }
            }
        }
        tracing::info!("Stall classifier scheduler shutting down");
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use rsi_common::model_control::ModelControlMode;
    use rsi_common::types::{SessionKind, SessionProvider, SessionStatus};
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    fn cfg() -> Config {
        let mut c = Config::from_env();
        c.stall_classifier_enabled = false;
        c.stall_classifier_max_per_session = 3;
        c.stall_classifier_cooldown_secs = 1800;
        c
    }

    fn classifier_cfg() -> ClassifierConfig {
        ClassifierConfig {
            max_per_session: 3,
            cooldown_secs: 1800,
            excerpt_event_limit: 30,
            excerpt_char_cap: 500,
        }
    }

    fn verdict(v: Verdict, confidence: f64, prompt: Option<&str>) -> ClassifierVerdict {
        ClassifierVerdict {
            verdict: v,
            confidence,
            nudge_prompt: prompt.map(String::from),
            reasoning: String::new(),
        }
    }

    #[test]
    fn classifier_config_mirrors_top_level_config() {
        let mut c = cfg();
        c.stall_classifier_max_per_session = 7;
        c.stall_classifier_cooldown_secs = 60;
        let cc = ClassifierConfig::from_config(&c);
        assert_eq!(cc.max_per_session, 7);
        assert_eq!(cc.cooldown_secs, 60);
        assert_eq!(cc.excerpt_event_limit, 30);
        assert_eq!(cc.excerpt_char_cap, 500);
    }

    // --- decide_action ---

    #[test]
    fn decide_action_finished_is_notify_only() {
        let v = verdict(Verdict::Finished, 0.99, None);
        assert!(matches!(
            decide_action(&v, 0.7),
            NudgeAction::NotifyOnly { .. }
        ));
    }

    #[test]
    fn decide_action_needs_user_is_notify_only() {
        let v = verdict(Verdict::NeedsUser, 0.99, Some("ignored"));
        assert!(matches!(
            decide_action(&v, 0.7),
            NudgeAction::NotifyOnly { .. }
        ));
    }

    #[test]
    fn decide_action_stalled_continue_below_floor_is_notify_only() {
        let v = verdict(Verdict::StalledContinue, 0.5, Some("go"));
        assert!(matches!(
            decide_action(&v, 0.7),
            NudgeAction::NotifyOnly { .. }
        ));
    }

    #[test]
    fn decide_action_stalled_continue_without_prompt_is_notify_only() {
        let v = verdict(Verdict::StalledContinue, 0.9, None);
        assert!(matches!(
            decide_action(&v, 0.7),
            NudgeAction::NotifyOnly { .. }
        ));
    }

    #[test]
    fn decide_action_stalled_continue_with_empty_prompt_is_notify_only() {
        let v = verdict(Verdict::StalledContinue, 0.9, Some("   \n  "));
        assert!(matches!(
            decide_action(&v, 0.7),
            NudgeAction::NotifyOnly { .. }
        ));
    }

    #[test]
    fn decide_action_stalled_continue_above_floor_with_prompt_is_continue() {
        let v = verdict(Verdict::StalledContinue, 0.85, Some("Please continue."));
        match decide_action(&v, 0.7) {
            NudgeAction::Continue { verdict, prompt } => {
                assert_eq!(verdict, Verdict::StalledContinue);
                assert_eq!(prompt, "Please continue.");
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn decide_action_stalled_check_team_above_floor_with_prompt_is_continue() {
        let v = verdict(Verdict::StalledCheckTeam, 0.9, Some("Check sub-agents."));
        match decide_action(&v, 0.7) {
            NudgeAction::Continue { verdict, .. } => {
                assert_eq!(verdict, Verdict::StalledCheckTeam);
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    // --- parse_verdict_payload ---

    #[test]
    fn parse_verdict_payload_accepts_strict_json() {
        let raw = r#"{"verdict":"Finished","confidence":0.95}"#;
        let v = parse_verdict_payload(raw).expect("ok");
        assert!(matches!(v.verdict, Verdict::Finished));
        assert_eq!(v.confidence, 0.95);
    }

    #[test]
    fn parse_verdict_payload_strips_code_fences() {
        let raw = "```json\n{\"verdict\":\"NeedsUser\",\"confidence\":0.5}\n```";
        let v = parse_verdict_payload(raw).expect("ok");
        assert!(matches!(v.verdict, Verdict::NeedsUser));
    }

    #[test]
    fn parse_verdict_payload_strips_generic_fences() {
        let raw = "```\n{\"verdict\":\"NeedsUser\",\"confidence\":0.5}\n```";
        let v = parse_verdict_payload(raw).expect("ok");
        assert!(matches!(v.verdict, Verdict::NeedsUser));
    }

    #[test]
    fn parse_verdict_payload_rejects_garbage() {
        assert!(parse_verdict_payload("not json").is_err());
        assert!(parse_verdict_payload(r#"{"verdict":"Maybe","confidence":0.5}"#).is_err());
    }

    // --- Pipeline integration with a mocked LLM client ---

    /// Minimal in-process fake of `ClassifierCompleter`. Each call returns
    /// the next queued response. Useful for asserting end-to-end behavior
    /// of `classify_once` without an HTTP roundtrip.
    struct FakeCompleter {
        responses: StdMutex<Vec<String>>,
        calls: AtomicUsize,
    }

    impl FakeCompleter {
        fn new(responses: Vec<String>) -> Self {
            Self {
                responses: StdMutex::new(responses),
                calls: AtomicUsize::new(0),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl ClassifierCompleter for FakeCompleter {
        async fn complete(
            &self,
            _: &str,
            _: &str,
            _execution: ModelExecutionCapability,
        ) -> Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut guard = self.responses.lock().unwrap();
            if guard.is_empty() {
                return Err(DaemonError::Process(
                    "fake completer: no more queued responses".to_string(),
                ));
            }
            Ok(guard.remove(0))
        }

        fn invocation_target(&self) -> crate::memory::llm::MemoryLlmTarget {
            crate::memory::llm::MemoryLlmTarget {
                provider: SessionProvider::Local,
                model: "qwen2.5:7b".to_string(),
                base_url: Some("http://127.0.0.1:11434/v1".to_string()),
                api_key: None,
            }
        }
    }

    async fn seed_active_session(
        active: &Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
        store: &Arc<Mutex<Store>>,
    ) -> Uuid {
        let id = Uuid::new_v4();
        let session = rsi_common::types::Session {
            id,
            status: SessionStatus::Running,
            ..base_test_session()
        };
        {
            let store_guard = store.lock().await;
            store_guard.insert_session(&session).expect("insert");
        }
        // Use the test-only `TrackedSession::new_for_test` constructor so the
        // crate-private `rotation_coordinator` module stays encapsulated.
        let mut tracked = TrackedSession::new_for_test(session);
        tracked.set_last_event_at_for_test(chrono::Utc::now() - chrono::Duration::seconds(800));
        active.write().await.insert(id, tracked);
        id
    }

    fn base_test_session() -> rsi_common::types::Session {
        // Mirrors `store_worker::tests::test_session` so the row passes
        // `Store::insert_session` SQL validation against the canonical
        // Session schema.
        rsi_common::types::Session {
            context_fill_pct: None,
            id: Uuid::new_v4(),
            provider: SessionProvider::Claude,
            claude_session_id: None,
            query: "classifier-test".to_string(),
            title: None,
            agent_role: None,
            epic_spawn_ordinal: None,
            description: None,
            short_summary: None,
            pending_question: None,
            pending_archive: false,
            working_dir: std::path::PathBuf::from("/tmp"),
            git_branch: None,
            status: SessionStatus::Running,
            project_id: None,
            pinned_at: None,
            testing_needed_at: None,
            rotation_disabled_at: None,
            session_kind: SessionKind::Standard,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            cost_usd: None,
            duration_ms: None,
            num_turns: None,
            model: None,
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
            parent_id: None,
            lead_session_id: None,
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

    fn fresh_store() -> (TempDir, Arc<Mutex<Store>>) {
        let dir = TempDir::new().expect("tempdir");
        let store = Store::open(&dir.path().join("classifier_test.db")).expect("open");
        (dir, Arc::new(Mutex::new(store)))
    }

    #[tokio::test]
    async fn classify_once_dispatches_continue_for_stalled_continue() {
        let active: Arc<RwLock<HashMap<Uuid, TrackedSession>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let (_dir, store) = fresh_store();
        let id = seed_active_session(&active, &store).await;
        let bus = Arc::new(EventBus::new(16));
        let llm = FakeCompleter::new(vec![
            r#"{"verdict":"StalledContinue","confidence":0.9,"nudge_prompt":"Continue please."}"#
                .to_string(),
        ]);
        let action = classify_once(id, &active, &store, &bus, &llm, &classifier_cfg(), 0.7)
            .await
            .expect("ok");
        match action {
            NudgeAction::Continue { verdict, prompt } => {
                assert_eq!(verdict, Verdict::StalledContinue);
                assert_eq!(prompt, "Continue please.");
            }
            other => panic!("expected Continue, got {:?}", other),
        }
        let guard = active.read().await;
        let tracked = guard.get(&id).expect("present");
        assert_eq!(tracked.classification_count, 1);
        assert_eq!(tracked.last_verdict, Some(Verdict::StalledContinue));
        assert!(tracked.last_classified_at.is_some());
        assert_eq!(llm.call_count(), 1);
    }

    #[tokio::test]
    async fn classifier_policy_denial_does_not_call_completer() {
        let active: Arc<RwLock<HashMap<Uuid, TrackedSession>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let (_dir, store) = fresh_store();
        let id = seed_active_session(&active, &store).await;
        store
            .lock()
            .await
            .set_model_control_mode(ModelControlMode::StopAll)
            .expect("stop all");
        let bus = Arc::new(EventBus::new(16));
        let llm = FakeCompleter::new(vec![
            r#"{"verdict":"Finished","confidence":1.0}"#.to_string(),
        ]);

        let error = classify_once(id, &active, &store, &bus, &llm, &classifier_cfg(), 0.7)
            .await
            .expect_err("policy denial");
        assert!(matches!(error, DaemonError::PolicyDenied(_)));
        assert_eq!(llm.call_count(), 0);
    }

    #[tokio::test]
    async fn classify_once_downgrades_low_confidence_to_notify_only() {
        let active: Arc<RwLock<HashMap<Uuid, TrackedSession>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let (_dir, store) = fresh_store();
        let id = seed_active_session(&active, &store).await;
        let bus = Arc::new(EventBus::new(16));
        let llm = FakeCompleter::new(vec![
            r#"{"verdict":"StalledContinue","confidence":0.4,"nudge_prompt":"too unsure"}"#
                .to_string(),
        ]);
        let action = classify_once(id, &active, &store, &bus, &llm, &classifier_cfg(), 0.7)
            .await
            .expect("ok");
        assert!(matches!(action, NudgeAction::NotifyOnly { .. }));
    }

    #[tokio::test]
    async fn classify_once_notify_only_when_nudge_prompt_missing() {
        let active: Arc<RwLock<HashMap<Uuid, TrackedSession>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let (_dir, store) = fresh_store();
        let id = seed_active_session(&active, &store).await;
        let bus = Arc::new(EventBus::new(16));
        let llm = FakeCompleter::new(vec![
            r#"{"verdict":"StalledContinue","confidence":0.9}"#.to_string(),
        ]);
        let action = classify_once(id, &active, &store, &bus, &llm, &classifier_cfg(), 0.7)
            .await
            .expect("ok");
        assert!(matches!(action, NudgeAction::NotifyOnly { .. }));
    }

    #[tokio::test]
    async fn classify_once_malformed_json_downgrades_to_needs_user() {
        let active: Arc<RwLock<HashMap<Uuid, TrackedSession>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let (_dir, store) = fresh_store();
        let id = seed_active_session(&active, &store).await;
        let bus = Arc::new(EventBus::new(16));
        let llm = FakeCompleter::new(vec!["not json at all".to_string()]);
        let action = classify_once(id, &active, &store, &bus, &llm, &classifier_cfg(), 0.7)
            .await
            .expect("ok");
        match action {
            NudgeAction::NotifyOnly { verdict } => {
                assert_eq!(verdict, Verdict::NeedsUser);
            }
            other => panic!("expected NotifyOnly, got {:?}", other),
        }
        // Even on parse failure we still count the classification — that's
        // what prevents an infinite retry loop on a stuck garbage model.
        let guard = active.read().await;
        assert_eq!(guard.get(&id).unwrap().classification_count, 1);
    }

    #[tokio::test]
    async fn classify_once_finished_verdict_emits_telemetry_only() {
        let active: Arc<RwLock<HashMap<Uuid, TrackedSession>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let (_dir, store) = fresh_store();
        let id = seed_active_session(&active, &store).await;
        let bus = Arc::new(EventBus::new(16));
        let llm = FakeCompleter::new(vec![
            r#"{"verdict":"Finished","confidence":0.95,"reasoning":"agent declared done"}"#
                .to_string(),
        ]);
        let mut rx = bus.subscribe();
        let action = classify_once(id, &active, &store, &bus, &llm, &classifier_cfg(), 0.7)
            .await
            .expect("ok");
        assert!(matches!(
            action,
            NudgeAction::NotifyOnly {
                verdict: Verdict::Finished
            }
        ));
        // The event_bus drops events if no one subscribes; we subscribed
        // before the classify call, so the SessionClassified event must
        // have landed.
        let classified = std::iter::from_fn(|| rx.try_recv().ok())
            .find(|event| matches!(&**event, DaemonEvent::SessionClassified { .. }));
        match classified.as_deref() {
            Some(DaemonEvent::SessionClassified {
                session_id,
                verdict,
                action_taken,
                ..
            }) => {
                assert_eq!(*session_id, id);
                assert_eq!(*verdict, Verdict::Finished);
                assert_eq!(action_taken, "notify_only");
            }
            other => panic!("expected SessionClassified, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn update_tracked_after_classification_increments_count() {
        let active: Arc<RwLock<HashMap<Uuid, TrackedSession>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let (_dir, store) = fresh_store();
        let id = seed_active_session(&active, &store).await;
        let v = verdict(Verdict::Finished, 0.9, None);
        let c = update_tracked_after_classification(&active, id, &v).await;
        assert_eq!(c, Some(1));
        let c2 = update_tracked_after_classification(&active, id, &v).await;
        assert_eq!(c2, Some(2));
    }

    #[tokio::test]
    async fn update_tracked_after_classification_noop_for_missing_session() {
        let active: Arc<RwLock<HashMap<Uuid, TrackedSession>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let v = verdict(Verdict::Finished, 0.9, None);
        let c = update_tracked_after_classification(&active, Uuid::new_v4(), &v).await;
        assert!(c.is_none());
    }

    #[test]
    fn strip_code_fence_handles_naked_text() {
        assert_eq!(strip_code_fence("plain"), "plain");
    }

    #[test]
    fn strip_code_fence_handles_json_fence() {
        assert_eq!(strip_code_fence("```json\n{x:1}\n```"), "{x:1}");
    }
}
