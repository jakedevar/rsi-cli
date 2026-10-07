use crate::error::Result;
use crate::session::agent_message_arbiter::{
    AgentMessageArbiter, ArbitrationGrant, BoundaryDecision, decide_next_boundary,
};
use crate::session::agent_message_delivery::build_claim_request;
use crate::session::harness::types::{ChatMessage, ChatRequest};
use crate::store::Store;
use crate::store::agent_coordination::NoEffectDisposition;
use rsi_common::agent_coordination::{
    BoundaryAdmissionV1, BoundaryClassificationV1, BoundaryProviderKindV1, MessageAttemptFenceV1,
};
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

#[async_trait::async_trait]
pub(crate) trait HarnessMailBoundary: Send + Sync {
    async fn claim(
        &self,
        model_invocation_id: Uuid,
    ) -> Result<Option<Box<dyn HarnessMailDelivery>>>;
}

#[async_trait::async_trait]
pub(crate) trait HarnessMailDelivery: Send {
    fn append(&mut self, history: &mut Vec<ChatMessage>, request: &mut ChatRequest);

    fn undo_append(&mut self, history: &mut Vec<ChatMessage>, request: &mut ChatRequest);

    async fn record_effect_possible(&mut self);

    async fn record_rejected(&mut self, error_class: &'static str);
}

pub(crate) struct HarnessAgentMailBoundary {
    store: Arc<Mutex<Store>>,
    arbiter: Arc<AgentMessageArbiter>,
    session_id: Uuid,
    monitor_generation: u64,
}

impl HarnessAgentMailBoundary {
    pub(crate) fn new(
        store: Arc<Mutex<Store>>,
        arbiter: Arc<AgentMessageArbiter>,
        session_id: Uuid,
        monitor_generation: u64,
    ) -> Self {
        Self {
            store,
            arbiter,
            session_id,
            monitor_generation,
        }
    }
}

#[async_trait::async_trait]
impl HarnessMailBoundary for HarnessAgentMailBoundary {
    async fn claim(
        &self,
        model_invocation_id: Uuid,
    ) -> Result<Option<Box<dyn HarnessMailDelivery>>> {
        let grant = {
            let store = self.store.lock().await;
            match decide_next_boundary(
                &store,
                &self.arbiter,
                self.session_id,
                self.monitor_generation,
            )? {
                BoundaryDecision::DeliverMail(grant) => grant,
                BoundaryDecision::SyntheticContinuation(_) => return Ok(None),
            }
        };

        let (_, claim) = {
            let store = self.store.lock().await;
            let boot_id = store.delivery_boot_id();
            (
                boot_id,
                build_claim_request(&grant, model_invocation_id, boot_id),
            )
        };
        let authority_id = claim.authority_id;
        let fence = {
            let store = self.store.lock().await;
            match store.claim_agent_message_exact(&claim)? {
                crate::store::agent_coordination::ClaimAgentMessageOutcome::Claimed(fence) => fence,
                crate::store::agent_coordination::ClaimAgentMessageOutcome::CasLost(_) => {
                    grant.release();
                    return Ok(None);
                }
            }
        };

        let claimed = ClaimedHarnessMail {
            store: Arc::clone(&self.store),
            grant: Some(grant),
            fence,
            authority_id,
            rendered: String::new(),
            appended: false,
        };
        let marked = {
            let store = self.store.lock().await;
            store.mark_agent_message_attempt_dispatching(&claimed.fence)
        };
        if let Err(error) = marked {
            tracing::warn!(
                target: "agent_coordination",
                session_id = %self.session_id,
                message_id = %claimed.fence.message_id,
                error = %error,
                "Harness mail dispatch marker failed; refusing to send"
            );
            let mut claimed = claimed;
            claimed
                .record_rejected("agent_message_dispatch_marker_failed")
                .await;
            return Ok(None);
        }

        let rendered = claimed
            .grant
            .as_deref()
            .map(ArbitrationGrant::render_payload_for_delivery)
            .unwrap_or_default();
        let claimed = ClaimedHarnessMail {
            rendered,
            ..claimed
        };
        Ok(Some(Box::new(claimed)))
    }
}

struct ClaimedHarnessMail {
    store: Arc<Mutex<Store>>,
    grant: Option<Box<ArbitrationGrant>>,
    fence: MessageAttemptFenceV1,
    authority_id: Uuid,
    rendered: String,
    appended: bool,
}

impl ClaimedHarnessMail {
    fn admission(
        &self,
        classification: BoundaryClassificationV1,
        error_class: Option<&'static str>,
    ) -> BoundaryAdmissionV1 {
        let provider_kind = self
            .grant
            .as_deref()
            .map(|grant| grant.request().provider_kind)
            .unwrap_or(BoundaryProviderKindV1::Harness);
        BoundaryAdmissionV1 {
            provider_kind,
            // #1183: the session's own matrix row. A Harness session is
            // `harness_tool_boundary`; an OpenRouter or Bedrock session on this
            // loop keeps its `codex_cli` row (`terminal_one_turn`), which is
            // what its attempt was claimed with. Both persist identically.
            capability_kind: provider_kind.capability_kind(),
            delivery_session_id: self.fence.delivery_session_id,
            session_generation: self.fence.delivery_session_generation,
            model_invocation_id: self.fence.delivery_model_invocation_id,
            native_turn_id: None,
            classification,
            provider_error_class: error_class.map(str::to_string),
        }
    }

