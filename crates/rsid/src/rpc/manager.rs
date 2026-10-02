use super::*;

pub(super) fn manager_node_view(
    node: crate::store::manager_nodes::AreaNode,
) -> rsi_common::manager_nodes::ManagerNodeViewV1 {
    use rsi_common::manager_nodes::{
        ManagerNodeGrantStateV1, ManagerNodeStateV1, ManagerNodeViewV1,
    };
    let state = if node.active {
        ManagerNodeStateV1::Active
    } else {
        ManagerNodeStateV1::Revoked
    };
    let grant_state = if !node.active {
        ManagerNodeGrantStateV1::Revoked
    } else if node.grant.is_some() {
        ManagerNodeGrantStateV1::Granted
    } else {
        ManagerNodeGrantStateV1::Absent
    };
    ManagerNodeViewV1 {
        node_id: node.id,
        parent_node_id: node.parent_node_id,
        seat_root_session_id: node.seat_root_session_id,
        project_id: node.project_id,
        selector: node.selector,
        state,
        grant_state,
        grant: node.grant,
        policy: node.policy,
        grant_version: node.grant_version,
        policy_version: node.policy_version,
        authority_epoch: node.authority_epoch,
        direct_reports: node.direct_reports,
        updated_at: node.updated_at,
    }
}

