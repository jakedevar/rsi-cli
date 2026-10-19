//! #1747: the operator to-do list RPCs.
//!
//! `AgentOperatorTodoUpsert` and `AgentOperatorTodoClose` are agent verbs: the
//! caller is the token-resolved session and its manager seat decides the scope.
//! `OperatorTodoList`, `OperatorTodoAdd`, `OperatorTodoClose` and
//! `OperatorTodoReopen` (#2950) are operator-only: they are not in the attributed verb registry, so a tokened
//! caller is default-denied (AGENTS.md rule 10).

use super::*;
use rsi_common::operator_todos::{
    OPERATOR_TODO_INVALID_REQUEST, OperatorTodoAddRequestV1, OperatorTodoAddResultV1,
    OperatorTodoCloseRequestV1, OperatorTodoCloseResultV1, OperatorTodoListRequestV1,
    OperatorTodoReopenRequestV1, OperatorTodoReopenResultV1,
};

fn decode<T: serde::de::DeserializeOwned>(request: &RpcRequest) -> Result<T> {
    let params = if request.params.is_null() {
        serde_json::json!({})
    } else {
        request.params.clone()
    };
    serde_json::from_value(params)
        .map_err(|_| DaemonError::InvalidParam(OPERATOR_TODO_INVALID_REQUEST.into()))
}

impl RpcServer {
    pub(super) async fn handle_agent_operator_todo_upsert(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let params = decode(request)?;
        let result = self
            .session_manager
            .agent_control()
            .agent_operator_todo_upsert(caller, params)
            .await?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_agent_operator_todo_close(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let params = decode(request)?;
        let result = self
            .session_manager
            .agent_control()
            .agent_operator_todo_close(caller, params)
            .await?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_operator_todo_list(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: OperatorTodoListRequestV1 = decode(request)?;
        let store = self.session_manager.store();
        let result = store.lock().await.operator_todo_list(params.closed_since)?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_operator_todo_add(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: OperatorTodoAddRequestV1 = decode(request)?;
        let store = self.session_manager.store();
        let write = store.lock().await.operator_todo_add(&params)?;
        Ok(serde_json::to_value(OperatorTodoAddResultV1 {
            todo: write.todo,
            deduplicated: write.deduplicated,
        })?)
    }

    pub(super) async fn handle_operator_todo_close(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: OperatorTodoCloseRequestV1 = decode(request)?;
        let store = self.session_manager.store();
        let todo = store.lock().await.operator_todo_operator_close(&params)?;
        Ok(serde_json::to_value(OperatorTodoCloseResultV1 { todo })?)
    }

    pub(super) async fn handle_operator_todo_reopen(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: OperatorTodoReopenRequestV1 = decode(request)?;
        let store = self.session_manager.store();
        let write = store.lock().await.operator_todo_operator_reopen(&params)?;
        Ok(serde_json::to_value(OperatorTodoReopenResultV1 {
            todo: write.todo,
            deduplicated: write.deduplicated,
        })?)
    }
}
