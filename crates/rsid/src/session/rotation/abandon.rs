//! #1176: the operator's abandon of a blocked rotation.
//!
//! A blocked rotation (#1156) keeps the seat on its predecessor while a
//! successor that never started holds the seat's sandbox. The abandon records
//! one durable request and rotates that holder forward: the ordinary rotation
//! decider runs with the `Failed` holder as predecessor, so the replacement is
//! reserved, bound to the sandbox (custody moves forward, cause `rotation`),
//! started and published by the same kernel as every rotation. Its publication
//! also settles the blocked chain (`Store::publish_rotation_successor_with_chain`).
//! Operator-only: the RPC is not an agent or read verb.
use super::{RotationPredecessorSource, SessionManager};
use crate::error::{DaemonError, Result};
use crate::store::rotation_abandon::{RotationAbandonAdmission, RotationAbandonRequest};
use rsi_common::rpc::{AbandonBlockedRotationParams, AbandonBlockedRotationResult};
use std::sync::Arc;

const IDEMPOTENCY_KEY_MAX: usize = 128;

impl SessionManager {
    /// Abandon the blocked rotation `params.session_id` belongs to: dispatch
    /// the holder's rotation to a fresh replacement, or return the first
    /// request of a repeated idempotency key.
    ///
    /// # Errors
    /// `Rpc` for invalid params; `PolicyDenied` when nothing is blocked, the
    /// holder is live, the rotation changed or an abandon is in flight.
    pub async fn abandon_blocked_rotation(
        &self,
        params: AbandonBlockedRotationParams,
    ) -> Result<AbandonBlockedRotationResult> {
        let key = params.idempotency_key.trim();
        if key.is_empty() || key.len() > IDEMPOTENCY_KEY_MAX {
            return Err(DaemonError::Rpc(format!(
                "idempotency_key must be 1-{IDEMPOTENCY_KEY_MAX} characters"
            )));
        }
        let model = params
            .model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty());
        let (blocked, replay) = {
            let store = self.store.lock().await;
            match store.blocked_rotation_chain_of(params.session_id)? {
                Some(blocked) => (Some(blocked), None),
                None => (
                    None,
                    store.rotation_abandon_request_by_key(params.session_id, key)?,
                ),
            }
        };
        if let Some(request) = replay {
            return self.abandon_receipt(&request, "replayed").await;
        }
        let Some(blocked) = blocked else {
            return Err(DaemonError::PolicyDenied(format!(
                "rotation_not_blocked:{}",
                params.session_id
            )));
        };
        let admission = {
            // The decider takes the holder's spawn guard itself; holding it
            // here orders the request after any in-flight start of the holder.
            let _holder_guard =
                super::super::spawn_single_flight::acquire_spawn_guard(blocked.holder).await;
            let holder_live = self.active.read().await.contains_key(&blocked.holder);
            // A spawn error settles the bound row durably before it ever
            // enters the runtime maps. Reuse Continue's terminal hydration so
            // a second abandon can rotate that holder in this same boot.
            if !holder_live && !self.completed.read().await.contains_key(&blocked.holder) {
                if let Some(holder) = self
                    .load_completed_session_from_store(blocked.holder)
                    .await?
                {
                    self.completed
                        .write()
                        .await
                        .entry(blocked.holder)
                        .or_insert(holder);
                }
            }
            self.store.lock().await.record_rotation_abandon_request(
                &blocked,
                key,
                params.provider,
                model,
                holder_live,
            )?
        };
        match admission {
            RotationAbandonAdmission::Replayed(request) => {
                self.abandon_receipt(&request, "replayed").await
            }
            RotationAbandonAdmission::Admitted(request) => {
                tracing::warn!(
                    predecessor = %request.predecessor,
                    rotation_id = %request.rotation_id,
                    holder = %request.holder_id,
                    abandon_rotation_id = %request.abandon_rotation_id,
                    "Operator abandoned a blocked rotation; rotating its holder forward"
                );
                self.dispatch_abandon_rotation(&request)?;
                self.abandon_receipt(&request, "dispatched").await
            }
        }
    }

    async fn abandon_receipt(
        &self,
        request: &RotationAbandonRequest,
        status: &str,
    ) -> Result<AbandonBlockedRotationResult> {
        let replacement = self
            .store
            .lock()
            .await
            .rotation_abandon_replacement(request)?
            .map(|(id, _)| id);
        Ok(AbandonBlockedRotationResult {
            predecessor_id: request.predecessor,
            rotation_id: request.rotation_id.clone(),
            holder_id: request.holder_id,
            abandon_rotation_id: request.abandon_rotation_id.clone(),
            replacement_id: replacement,
            status: status.to_string(),
        })
    }

    fn dispatch_abandon_rotation(&self, request: &RotationAbandonRequest) -> Result<()> {
        let model_call_settlements = self.model_call_settlements.handle()?;
        let decider = Self::decide_rotation_successor(
            request.holder_id,
            None,
            Some(request.abandon_rotation_id.clone()),
            Arc::clone(&self.active),
            Arc::clone(&self.completed),
            Arc::clone(&self.event_bus),
            Arc::clone(&self.store),
            model_call_settlements,
            self.persistence.clone(),
            self.context_rotation_enabled,
            self.socket_path.clone(),
            Arc::clone(&self.token_counter),
            self.memory_handle.clone(),
            self.retry_tx.clone(),
            Arc::clone(&self.runtime_config),
            Arc::clone(&self.spawn_coordinator),
            Arc::clone(&self.agent_tokens),
            Arc::clone(&self.spawn_epoch),
            Arc::clone(&self.agent_message_arbiter),
            self.codegraph_handle.clone(),
            self.custody_execution_runtime(),
            RotationPredecessorSource::BlockedHolder,
        );
        // The decider awaits the replacement's monitor; never block the RPC.
        tokio::spawn(Box::pin(decider));
        Ok(())
    }
}