    async fn record(
        &mut self,
        classification: BoundaryClassificationV1,
        error_class: Option<&'static str>,
    ) {
        let admission = self.admission(classification, error_class);
        let outcome = {
            let store = self.store.lock().await;
            store.record_agent_message_admission(
                &self.fence,
                &admission,
                NoEffectDisposition::Requeue,
                self.authority_id,
            )
        };
        match outcome {
            Ok(crate::store::agent_coordination::RecordAdmissionOutcome::Recorded {
                state,
                ..
            }) => tracing::debug!(
                target: "agent_coordination",
                message_id = %self.fence.message_id,
                attempt_number = self.fence.attempt_number,
                state = state.as_str(),
                classification = classification.as_str(),
                "recorded a Harness tool-boundary mail delivery"
            ),
            Ok(_) => {}
            Err(error) => tracing::warn!(
                target: "agent_coordination",
                message_id = %self.fence.message_id,
                attempt_number = self.fence.attempt_number,
                error = %error,
                "failed to record a Harness tool-boundary mail delivery; keeping the attempt conservative"
            ),
        }
        if let Some(grant) = self.grant.take() {
            grant.release();
        }
    }
}

#[async_trait::async_trait]
impl HarnessMailDelivery for ClaimedHarnessMail {
    fn append(&mut self, history: &mut Vec<ChatMessage>, request: &mut ChatRequest) {
        if self.appended {
            return;
        }
        let message = ChatMessage::user(self.rendered.as_str());
        history.push(message.clone());
        request.messages.push(message);
        self.appended = true;
    }

    fn undo_append(&mut self, history: &mut Vec<ChatMessage>, request: &mut ChatRequest) {
        if !self.appended {
            return;
        }
        if history
            .last()
            .is_some_and(|message| message.content == self.rendered)
        {
            history.pop();
        }
        if request
            .messages
            .last()
            .is_some_and(|message| message.content == self.rendered)
        {
            request.messages.pop();
        }
        self.appended = false;
    }

    async fn record_effect_possible(&mut self) {
        self.record(BoundaryClassificationV1::AdmittedEffectPossible, None)
            .await;
    }

    async fn record_rejected(&mut self, error_class: &'static str) {
        self.record(
            BoundaryClassificationV1::RejectedBeforeEffect,
            Some(error_class),
        )
        .await;
    }
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct TestHarnessMailDelivery {
    pub(crate) message: String,
    pub(crate) appended: usize,
    pub(crate) history_appends: usize,
    pub(crate) undone: usize,
    pub(crate) effect_possible: usize,
    pub(crate) rejected: usize,
}

#[cfg(test)]
struct TestHarnessMailDeliveryHandle {
    state: std::sync::Arc<std::sync::Mutex<TestHarnessMailDelivery>>,
}

#[cfg(test)]
#[async_trait::async_trait]
impl HarnessMailDelivery for TestHarnessMailDeliveryHandle {
    fn append(&mut self, history: &mut Vec<ChatMessage>, request: &mut ChatRequest) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.appended += 1;
        state.history_appends += 1;
        let message = ChatMessage::user(state.message.as_str());
        history.push(message.clone());
        request.messages.push(message);
    }

    fn undo_append(&mut self, history: &mut Vec<ChatMessage>, request: &mut ChatRequest) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.undone += 1;
        history.pop();
        request.messages.pop();
    }

    async fn record_effect_possible(&mut self) {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .effect_possible += 1;
    }