impl RpcServer {
    pub(super) async fn handle_agent_manager_progress(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let params: AgentManagerProgressRequestV1 =
            rsi_common::harness_manager::decode_manager_request(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam("manager_invalid_request".into()))?;
        let result = self
            .session_manager
            .agent_control()
            .agent_manager_progress(caller, params)
            .await?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_agent_manager_inbox(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let params: AgentManagerInboxRequestV1 =
            rsi_common::harness_manager::decode_manager_request(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam("manager_invalid_request".into()))?;
        let result = self
            .session_manager
            .agent_control()
            .agent_manager_inbox(caller, params)
            .await?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_agent_manager_send(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let params: AgentManagerSendRequestV1 =
            rsi_common::harness_manager::decode_manager_request(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam("manager_invalid_request".into()))?;
        let result = self
            .session_manager
            .agent_control()
            .agent_manager_send(caller, params)
            .await?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_agent_manager_reply(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let params: AgentManagerReplyRequestV1 =
            rsi_common::harness_manager::decode_manager_request(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam("manager_invalid_request".into()))?;
        let result = self
            .session_manager
            .agent_control()
            .agent_manager_reply(caller, params)
            .await?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_agent_manager_notify(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let params: AgentManagerNotifyRequestV1 =
            rsi_common::harness_manager::decode_manager_request(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam("manager_invalid_request".into()))?;
        params
            .validate()
            .map_err(|code| DaemonError::InvalidParam(code.into()))?;
        let result = self
            .session_manager
            .agent_control()
            .agent_manager_notify(caller, params)
            .await?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_agent_manager_inspect(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let params: rsi_common::harness_manager_v2::AgentManagerInspectRequestV2 =
            rsi_common::harness_manager::decode_manager_request(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam("manager_invalid_request".into()))?;
        let result = self
            .session_manager
            .agent_control()
            .agent_manager_inspect(caller, params)
            .await?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_agent_manager_update(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let params: rsi_common::harness_manager_v2::AgentManagerUpdateRequestV2 =
            rsi_common::harness_manager::decode_manager_request(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam("manager_invalid_request".into()))?;
        let result = self
            .session_manager
            .agent_control()
            .agent_manager_update(caller, params)
            .await?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_agent_submit_review_receipt(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let params: rsi_common::harness_manager_v2::AgentSubmitReviewReceiptRequestV1 =
            rsi_common::harness_manager::decode_manager_request(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam("manager_invalid_request".into()))?;
        let result = self
            .session_manager
            .agent_control()
            .agent_submit_review_receipt(caller, params)
            .await?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_agent_manager_control(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let params: rsi_common::harness_manager_v2::AgentManagerControlRequestV2 =
            rsi_common::harness_manager::decode_manager_request(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam("manager_invalid_request".into()))?;
        let result = self
            .session_manager
            .agent_control()
            .agent_manager_control(caller, params)
            .await?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_agent_manager_prepare_control(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let params: rsi_common::harness_manager_v2::AgentManagerPrepareControlRequestV2 =
            rsi_common::harness_manager::decode_manager_request(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam("manager_invalid_request".into()))?;
        let result = self
            .session_manager
            .agent_control()
            .agent_manager_prepare_control(caller, params)
            .await?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_agent_manager_commit_prepared_control(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let params: rsi_common::harness_manager_v2::AgentManagerCommitPreparedControlRequestV2 =
            rsi_common::harness_manager::decode_manager_request(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam("manager_invalid_request".into()))?;
        let result = self
            .session_manager
            .agent_control()
            .agent_manager_commit_prepared_control(caller, params)
            .await?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_agent_manager_get_action(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let params: rsi_common::harness_manager_v2::AgentManagerGetActionRequestV2 =
            rsi_common::harness_manager::decode_manager_request(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam("manager_invalid_request".into()))?;
        let result = self
            .session_manager
            .agent_control()
            .agent_manager_get_action(caller, params)
            .await?;
        Ok(serde_json::to_value(result)?)
    }

    /// Issue #548: read-only projection for a manager-created session. The
    /// caller is token-bound; Epic, manager and scope are daemon-derived.
    pub(super) async fn handle_agent_manager_work_view(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let params: rsi_common::harness_manager::AgentManagerWorkViewRequestV1 =
            rsi_common::harness_manager::decode_manager_request(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam("manager_invalid_request".into()))?;
        let result = self
            .session_manager
            .agent_control()
            .agent_manager_work_view(caller, params)
            .await?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_get_harness_manager_policy(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::harness_manager::GetHarnessManagerRequestV1 =
            rsi_common::harness_manager::decode_manager_request(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam("manager_invalid_request".into()))?;
        let store = self.session_manager.store();
        let result = store
            .lock()
            .await
            .get_harness_manager_policy(params.project_id)?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_configure_harness_manager_policy(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::harness_manager_v2::ConfigureHarnessManagerPolicyRequestV2 =
            rsi_common::harness_manager::decode_manager_request(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam("manager_invalid_request".into()))?;
        let store = self.session_manager.store();
        let result = store
            .lock()
            .await
            .configure_harness_manager_policy(&params)?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_get_harness_manager_state(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::harness_manager_v2::GetHarnessManagerStateRequestV2 =
            rsi_common::harness_manager::decode_manager_request(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam("manager_invalid_request".into()))?;
        let result = self
            .session_manager
            .agent_control()
            .get_harness_manager_state(params)
            .await?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_answer_harness_manager_decision(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::harness_manager_v2::AnswerHarnessManagerDecisionRequestV2 =
            rsi_common::harness_manager::decode_manager_request(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam("manager_invalid_request".into()))?;
        let result = self
            .session_manager
            .answer_harness_manager_decision(params)
            .await?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_get_harness_manager(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: GetHarnessManagerRequestV1 =
            rsi_common::harness_manager::decode_manager_request(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam("manager_invalid_request".into()))?;
        let store = self.session_manager.store();
        let result = store.lock().await.get_harness_manager(params.project_id)?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_list_harness_manager_epics(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::harness_manager::ListHarnessManagerEpicsRequestV1 =
            rsi_common::harness_manager::decode_manager_request(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam("manager_invalid_request".into()))?;
        let store = self.session_manager.store();
        let result = store.lock().await.list_harness_manager_epics(&params)?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_list_harness_manager_scope(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::harness_manager::ListHarnessManagerScopeRequestV1 =
            rsi_common::harness_manager::decode_manager_request(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam("manager_invalid_request".into()))?;
        let store = self.session_manager.store();
        let result = store.lock().await.list_harness_manager_scope(&params)?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_configure_harness_manager(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ConfigureHarnessManagerRequestV1 =
            rsi_common::harness_manager::decode_manager_request(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam("manager_invalid_request".into()))?;
        let store = self.session_manager.store();
        let result = store.lock().await.configure_harness_manager(&params)?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_list_manager_nodes(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        use rsi_common::manager_nodes::{ListManagerNodesRequestV1, ListManagerNodesResultV1};
        let params: ListManagerNodesRequestV1 =
            rsi_common::harness_manager::decode_manager_request(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam("manager_node_invalid_request".into()))?;
        params
            .validate()
            .map_err(|error| DaemonError::InvalidParam(error.into()))?;
        let store = self.session_manager.store();
        let rows = store.lock().await.list_area_nodes(params.project_id)?;
        let mut rows: Vec<_> = rows
            .into_iter()
            .map(manager_node_view)
            .filter(|row| params.after_node_id.is_none_or(|after| row.node_id > after))
            .collect();
        rows.sort_by_key(|row| row.node_id);
        let more = rows.len() > usize::from(params.limit);
        rows.truncate(usize::from(params.limit));
        let result = ListManagerNodesResultV1 {
            next_after_node_id: more.then(|| rows.last().expect("nonempty page").node_id),
            rows,
        };
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_get_manager_node(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::manager_nodes::GetManagerNodeRequestV1 =
            rsi_common::harness_manager::decode_manager_request(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam("manager_node_invalid_request".into()))?;
        params
            .validate()
            .map_err(|error| DaemonError::InvalidParam(error.into()))?;
        let store = self.session_manager.store();
        let result = store
            .lock()
            .await
            .get_area_node(params.project_id, params.node_id)?
            .map(manager_node_view);
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_configure_manager_node(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        use crate::store::manager_nodes::AppointAreaNode;
        let params: rsi_common::manager_nodes::ConfigureManagerNodeRequestV1 =
            rsi_common::harness_manager::decode_manager_request(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam("manager_node_invalid_request".into()))?;
        params
            .validate()
            .map_err(|error| DaemonError::InvalidParam(error.into()))?;
        let store = self.session_manager.store();
        let result = store.lock().await.appoint_area_node(&AppointAreaNode {
            idempotency_key: Some(params.idempotency_key),
            node_id: params.node_id,
            expected_node_grant_version: params.expected_node_grant_version,
            project_id: params.project_id,
            parent_node_id: params.parent_node_id,
            expected_parent_grant_version: params.expected_parent_grant_version,
            expected_parent_policy_version: params.expected_parent_policy_version,
            expected_parent_epoch: params.expected_parent_authority_epoch,
            seat_root_session_id: params.seat_root_session_id,
            selector: params.selector,
            grant: params.grant,
            policy: params.policy,
            operator_origin: "operator_rpc".into(),
        })?;
        Ok(serde_json::to_value(manager_node_view(result))?)
    }

    pub(super) async fn handle_agent_manager_delegate_node(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        use crate::store::manager_nodes::AppointAreaNode;
        let caller = self.resolve_caller_session_id(request).await?;
        let params: rsi_common::manager_nodes::DelegateManagerNodeRequestV1 =
            rsi_common::harness_manager::decode_manager_request(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam("manager_node_invalid_request".into()))?;
        params
            .validate()
            .map_err(|error| DaemonError::InvalidParam(error.into()))?;
        let store = self.session_manager.store();
        let store = store.lock().await;
        let project_id = store
            .area_node_project(params.parent_node_id)?
            .ok_or_else(|| DaemonError::InvalidParam("manager_node_delegate_denied".into()))?;
        let params = params.into_configuration(project_id);
        let result = store.delegate_area_node(
            caller,
            &AppointAreaNode {
                idempotency_key: Some(params.idempotency_key),
                node_id: params.node_id,
                expected_node_grant_version: params.expected_node_grant_version,
                project_id: params.project_id,
                parent_node_id: params.parent_node_id,
                expected_parent_grant_version: params.expected_parent_grant_version,
                expected_parent_policy_version: params.expected_parent_policy_version,
                expected_parent_epoch: params.expected_parent_authority_epoch,
                seat_root_session_id: params.seat_root_session_id,
                selector: params.selector,
                grant: params.grant,
                policy: params.policy,
                operator_origin: format!("manager:{caller}"),
            },
        )?;
        Ok(serde_json::to_value(manager_node_view(result))?)
    }

    pub(super) async fn handle_agent_manager_escalate(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let params: rsi_common::harness_manager::AgentManagerEscalateInputV1 =
            rsi_common::harness_manager::decode_manager_request(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam("manager_node_invalid_escalation".into()))?;
        params
            .validate()
            .map_err(|error| DaemonError::InvalidParam(error.into()))?;
        let store = self.session_manager.store();
        let store = store.lock().await;
        let project = store
            .get_session(caller)?
            .and_then(|session| session.project_id)
            .ok_or_else(|| DaemonError::InvalidParam("manager_node_escalation_denied".into()))?;
        let result = store.create_manager_node_escalation(caller, &params.into_request(project))?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_agent_manager_list_escalations(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let _: rsi_common::harness_manager::AgentManagerListEscalationsRequestV1 =
            rsi_common::harness_manager::decode_manager_request(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam("manager_node_invalid_escalation".into()))?;
        let store = self.session_manager.store();
        let store = store.lock().await;
        let project = store
            .get_session(caller)?
            .and_then(|session| session.project_id)
            .ok_or_else(|| DaemonError::InvalidParam("manager_node_escalation_denied".into()))?;
        Ok(serde_json::to_value(
            store.list_manager_node_escalations(caller, project)?,
        )?)
    }

    pub(super) async fn handle_agent_manager_resolve_escalation(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let params: rsi_common::harness_manager::AgentManagerResolveEscalationRequestV1 =
            rsi_common::harness_manager::decode_manager_request(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam("manager_node_invalid_escalation".into()))?;
        params
            .validate()
            .map_err(|error| DaemonError::InvalidParam(error.into()))?;
        let result = self
            .session_manager
            .store()
            .lock()
            .await
            .resolve_manager_node_escalation(caller, &params)?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_revoke_manager_node(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        use crate::store::manager_nodes::RevokeAreaNode;
        let params: rsi_common::manager_nodes::RevokeManagerNodeRequestV1 =
            rsi_common::harness_manager::decode_manager_request(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam("manager_node_invalid_request".into()))?;
        params
            .validate()
            .map_err(|error| DaemonError::InvalidParam(error.into()))?;
        let store = self.session_manager.store();
        let store = store.lock().await;
        store.revoke_area_node(&RevokeAreaNode {
            idempotency_key: Some(params.idempotency_key),
            project_id: params.project_id,
            node_id: params.node_id,
            expected_grant_version: params.expected_grant_version,
            expected_epoch: params.expected_authority_epoch,
            operator_origin: "operator_rpc".into(),
        })?;
        let result = store
            .get_area_node(params.project_id, params.node_id)?
            .ok_or_else(|| DaemonError::Store("revoked manager node missing".into()))?;
        Ok(serde_json::to_value(manager_node_view(result))?)
    }
}
