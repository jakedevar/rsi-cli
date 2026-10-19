//! `AgentOperatorTodoUpsert` / `AgentOperatorTodoClose` (#1747 slice 1).
//!
//! Authority, scope derivation, idempotency and the evidence rule live in the
//! store (`Store::operator_todo_agent_actor`, `operator_todo_upsert`,
//! `operator_todo_agent_close`). The caller is the token-resolved session; the
//! item's scope comes from its manager seat, never from the request.

use rsi_common::operator_todos::{
    AgentOperatorTodoCloseRequestV1, AgentOperatorTodoCloseResultV1,
    AgentOperatorTodoUpsertRequestV1, AgentOperatorTodoUpsertResultV1,
};
use uuid::Uuid;

use super::agent_verbs::AgentControlHandle;
use crate::error::{DaemonError, Result};

fn invalid(code: &'static str) -> DaemonError {
    DaemonError::InvalidParam(code.into())
}

impl AgentControlHandle {
    /// `AgentOperatorTodoUpsert`: put an operator-only action on the list, or
    /// update the open item with the same `todo_key` in the caller's scope.
    ///
    /// # Errors
    /// `operator_todo_not_authorized`, `operator_todo_invalid_request`,
    /// `operator_todo_closed`, `operator_todo_limit`,
    /// `operator_todo_idempotency_conflict` or a persistence error.
    pub async fn agent_operator_todo_upsert(
        &self,
        caller: Uuid,
        request: AgentOperatorTodoUpsertRequestV1,
    ) -> Result<AgentOperatorTodoUpsertResultV1> {
        request.validate().map_err(invalid)?;
        let store = self.store.lock().await;
        let actor = store.operator_todo_agent_actor(caller)?;
        let write = store.operator_todo_upsert(&actor, &request)?;
        Ok(AgentOperatorTodoUpsertResultV1 {
            todo: write.todo,
            created: write.created,
            deduplicated: write.deduplicated,
        })
    }

    /// `AgentOperatorTodoClose`: close an item with recorded evidence.
    ///
    /// # Errors
    /// `operator_todo_not_authorized`, `operator_todo_invalid_request`,
    /// `operator_todo_evidence_required`, `operator_todo_not_found`,
    /// `operator_todo_not_in_scope`, `operator_todo_idempotency_conflict` or a
    /// persistence error. Closing an already closed item answers with the
    /// recorded close (#2974).
    pub async fn agent_operator_todo_close(
        &self,
        caller: Uuid,
        request: AgentOperatorTodoCloseRequestV1,
    ) -> Result<AgentOperatorTodoCloseResultV1> {
        request.validate().map_err(invalid)?;
        let store = self.store.lock().await;
        let actor = store.operator_todo_agent_actor(caller)?;
        let write = store.operator_todo_agent_close(&actor, &request)?;
        Ok(AgentOperatorTodoCloseResultV1 {
            todo: write.todo,
            deduplicated: write.deduplicated,
        })
    }
}

#[cfg(test)]
#[path = "operator_todos_tests.rs"]
mod tests;