    async fn record_rejected(&mut self, _error_class: &'static str) {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .rejected += 1;
    }
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct TestHarnessMailBoundary {
    pub(crate) claims: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pub(crate) delivery: Option<std::sync::Arc<std::sync::Mutex<TestHarnessMailDelivery>>>,
    pub(crate) cancel_on_claim: Option<tokio_util::sync::CancellationToken>,
}

#[cfg(test)]
#[async_trait::async_trait]
impl HarnessMailBoundary for TestHarnessMailBoundary {
    async fn claim(
        &self,
        _model_invocation_id: Uuid,
    ) -> Result<Option<Box<dyn HarnessMailDelivery>>> {
        self.claims
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Some(cancel) = &self.cancel_on_claim {
            cancel.cancel();
        }
        Ok(self.delivery.clone().map(|state| {
            Box::new(TestHarnessMailDeliveryHandle { state }) as Box<dyn HarnessMailDelivery>
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::agent_verbs::tests::test_session;
    use rsi_common::agent_coordination::AgentSendMessageRequestV1;
    use rsi_common::types::{SessionKind, SessionProvider, SessionStatus};

    fn message_state(store: &Store, message_id: Uuid) -> String {
        store
            .conn
            .query_row(
                "SELECT state FROM agent_messages WHERE id=?1",
                rusqlite::params![message_id.to_string()],
                |row| row.get(0),
            )
            .expect("message state")
    }

    /// A running session of `provider` with a current turn invocation, a
    /// manager sender, and one queued message for it.
    fn running_with_mail(provider: SessionProvider) -> (Arc<Mutex<Store>>, Uuid, Uuid, Uuid) {
        let store = Store::open_in_memory().expect("store");
        store.set_delivery_boot_id(Uuid::new_v4()).expect("boot id");
        let owner = Uuid::new_v4();
        let target = Uuid::new_v4();
        let mut owner_row = test_session(owner, std::path::PathBuf::from("/tmp"));
        owner_row.session_kind = SessionKind::Task;
        owner_row.status = SessionStatus::Running;
        owner_row.agent_role = Some("manager".to_string());
        store.insert_session(&owner_row).expect("owner");
        let mut row = test_session(target, std::path::PathBuf::from("/tmp"));
        row.session_kind = SessionKind::Task;
        row.status = SessionStatus::Running;
        row.provider = provider;
        store.insert_session(&row).expect("target");
        let invocation = Uuid::new_v4();
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        store
            .conn
            .execute(
                "INSERT INTO model_invocations (
                     id, purpose, invocation_kind, foreground, paid_risk,
                     admission_status, status, trigger_source, session_id,
                     policy_snapshot_json, created_at, started_at
                 ) VALUES (?1, 'session.harness.turn', 'model', 'foreground',
                     'paid_capable', 'admitted', 'running', 'harness_mail_tests',
                     ?2, '{}', ?3, ?3)",
                rusqlite::params![invocation.to_string(), target.to_string(), now],
            )
            .expect("invocation");
        store
            .conn
            .execute(
                "UPDATE sessions SET model_invocation_id=?1 WHERE id=?2",
                rusqlite::params![invocation.to_string(), target.to_string()],
            )
            .expect("bind invocation");
        let message_id = store
            .accept_agent_message(
                owner,
                None,
                &AgentSendMessageRequestV1 {
                    target_session_id: target,
                    message: "rebase onto rolling before the gate".to_string(),
                    idempotency_key: "k-1183".to_string(),
                    expires_at: None,
                },
            )
            .expect("accept")
            .receipt()
            .message_id;
        (Arc::new(Mutex::new(store)), target, invocation, message_id)
    }

    fn chat_request() -> ChatRequest {
        ChatRequest {
            messages: Vec::new(),
            model: "m".to_string(),
            temperature: None,
            max_tokens: None,
            tools: Vec::new(),
            stream: true,
            reasoning_effort: None,
            context_editing: false,
        }
    }

    /// #1183: an OpenRouter (or Bedrock) session on the Harness tool loop takes
    /// its mail at the next model-call boundary exactly like a Harness
    /// session: appended once, recorded `injected`, never claimed again.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn a_harness_routed_openrouter_session_takes_mail_once_at_the_tool_boundary() {
        for provider in [
            SessionProvider::OpenRouter,
            SessionProvider::Bedrock,
            SessionProvider::Harness,
        ] {
            let (store, target, invocation, message_id) = running_with_mail(provider);
            let arbiter = Arc::new(AgentMessageArbiter::new());
            let boundary =
                HarnessAgentMailBoundary::new(Arc::clone(&store), Arc::clone(&arbiter), target, 0);

            let mut mail = boundary
                .claim(invocation)
                .await
                .expect("claim")
                .unwrap_or_else(|| panic!("{provider:?}: mail claimed at the boundary"));
            let mut history = Vec::new();
            let mut request = chat_request();
            mail.append(&mut history, &mut request);
            mail.append(&mut history, &mut request);
            assert_eq!(history.len(), 1, "{provider:?}");
            assert!(
                request.messages[0]
                    .content
                    .contains("rebase onto rolling before the gate")
            );
            mail.record_effect_possible().await;
            assert_eq!(
                message_state(&*store.lock().await, message_id),
                "injected",
                "{provider:?}"
            );

            // At most once: the next boundary finds nothing.
            assert!(
                boundary.claim(invocation).await.expect("claim").is_none(),
                "{provider:?}"
            );
            assert!(arbiter.roots_with_outstanding_grant().is_empty());
        }
    }

    /// A delivery cancelled before the model call is put back, not lost: the
    /// row is requeued and the next boundary delivers it.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn a_boundary_delivery_rejected_before_dispatch_is_requeued_for_the_next_boundary() {
        let (store, target, invocation, message_id) =
            running_with_mail(SessionProvider::OpenRouter);
        let arbiter = Arc::new(AgentMessageArbiter::new());
        let boundary =
            HarnessAgentMailBoundary::new(Arc::clone(&store), Arc::clone(&arbiter), target, 0);
        let mut mail = boundary
            .claim(invocation)
            .await
            .expect("claim")
            .expect("mail");
        mail.record_rejected("agent_message_cancelled_before_dispatch")
            .await;
        assert_eq!(message_state(&*store.lock().await, message_id), "queued");
        let mut again = boundary
            .claim(invocation)
            .await
            .expect("claim")
            .expect("again");
        again.record_effect_possible().await;
        assert_eq!(message_state(&*store.lock().await, message_id), "injected");
    }
}
