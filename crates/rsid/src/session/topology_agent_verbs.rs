//! RPC and native-tool glue for the scoped agent topology verbs (#633,
//! plan §5). Caller identity is always the token-resolved (RPC) or
//! registration-bound (native) session; authority, policy and the durable
//! writes live in [`crate::topology::agent`]. Every call after token
//! resolution appends exactly one `topology_agent_requests` row (V133);
//! execution-scoped events additionally land in `topology_events`, and every
//! call is traced on `rsi::topology_agent`.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use rsi_common::agent_control_schema::AgentControlVerbV1;
use rsi_common::rpc::{ResolveTopologyAttemptParams, ResolveTopologyAttemptResponse};
use rsi_common::topology_agent::{
    AgentTopologyExecuteRequestV1, AgentTopologyExecuteResultV1,
    AgentTopologyGetExecutionRequestV1, AgentTopologyGetExecutionResultV1,
    AgentTopologyInterruptRequestV1, AgentTopologyInterruptResultV1, AgentTopologyListRequestV1,
    AgentTopologyListResultV1, AgentTopologyUpsertRequestV1, AgentTopologyUpsertResultV1,
};
use uuid::Uuid;

use super::SessionManager;
use super::agent_verbs::AgentControlHandle;
use crate::bus::DaemonEvent;
use crate::error::{DaemonError, Result};
use crate::topology::agent::{self, AgentKnobs, CallAudit, CallOutcome};

/// One audit line per verb call, success or refusal.
fn audit<T>(verb: &'static str, caller: Uuid, outcome: &Result<T>) {
    match outcome {
        Ok(_) => tracing::info!(target: "rsi::topology_agent", verb, %caller, outcome = "ok"),
        Err(error) => {
            let code = match error {
                DaemonError::StructuredRpc { data, .. } => data
                    .get("code")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("request_failed")
                    .to_owned(),
                _ => "request_failed".to_owned(),
            };
            tracing::info!(target: "rsi::topology_agent", verb, %caller, outcome = %code);
        }
    }
}

/// The six scoped topology verbs, in catalog order (plan §5.1).
pub(crate) const TOPOLOGY_AGENT_VERBS: [AgentControlVerbV1; 6] = [
    AgentControlVerbV1::TopologyUpsert,
    AgentControlVerbV1::TopologyList,
    AgentControlVerbV1::TopologyExecute,
    AgentControlVerbV1::TopologyGetExecution,
    AgentControlVerbV1::TopologyInterrupt,
    AgentControlVerbV1::TopologyResolveAttempt,
];

/// Strict decode of a topology verb request. Serde diagnostics can echo
/// unknown keys or raw values, so only the closed envelope crosses the
/// agent boundary. An absent (`null`) body reads as `{}`.
fn decode<T: serde::de::DeserializeOwned>(params: &serde_json::Value) -> Result<T> {
    let value = if params.is_null() {
        serde_json::Value::Object(serde_json::Map::new())
    } else {
        params.clone()
    };
    serde_json::from_value(value).map_err(|_| {
        agent::refusal(
            "invalid_params",
            "match the request schema: rsi-rpc <AgentTopology verb> --schema",
        )
    })
}

/// The redacted `{code, next_action, ...}` object behind a refusal, for
/// native tool transports that return text instead of JSON-RPC errors.
pub(crate) fn error_json(error: DaemonError) -> String {
    match agent::redact("native", error) {
        DaemonError::StructuredRpc { data, .. } => data.to_string(),
        other => other.to_string(),
    }
}

/// Refusal for an agent verb call whose token did not resolve to a session.
pub(crate) fn unattributed() -> DaemonError {
    agent::refusal(
        "authority_denied",
        "call AgentTopology verbs from an rsi-managed session with its own session token",
    )
}

fn record_locked(store: &crate::store::Store, call: &CallAudit, outcome: CallOutcome<'_>) {
    agent::record_result(
        store,
        call,
        match outcome {
            CallOutcome::Accepted(receipt) => Ok((receipt, false)),
            CallOutcome::Deduplicated(receipt) => Ok((receipt, true)),
            CallOutcome::Refused(error) => Err(error),
        },
    );
}

fn finish<T>(verb: &'static str, caller: Uuid, outcome: Result<T>) -> Result<T> {
    let outcome = outcome.map_err(|error| agent::redact(verb, error));
    audit(verb, caller, &outcome);
    outcome
}

