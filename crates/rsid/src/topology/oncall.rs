//! The on-call manager seat of a topology execution (#1641 S3a).
//!
//! An execution names a seat reference, never a session id (seats rotate).
//! It is stored in the execution's `input_json` under the reserved key
//! [`ON_CALL_INPUT_KEY`]; the executor strips that key from node inputs.
//! Resolution is re-derived from rows every time it is asked, so a manager
//! that comes back or is replaced is seen at the next tick.

use rsi_common::topology_agent::ON_CALL_INPUT_KEY;
use rsi_common::types::TopologyOnCallSeat;
use serde_json::Value;
use uuid::Uuid;

use crate::error::{DaemonError, Result};
use crate::store::Store;

/// Sessions of topology nodes that wait on an answer from the on-call
/// manager, kept by the executor (#1641 S3c). The stall detector has no store
/// handle, so it asks this registry instead: the wait is deliberate and its
/// own wall clock is paused, so the idle-session timer must not interrupt it.
/// Filled and cleared by `Executor::account_node_wait` on every tick, so a
/// restart re-learns it within one tick.
static ANSWER_WAITS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<Uuid>>> =
    std::sync::OnceLock::new();

fn answer_waits() -> std::sync::MutexGuard<'static, std::collections::HashSet<Uuid>> {
    ANSWER_WAITS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Record whether the node session `session` waits on an answer right now.
pub(crate) fn note_answer_wait(session: Uuid, waiting: bool) {
    let mut waits = answer_waits();
    if waiting {
        waits.insert(session);
    } else {
        waits.remove(&session);
    }
}

/// Whether `session` is a topology node waiting on its on-call manager.
pub(crate) fn awaits_on_call_answer(session: Uuid) -> bool {
    answer_waits().contains(&session)
}

/// The execution has no project, so no manager can cover it.
pub(crate) const NO_PROJECT: &str = "no_project";
/// The project has no live manager and no live covering portfolio seat.
pub(crate) const NO_LIVE_PROJECT_MANAGER: &str = "no_live_project_manager";
/// The named portfolio node does not cover the project or has no live seat.
pub(crate) const PORTFOLIO_SEAT_NOT_LIVE: &str = "portfolio_seat_not_live";

/// What the named seat resolves to right now.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum OnCall {
    /// A live seat can answer. `seat` is the seat that was resolved (a
    /// portfolio node when the project manager is not live and a covering
    /// portfolio seat answers instead).
    Live {
        seat: TopologyOnCallSeat,
        session_id: Uuid,
    },
    /// Nobody can answer; `reason` is the typed wait reason.
    Unavailable { reason: &'static str },
}

/// The seat an execution input names; the project manager when none does (or
/// when the stored value is unreadable, which the entry points refuse).
pub(crate) fn seat_of(input: Option<&Value>) -> TopologyOnCallSeat {
    input
        .and_then(|input| input.get(ON_CALL_INPUT_KEY))
        .and_then(|seat| serde_json::from_value(seat.clone()).ok())
        .unwrap_or_default()
}

/// The execution input that carries `seat`. `None` leaves `inputs` as given;
/// a seat needs an object (or null) input and may not collide with a caller
/// key.
///
/// # Errors
/// `InvalidParam` when `inputs` is not an object or already uses the reserved
/// key.
pub(crate) fn embed(inputs: Value, seat: Option<&TopologyOnCallSeat>) -> Result<Option<Value>> {
    if inputs
        .as_object()
        .is_some_and(|inputs| inputs.contains_key(ON_CALL_INPUT_KEY))
    {
        return Err(DaemonError::InvalidParam(format!(
            "inputs may not use the reserved key {ON_CALL_INPUT_KEY}; pass on_call instead"
        )));
    }
    let Some(seat) = seat else {
        return Ok((!inputs.is_null()).then_some(inputs));
    };
    if matches!(seat, TopologyOnCallSeat::Portfolio { node_id } if node_id.is_nil()) {
        return Err(DaemonError::InvalidParam(
            "on_call.node_id must name a portfolio node".into(),
        ));
    }
    let mut object = match inputs {
        Value::Null => serde_json::Map::new(),
        Value::Object(object) => object,
        _ => {
            return Err(DaemonError::InvalidParam(
                "inputs must be an object when on_call is set".into(),
            ));
        }
    };
    object.insert(ON_CALL_INPUT_KEY.into(), serde_json::to_value(seat)?);
    Ok(Some(Value::Object(object)))
}

/// Resolve `seat` for an execution of `project`.
///
/// 1. `project_manager`: the project's manager when it can act, otherwise the
///    nearest live covering portfolio seat (PM ruling P1).
/// 2. `portfolio`: exactly that node's seat, when it covers the project and
///    its seat is live. A named seat never falls back.
///
/// # Errors
/// A store read failure.
pub(crate) fn resolve(
    store: &Store,
    project: Option<Uuid>,
    seat: &TopologyOnCallSeat,
) -> Result<OnCall> {
    let Some(project) = project else {
        return Ok(OnCall::Unavailable { reason: NO_PROJECT });
    };
    match seat {
        TopologyOnCallSeat::ProjectManager => {
            if let Some(session_id) = store.on_call_project_manager_seat(project)? {
                return Ok(OnCall::Live {
                    seat: TopologyOnCallSeat::ProjectManager,
                    session_id,
                });
            }
            Ok(store.on_call_portfolio_seats(project)?.first().map_or(
                OnCall::Unavailable {
                    reason: NO_LIVE_PROJECT_MANAGER,
                },
                |&(node_id, session_id)| OnCall::Live {
                    seat: TopologyOnCallSeat::Portfolio { node_id },
                    session_id,
                },
            ))
        }
        TopologyOnCallSeat::Portfolio { node_id } => Ok(store
            .on_call_portfolio_seats(project)?
            .into_iter()
            .find(|&(node, _)| node == *node_id)
            .map_or(
                OnCall::Unavailable {
                    reason: PORTFOLIO_SEAT_NOT_LIVE,
                },
                |(node_id, session_id)| OnCall::Live {
                    seat: TopologyOnCallSeat::Portfolio { node_id },
                    session_id,
                },
            )),
    }
}

/// Refuse a named portfolio node that is not an active node covering
/// `project`. Liveness is not checked here: a seat that is down is a visible
/// wait at run time, not a refusal at start.
///
/// # Errors
/// `InvalidParam` for a node that does not cover the project, or a store
/// read failure.
pub(crate) fn require_covering(
    store: &Store,
    project: Option<Uuid>,
    seat: Option<&TopologyOnCallSeat>,
) -> Result<()> {
    let Some(TopologyOnCallSeat::Portfolio { node_id }) = seat else {
        return Ok(());
    };
    let covers = match project {
        Some(project) => store
            .portfolio_chain_for_project(project)?
            .iter()
            .any(|coverage| coverage.node_id == *node_id),
        None => false,
    };
    if covers {
        Ok(())
    } else {
        Err(DaemonError::InvalidParam(
            "on_call names a portfolio node that does not cover this project".into(),
        ))
    }
}
