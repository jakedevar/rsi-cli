//! Sessions the operator DRAIN wait need not wait for (#2353).
//!
//! A DRAIN restart waits up to `OPERATOR_DRAIN_LIMIT` for open turns to end.
//! A turn whose only open work is a call the *daemon* is serving cannot end
//! while the daemon drains, so waiting for it only burns the whole limit. This
//! module proves that case from daemon-owned facts and never from idle time:
//!
//! 1. **The in-flight control-call registry.** `RpcServer` registers every
//!    admitted, token-attributed request for exactly as long as its handler
//!    runs ([`DaemonWaitRegistry::begin`], an RAII guard, so a dropped or
//!    cancelled handler unregisters too). A call counts only once it has been
//!    in flight for [`DAEMON_WAIT_SETTLE`]; a call that is about to return is
//!    just a turn between tools.
//! 2. **The turn's tool ledger** ([`TurnToolLedger`]). The monitor feeds it
//!    from the provider stream: every `tool_use` block opens a tool, its
//!    `tool_result` closes it, `result` ends the turn. The turn's open work is
//!    daemon-owned only when it is nothing but typed `rsi-agent` control tools
//!    (their MCP gateway is a daemon client by construction). Every other open
//!    tool (edit, read, search, subagent, **a shell**), a turn with no open
//!    tool (the model is sampling), and a turn the ledger did not see from its
//!    start keep the full wait.
//!
//! **A shell tool never counts (#2388, #2387).** A shell can run builtins (a
//! write loop, a `read`, arithmetic) with no child program at all, and a
//! process listing can neither tell it from a shell blocked on its client nor
//! always read a working sibling. The registry shows that a daemon call is
//! being served, not that the shell is not also working, so a shell tool keeps
//! the full wait. What still ends early is a manager parked on a pure control
//! call such as `AgentEnqueueLandingSource`.
//!
//! **The classification is one synchronous read (#2387).**
//! [`DaemonWaitRegistry::is_waiting_on_daemon`] has no await point, and the
//! DRAIN wait calls it for every session under one read of the active map (the
//! lock the monitor takes to update the ledger). The ledger and the registry
//! are read at the instant the decision acts, never from an earlier snapshot:
//! a tool that opens, or a call that completes, before that read is a full
//! wait.
//!
//! Only the Claude CLI stream is tracked; every other provider keeps the full
//! wait. What a cut turn does afterwards is the existing graceful-restart
//! journal: `interrupt_restart_sessions` records the intent and the next
//! process continues the same session (`reconcile_restart_intents_at_startup`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use tokio::time::{Duration, Instant};
use uuid::Uuid;

/// How long a control call must already be in flight before the session
/// counts as waiting on the daemon. A shorter call is a turn between tools.
pub(crate) const DAEMON_WAIT_SETTLE: Duration = Duration::from_secs(1);

/// `mcp__rsi-agent__<tool>`: a typed control tool served by the daemon.
fn is_control_tool(name: &str) -> bool {
    name.strip_prefix("mcp__")
        .and_then(|rest| rest.strip_prefix(crate::claude::AGENT_MCP_SERVER_NAME))
        .is_some_and(|rest| rest.starts_with("__"))
}

/// The tools the current turn has opened and not yet seen a result for.
#[derive(Debug, Clone, Default)]
pub(crate) struct TurnToolLedger {
    /// `tool_use_id` -> tool name.
    open: HashMap<String, String>,
    /// The ledger cannot vouch for the turn: it was adopted mid-turn, or a
    /// tool call arrived without an identity, or a result without its call.
    untracked: bool,
}

impl TurnToolLedger {
    /// A ledger that never vouches for its turn (an adopted or restored
    /// session was not observed from the start of its turn).
    pub(crate) fn untracked() -> Self {
        Self {
            open: HashMap::new(),
            untracked: true,
        }
    }

    /// Apply one raw provider stream event (Claude CLI stream-json).
    pub(crate) fn observe_stream(&mut self, event_type: &str, data: &serde_json::Value) {
        let blocks = || {
            data.get("message")
                .and_then(|message| message.get("content"))
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
        };
        match event_type {
            "assistant" => {
                for block in blocks() {
                    if block.get("type").and_then(serde_json::Value::as_str) != Some("tool_use") {
                        continue;
                    }
                    let id = block.get("id").and_then(serde_json::Value::as_str);
                    let name = block.get("name").and_then(serde_json::Value::as_str);
                    match (id, name) {
                        (Some(id), Some(name)) => {
                            self.open.insert(id.to_owned(), name.to_owned());
                        }
                        _ => self.untracked = true,
                    }
                }
            }
            "user" => {
                for block in blocks() {
                    if block.get("type").and_then(serde_json::Value::as_str) != Some("tool_result")
                    {
                        continue;
                    }
                    let closed = block
                        .get("tool_use_id")
                        .and_then(serde_json::Value::as_str)
                        .and_then(|id| self.open.remove(id));
                    if closed.is_none() {
                        self.untracked = true;
                    }
                }
            }
            // The turn is over: no tool can still be open.
            "result" => self.open.clear(),
            _ => {}
        }
    }