impl SessionManager {
    /// Single entry for the `AgentTopology*` RPC verbs and the native
    /// `rsi_control_topology_*` tools: both transports bind `caller` before
    /// calling, and both reach the same guarded service.
    pub(crate) async fn agent_topology_call(
        self: &Arc<Self>,
        caller: Uuid,
        verb: AgentControlVerbV1,
        params: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let value = match verb {
            AgentControlVerbV1::TopologyUpsert => serde_json::to_value(
                self.agent_topology_upsert(
                    caller,
                    self.decode_recorded("upsert", caller, params).await?,
                )
                .await?,
            ),
            AgentControlVerbV1::TopologyList => serde_json::to_value(
                self.agent_topology_list(
                    caller,
                    self.decode_recorded("list", caller, params).await?,
                )
                .await?,
            ),
            AgentControlVerbV1::TopologyExecute => serde_json::to_value(
                self.agent_topology_execute(
                    caller,
                    self.decode_recorded("execute", caller, params).await?,
                )
                .await?,
            ),
            AgentControlVerbV1::TopologyGetExecution => serde_json::to_value(
                self.agent_topology_get_execution(
                    caller,
                    self.decode_recorded("get_execution", caller, params)
                        .await?,
                )
                .await?,
            ),
            AgentControlVerbV1::TopologyInterrupt => serde_json::to_value(
                self.agent_topology_interrupt(
                    caller,
                    self.decode_recorded("interrupt", caller, params).await?,
                )
                .await?,
            ),
            AgentControlVerbV1::TopologyResolveAttempt => serde_json::to_value(
                self.agent_topology_resolve_attempt(
                    caller,
                    self.decode_recorded("resolve_attempt", caller, params)
                        .await?,
                )
                .await?,
            ),
            _ => {
                return Err(agent::refusal(
                    "invalid_params",
                    "use one of the AgentTopology verbs",
                ));
            }
        };
        Ok(value?)
    }

    /// Operator `UpdateTopology {shared}` (#633): the only switch that makes
    /// an operator or manager topology visible to agent callers.
    pub(crate) async fn set_topology_shared(&self, topology_id: Uuid, shared: bool) -> Result<()> {
        let store = self.store.lock().await;
        if agent::set_shared(&store, topology_id, shared)? {
            Ok(())
        } else {
            Err(super::topology_ops::TopologyError::NotFound.into())
        }
    }

    /// Strict decode; a malformed request is still one refused ledger row.
    async fn decode_recorded<T: serde::de::DeserializeOwned>(
        &self,
        verb: &'static str,
        caller: Uuid,
        params: &serde_json::Value,
    ) -> Result<T> {
        let decoded = decode(params);
        if let Err(error) = &decoded {
            audit(verb, caller, &decoded);
            let digest = agent::request_digest(params);
            self.record(
                CallAudit {
                    verb,
                    caller,
                    request_digest: digest,
                    epic_id: None,
                    topology_id: None,
                    execution_id: None,
                },
                CallOutcome::Refused(error),
            )
            .await;
        }
        decoded
    }

    /// Append one ledger row. Best effort once the effect committed: the
    /// failure is logged, never turned into a second outcome.
    async fn record(&self, call: CallAudit, outcome: CallOutcome<'_>) {
        let store = self.store.lock().await;
        record_locked(&store, &call, outcome);
    }

    pub(crate) fn topology_agent_knobs(&self) -> AgentKnobs {
        AgentKnobs {
            executor_enabled: self.topology_executor_enabled(),
            bulk_fanout_min_openrouter: self
                .runtime_config
                .topology_bulk_fanout_min_openrouter
                .load(Ordering::Relaxed),
        }
    }

    /// `AgentTopologyUpsert`.
    pub(crate) async fn agent_topology_upsert(
        &self,
        caller: Uuid,
        request: AgentTopologyUpsertRequestV1,
    ) -> Result<AgentTopologyUpsertResultV1> {
        let knobs = self.topology_agent_knobs();
        let outcome = {
            let store = self.store.lock().await;
            agent::upsert(&store, knobs, caller, &request)
        };
        let (result, written) = finish("upsert", caller, outcome)?;
        if let Some(topology) = written
            && let Err(error) = self.upsert_bridged_workflow(&topology, None).await
        {
            // Best effort, as for the operator UpdateTopology: the next
            // execute re-bridges the definition.
            tracing::warn!(%error, topology_id = %topology.id, "agent topology bridge upsert failed");
        }
        Ok(result)
    }

