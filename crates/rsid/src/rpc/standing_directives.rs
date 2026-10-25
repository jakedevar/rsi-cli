//! #2649: standing operator directive RPCs.
//!
//! `AgentStandingDirective` is an agent verb: the caller is the token-resolved
//! session and its manager seat decides which scopes it may write.
//! `StandingDirectiveSet`, `StandingDirectiveClear` and `StandingDirectiveList`
//! are operator-only: they are not in the attributed verb registry, so a
//! tokened caller is default-denied (AGENTS.md rule 10).

use super::*;
use chrono::Utc;
use rsi_common::standing_directives::{
    STANDING_DIRECTIVE_INVALID_REQUEST, StandingDirectiveClearRequestV1,
    StandingDirectiveListRequestV1, StandingDirectiveListResultV1, StandingDirectiveSetRequestV1,
    StandingDirectiveWriteResultV1,
};

use crate::store::standing_directives::{StandingDirectiveActor, StandingDirectiveSetInput};

fn decode<T: serde::de::DeserializeOwned>(request: &RpcRequest) -> Result<T> {
    let params = if request.params.is_null() {
        serde_json::json!({})
    } else {
        request.params.clone()
    };
    serde_json::from_value(params)
        .map_err(|_| DaemonError::InvalidParam(STANDING_DIRECTIVE_INVALID_REQUEST.into()))
}

fn invalid(code: &'static str) -> DaemonError {
    DaemonError::InvalidParam(code.into())
}

impl RpcServer {
    pub(super) async fn handle_agent_standing_directive(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let params = decode(request)?;
        let result = self
            .session_manager
            .agent_control()
            .agent_standing_directive(caller, params)
            .await?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_standing_directive_set(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: StandingDirectiveSetRequestV1 = decode(request)?;
        params.validate().map_err(invalid)?;
        let store = self.session_manager.store();
        let write = store.lock().await.standing_directive_set(
            &StandingDirectiveActor::Operator,
            &StandingDirectiveSetInput {
                scope: params.scope,
                text: &params.text,
                provenance: None,
                kind: rsi_common::standing_directives::StandingDirectiveKindV1::OperatorRelay,
                expires_in_secs: params.expires_in_secs,
                idempotency_key: &params.idempotency_key,
            },
            Utc::now(),
        )?;
        Ok(serde_json::to_value(StandingDirectiveWriteResultV1 {
            directive: write.directive,
            deduplicated: write.deduplicated,
        })?)
    }

    pub(super) async fn handle_standing_directive_clear(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: StandingDirectiveClearRequestV1 = decode(request)?;
        params.validate().map_err(invalid)?;
        let store = self.session_manager.store();
        let write = store.lock().await.standing_directive_clear(
            &StandingDirectiveActor::Operator,
            params.directive_id,
            Utc::now(),
        )?;
        Ok(serde_json::to_value(StandingDirectiveWriteResultV1 {
            directive: write.directive,
            deduplicated: write.deduplicated,
        })?)
    }

    pub(super) async fn handle_standing_directive_list(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: StandingDirectiveListRequestV1 = decode(request)?;
        let store = self.session_manager.store();
        let directives = store.lock().await.standing_directive_list(
            params.scope,
            params.include_inactive,
            Utc::now(),
        )?;
        Ok(serde_json::to_value(StandingDirectiveListResultV1 {
            directives,
        })?)
    }
}
