//! Operator quiet-point restart (#1122) RPC handlers.
//!
//! `RequestOperatorRestart`, `GetOperatorRestart`, `CancelOperatorRestart` and
//! `ForceOperatorRestart` are operator-only: they are not declared in the
//! attributed verb registry, so a tokened caller is default-denied (AGENTS.md
//! rule 10).

use super::{RpcRequest, RpcServer};
use crate::deploy::DeployService;
use crate::deploy_operator;
use crate::error::{DaemonError, Result};
use chrono::Utc;
use rsi_common::operator_restart::{
    OperatorRestartNoParamsV1, RESTART_INVALID_REQUEST, RequestOperatorRestartRequestV1,
};

fn decode<T: serde::de::DeserializeOwned>(request: &RpcRequest) -> Result<T> {
    let params = if request.params.is_null() {
        serde_json::json!({})
    } else {
        request.params.clone()
    };
    serde_json::from_value(params)
        .map_err(|_| DaemonError::InvalidParam(RESTART_INVALID_REQUEST.into()))
}

impl RpcServer {
    pub(super) async fn handle_request_operator_restart(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: RequestOperatorRestartRequestV1 = decode(request)?;
        let store = self.session_manager.store();
        let status =
            deploy_operator::request(store, DeployService::global(), params, Utc::now()).await?;
        Ok(serde_json::to_value(status)?)
    }

    pub(super) async fn handle_get_operator_restart(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let _: OperatorRestartNoParamsV1 = decode(request)?;
        let store = self.session_manager.store();
        let status = deploy_operator::get(store, DeployService::global()).await?;
        Ok(serde_json::to_value(status)?)
    }

    pub(super) async fn handle_cancel_operator_restart(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let _: OperatorRestartNoParamsV1 = decode(request)?;
        let store = self.session_manager.store();
        let status = deploy_operator::cancel(store, DeployService::global(), Utc::now()).await?;
        Ok(serde_json::to_value(status)?)
    }

    pub(super) async fn handle_force_operator_restart(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let _: OperatorRestartNoParamsV1 = decode(request)?;
        let store = self.session_manager.store();
        let status = deploy_operator::force(store, DeployService::global()).await?;
        Ok(serde_json::to_value(status)?)
    }
}