    /// `AgentTopologyList`.
    pub(crate) async fn agent_topology_list(
        &self,
        caller: Uuid,
        request: AgentTopologyListRequestV1,
    ) -> Result<AgentTopologyListResultV1> {
        let outcome = {
            let store = self.store.lock().await;
            let outcome = agent::list(&store, caller, &request);
            let call = CallAudit {
                verb: "list",
                caller,
                request_digest: agent::request_digest(&request),
                epic_id: request.epic_id,
                topology_id: None,
                execution_id: None,
            };
            record_locked(
                &store,
                &call,
                match &outcome {
                    Ok(_) => CallOutcome::Accepted(None),
                    Err(error) => CallOutcome::Refused(error),
                },
            );
            outcome
        };
        finish("list", caller, outcome)
    }

    /// `AgentTopologyGetExecution`.
    pub(crate) async fn agent_topology_get_execution(
        &self,
        caller: Uuid,
        request: AgentTopologyGetExecutionRequestV1,
    ) -> Result<AgentTopologyGetExecutionResultV1> {
        let outcome = {
            let store = self.store.lock().await;
            let outcome = agent::get_execution(&store, caller, &request);
            let call = CallAudit {
                verb: "get_execution",
                caller,
                request_digest: agent::request_digest(&request),
                epic_id: None,
                topology_id: None,
                execution_id: Some(request.execution_id),
            };
            record_locked(
                &store,
                &call,
                match &outcome {
                    Ok(_) => CallOutcome::Accepted(None),
                    Err(error) => CallOutcome::Refused(error),
                },
            );
            outcome
        };
        finish("get_execution", caller, outcome)
    }

    /// `AgentTopologyExecute`.
    pub(crate) async fn agent_topology_execute(
        self: &Arc<Self>,
        caller: Uuid,
        request: AgentTopologyExecuteRequestV1,
    ) -> Result<AgentTopologyExecuteResultV1> {
        // `agent::execute` appends its own ledger row.
        let outcome = agent::execute(
            &self.store,
            || self.topology_agent_knobs(),
            caller,
            &request,
        )
        .await;
        let executed = finish("execute", caller, outcome)?;
        if let Some(update) = executed.accepted {
            self.event_bus()
                .publish(DaemonEvent::GraphExecution { update });
            // P1.12 §9 parity with ExecuteTopology: mirror for the gv picker.
            if let Some(topology) = &executed.topology
                && let Err(error) = self
                    .upsert_bridged_workflow(topology, executed.project_id)
                    .await
            {
                tracing::warn!(%error, "agent topology bridge upsert failed");
            }
            let executor = self.topology_executor().await;
            self.drive_topology_execution(executor, executed.result.execution_id);
        }
        Ok(executed.result)
    }

    /// `AgentTopologyInterrupt`.
    pub(crate) async fn agent_topology_interrupt(
        self: &Arc<Self>,
        caller: Uuid,
        request: AgentTopologyInterruptRequestV1,
    ) -> Result<AgentTopologyInterruptResultV1> {
        let outcome = {
            let store = self.store.lock().await;
            // `agent::interrupt` appends its own ledger row.
            agent::interrupt(&store, caller, &request)
        };
        let (result, updates) = finish("interrupt", caller, outcome)?;
        for update in updates {
            self.event_bus()
                .publish(DaemonEvent::GraphExecution { update });
        }
        let executor = self.topology_executor().await;
        self.drive_topology_execution(executor, request.execution_id);
        Ok(result)
    }

    /// `AgentTopologyResolveAttempt`.
    pub(crate) async fn agent_topology_resolve_attempt(
        self: &Arc<Self>,
        caller: Uuid,
        params: ResolveTopologyAttemptParams,
    ) -> Result<ResolveTopologyAttemptResponse> {
        let executor = self.topology_executor().await;
        // `agent::resolve` appends its own ledger row.
        let outcome = agent::resolve(&executor, caller, &params).await;
        let response = finish("resolve_attempt", caller, outcome)?;
        if !response.deduplicated {
            self.drive_topology_execution(executor, params.execution_id);
        }
        Ok(response)
    }
}