    /// Whether every open tool is a typed `rsi-agent` control tool (and the
    /// ledger saw the turn from its start and at least one tool is open).
    pub(crate) fn only_control_tools_open(&self) -> bool {
        !self.untracked
            && !self.open.is_empty()
            && self.open.values().all(|name| is_control_tool(name))
    }
}

/// An admitted control call the daemon is serving.
#[derive(Debug, Clone, Copy)]
struct InFlightCall {
    id: u64,
    since: Instant,
}

/// Admitted control calls the daemon is serving right now, per session.
#[derive(Default)]
pub(crate) struct DaemonWaitRegistry {
    calls: Mutex<HashMap<Uuid, Vec<InFlightCall>>>,
    next_id: AtomicU64,
}

/// Unregisters its call when the handler returns or is dropped.
pub(crate) struct InFlightCallGuard {
    registry: Arc<DaemonWaitRegistry>,
    session_id: Uuid,
    id: u64,
}

impl Drop for InFlightCallGuard {
    fn drop(&mut self) {
        let mut calls = lock(&self.registry.calls);
        if let Some(session_calls) = calls.get_mut(&self.session_id) {
            session_calls.retain(|call| call.id != self.id);
            if session_calls.is_empty() {
                calls.remove(&self.session_id);
            }
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl DaemonWaitRegistry {
    /// Register one admitted control call of `session_id`.
    pub(crate) fn begin(self: &Arc<Self>, session_id: Uuid) -> InFlightCallGuard {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        lock(&self.calls)
            .entry(session_id)
            .or_default()
            .push(InFlightCall {
                id,
                since: Instant::now(),
            });
        InFlightCallGuard {
            registry: Arc::clone(self),
            session_id,
            id,
        }
    }

    /// A call already past its settle time, for tests that do not run on a
    /// paused clock.
    #[cfg(test)]
    pub(crate) fn begin_settled_for_test(self: &Arc<Self>, session_id: Uuid) -> InFlightCallGuard {
        let guard = self.begin(session_id);
        let settled = Instant::now()
            .checked_sub(DAEMON_WAIT_SETTLE * 2)
            .expect("the clock has run for at least two settle times");
        for call in lock(&self.calls)
            .get_mut(&session_id)
            .into_iter()
            .flatten()
            .filter(|call| call.id == guard.id)
        {
            call.since = settled;
        }
        guard
    }

    /// Calls of `session_id` the daemon is serving right now.
    #[cfg(test)]
    pub(crate) fn in_flight(&self, session_id: Uuid) -> usize {
        lock(&self.calls).get(&session_id).map_or(0, Vec::len)
    }

    /// Whether the session's only open work is a call the daemon is serving:
    /// every open tool is a typed control tool and one such call has been in
    /// flight past [`DAEMON_WAIT_SETTLE`]. Synchronous on purpose (#2387): the
    /// caller reads `tools` and this registry at the same instant it decides.
    pub(crate) fn is_waiting_on_daemon(
        &self,
        session_id: Uuid,
        tools: &TurnToolLedger,
        now: Instant,
    ) -> bool {
        tools.only_control_tools_open()
            && lock(&self.calls)
                .get(&session_id)
                .into_iter()
                .flatten()
                .any(|call| now.saturating_duration_since(call.since) >= DAEMON_WAIT_SETTLE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn assistant_calls(tools: &[(&str, &str)]) -> serde_json::Value {
        json!({"message": {"content": tools.iter()
            .map(|(id, name)| json!({"type": "tool_use", "id": id, "name": name, "input": {}}))
            .collect::<Vec<_>>()}})
    }

    fn user_results(ids: &[&str]) -> serde_json::Value {
        json!({"message": {"content": ids.iter()
            .map(|id| json!({"type": "tool_result", "tool_use_id": id, "content": ""}))
            .collect::<Vec<_>>()}})
    }

    fn ledger_with(tools: &[(&str, &str)]) -> TurnToolLedger {
        let mut ledger = TurnToolLedger::default();
        ledger.observe_stream("assistant", &assistant_calls(tools));
        ledger
    }

    const CONTROL: &str = "mcp__rsi-agent__rsi_control_get_issue";

    fn registry() -> Arc<DaemonWaitRegistry> {
        Arc::new(DaemonWaitRegistry::default())
    }

    fn waiting(registry: &DaemonWaitRegistry, session: Uuid, tools: &TurnToolLedger) -> bool {
        registry.is_waiting_on_daemon(session, tools, Instant::now())
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn the_ledger_opens_a_tool_on_its_call_and_closes_it_on_its_result_2353() {
        let mut ledger = ledger_with(&[("a", CONTROL), ("b", "Bash")]);
        assert!(
            !ledger.only_control_tools_open(),
            "a control tool beside a shell"
        );
        ledger.observe_stream("user", &user_results(&["b"]));
        assert!(ledger.only_control_tools_open());
        ledger.observe_stream("user", &user_results(&["a"]));
        assert!(
            !ledger.only_control_tools_open(),
            "no tool is open: the model is sampling"
        );
        ledger.observe_stream("assistant", &assistant_calls(&[("c", CONTROL)]));
        assert!(ledger.only_control_tools_open());
        ledger.observe_stream("result", &json!({}));
        assert!(
            !ledger.only_control_tools_open(),
            "the turn result closes every tool"
        );
    }

    /// Only typed control tools are daemon-owned. A shell tool, alone or not,
    /// keeps the full wait (#2388): it may be running builtins no process
    /// listing shows.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn only_typed_control_tools_can_be_daemon_owned_2353() {
        for (tools, expected) in [
            (vec![("a", CONTROL)], true),
            (
                vec![("a", CONTROL), ("b", "mcp__rsi-agent__schedule_wake")],
                true,
            ),
            (vec![("a", "Bash")], false),
            (vec![("a", "Bash"), ("b", CONTROL)], false),
            (vec![("a", "Bash"), ("b", "Bash")], false),
            (vec![("a", "Read")], false),
            (vec![("a", "Edit"), ("b", CONTROL)], false),
            (vec![("a", "Task")], false),
            // Another server's tool is not a daemon call.
            (vec![("a", "mcp__other__rsi_control_get_issue")], false),
            (vec![("a", "mcp__rsi-agentx__tool")], false),
        ] {
            assert_eq!(
                ledger_with(&tools).only_control_tools_open(),
                expected,
                "{tools:?}"
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn a_ledger_that_missed_part_of_the_turn_never_vouches_for_it_2353() {
        let mut adopted = TurnToolLedger::untracked();
        adopted.observe_stream("assistant", &assistant_calls(&[("a", CONTROL)]));
        assert!(!adopted.only_control_tools_open(), "adopted mid-turn");

        let mut unmatched = ledger_with(&[("a", CONTROL)]);
        unmatched.observe_stream("user", &user_results(&["unseen"]));
        assert!(
            !unmatched.only_control_tools_open(),
            "a result whose call was missed"
        );
        unmatched.observe_stream("result", &json!({}));
        assert!(
            !unmatched.only_control_tools_open(),
            "the doubt outlives the turn result"
        );

        let mut nameless = TurnToolLedger::default();
        nameless.observe_stream(
            "assistant",
            &json!({"message": {"content": [{"type": "tool_use", "id": "a"}]}}),
        );
        assert!(
            !nameless.only_control_tools_open(),
            "a call with no tool name"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test(start_paused = true)]
    async fn a_control_call_counts_only_after_it_has_been_in_flight_for_the_settle_time_2353() {
        let registry = registry();
        let session = Uuid::new_v4();
        let tools = ledger_with(&[("a", CONTROL)]);

        assert!(
            !waiting(&registry, session, &tools),
            "no call in flight: the tool is between calls"
        );
        let call = registry.begin(session);
        assert_eq!(registry.in_flight(session), 1);
        assert!(
            !waiting(&registry, session, &tools),
            "a call that has only just started"
        );
        tokio::time::advance(DAEMON_WAIT_SETTLE / 2).await;
        assert!(!waiting(&registry, session, &tools));
        tokio::time::advance(DAEMON_WAIT_SETTLE / 2).await;
        assert!(waiting(&registry, session, &tools));
        drop(call);
        assert_eq!(
            registry.in_flight(session),
            0,
            "the guard unregisters on drop"
        );
        assert!(!waiting(&registry, session, &tools), "the call returned");
    }

    /// A settled call never makes a shell tool a daemon wait (#2388).
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test(start_paused = true)]
    async fn a_settled_call_never_makes_a_shell_tool_a_daemon_wait_2388() {
        let registry = registry();
        let session = Uuid::new_v4();
        let _call = registry.begin(session);
        tokio::time::advance(DAEMON_WAIT_SETTLE * 2).await;
        assert!(waiting(&registry, session, &ledger_with(&[("a", CONTROL)])));
        assert!(!waiting(&registry, session, &ledger_with(&[("a", "Bash")])));
        assert!(!waiting(
            &registry,
            session,
            &ledger_with(&[("a", "Bash"), ("b", CONTROL)])
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test(start_paused = true)]
    async fn calls_of_other_sessions_never_count_for_this_one_2353() {
        let registry = registry();
        let (session, other) = (Uuid::new_v4(), Uuid::new_v4());
        let _call = registry.begin(other);
        tokio::time::advance(DAEMON_WAIT_SETTLE * 2).await;
        assert!(!waiting(
            &registry,
            session,
            &ledger_with(&[("a", CONTROL)])
        ));
    }
}