impl AgentControlHandle {
    /// The shared manager behind the native `rsi_control_topology_*` tools;
    /// the registration-bound caller reaches the same guarded service as the
    /// token-authenticated RPC verbs.
    pub(crate) fn topology_manager(&self) -> Result<Arc<SessionManager>> {
        self.topology_agent
            .as_ref()
            .and_then(|cell| cell.get())
            .and_then(std::sync::Weak::upgrade)
            .ok_or_else(|| {
                agent::refusal(
                    "executor_unavailable",
                    "retry after the daemon finishes starting the topology executor",
                )
            })
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use rsi_common::types::SessionKind;

    fn manager(dir: &std::path::Path) -> Arc<SessionManager> {
        let store = crate::store::Store::open(&dir.join("rsi.db")).expect("store");
        let config = crate::config::Config::from_env();
        let runtime = crate::config::RuntimeConfig::from_config(&config);
        Arc::new(
            SessionManager::new(
                Arc::new(crate::bus::EventBus::new(16)),
                store,
                false,
                dir.join("daemon.sock"),
                None,
                Vec::new(),
                runtime,
                dir.join("sandboxes"),
            )
            .expect("manager"),
        )
    }

    /// Ledger `(verb, caller_kind, outcome, code)` rows of one caller.
    async fn ledger(
        manager: &SessionManager,
        caller: Uuid,
    ) -> Vec<(String, Option<String>, String, Option<String>)> {
        let store = manager.store.lock().await;
        let mut statement = store
            .conn
            .prepare(
                "SELECT verb,caller_kind,outcome,code FROM topology_agent_requests \
                 WHERE caller_session_id=?1 ORDER BY id",
            )
            .unwrap();
        statement
            .query_map([caller.to_string()], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    /// R1 [durable-audit-gap]: every one of the six verbs, through the shared
    /// RPC/native dispatcher, appends exactly one ledger row per call:
    /// accepted reads, pre-admission refusals, callers without authority and
    /// malformed requests alike.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn t4_r1_every_agent_topology_call_appends_one_ledger_row() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(dir.path());
        let (epic, lead, worker) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        {
            let store = manager.store.lock().await;
            let mut row = crate::session::agent_verbs::tests::test_session(epic, dir.path().into());
            row.session_kind = SessionKind::Epic;
            row.lead_session_id = Some(lead);
            store.insert_session(&row).unwrap();
            let mut row = crate::session::agent_verbs::tests::test_session(lead, dir.path().into());
            row.session_kind = SessionKind::Task;
            row.parent_id = Some(epic);
            store.insert_session(&row).unwrap();
        }
        let missing = Uuid::new_v4();
        let digest = format!("sha256:{}", "a".repeat(64));
        let calls = [
            (
                AgentControlVerbV1::TopologyUpsert,
                serde_json::json!({"name":"audited","scope":"epic","validate_only":true,
                    "idempotency_key":"u","definition":{"nodes":[],"edges":[]}}),
            ),
            (AgentControlVerbV1::TopologyList, serde_json::json!({})),
            (
                AgentControlVerbV1::TopologyExecute,
                serde_json::json!({"topology_id":missing,"expected_digest":digest,
                    "epic_id":epic,"idempotency_key":"e"}),
            ),
            (
                AgentControlVerbV1::TopologyGetExecution,
                serde_json::json!({"execution_id":missing}),
            ),
            (
                AgentControlVerbV1::TopologyInterrupt,
                serde_json::json!({"execution_id":missing,"expected_row_version":1,
                    "idempotency_key":"i"}),
            ),
            (
                AgentControlVerbV1::TopologyResolveAttempt,
                serde_json::json!({"execution_id":missing,"attempt_id":missing,
                    "action":"inspect","expected_row_version":1,"idempotency_key":"r"}),
            ),
        ];
        for (verb, params) in &calls {
            let _ = manager.agent_topology_call(lead, *verb, params).await;
            let _ = manager.agent_topology_call(worker, *verb, params).await;
        }
        let _ = manager
            .agent_topology_call(
                worker,
                AgentControlVerbV1::TopologyList,
                &serde_json::json!({"caller_session_id": worker}),
            )
            .await;

        let lead_kind = Some("epic_lead".to_owned());
        let refused = |verb: &str, code: &str| {
            (
                verb.to_owned(),
                lead_kind.clone(),
                "refused".to_owned(),
                Some(code.to_owned()),
            )
        };
        let accepted = |verb: &str| {
            (
                verb.to_owned(),
                lead_kind.clone(),
                "accepted".to_owned(),
                None,
            )
        };
        assert_eq!(
            ledger(&manager, lead).await,
            vec![
                accepted("upsert"),
                accepted("list"),
                refused("execute", "not_found_in_scope"),
                refused("get_execution", "not_found_in_scope"),
                refused("interrupt", "not_found_in_scope"),
                refused("resolve_attempt", "not_found_in_scope"),
            ]
        );
        let denied = |verb: &str, code: &str| {
            (
                verb.to_owned(),
                None,
                "refused".to_owned(),
                Some(code.to_owned()),
            )
        };
        assert_eq!(
            ledger(&manager, worker).await,
            vec![
                denied("upsert", "authority_denied"),
                denied("list", "authority_denied"),
                denied("execute", "authority_denied"),
                denied("get_execution", "authority_denied"),
                denied("interrupt", "authority_denied"),
                denied("resolve_attempt", "authority_denied"),
                denied("list", "invalid_params"),
            ]
        );
    }
}
