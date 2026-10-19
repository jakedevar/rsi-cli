//! Remote V1 read facade. The bounded SQLite source reads, cursor signer and
//! projections live in the `rsid-remote-read` leaf crate (#2410), re-exported
//! here at their old paths. This module keeps the daemon-side authority: the RPC
//! owner supplies a read transaction, project authorization, runtime
//! observations and transport lifetime guards. No function here exposes an agent
//! control method.

mod decision_targets;
mod manager_v2;
mod owner;
mod permit;
mod rpc_owner;
mod store_budget;

// The leaf takes a borrowed `Connection` and value observations only; Store
// locking, admission, runtime permits and the per-boot signer stay below.
#[allow(unused_imports)] // The RPC owner and session runtime consume these seams.
pub use rsid_remote_read::*;

pub(crate) use manager_v2::{parse_send, send_message};
#[allow(unused_imports)]
// The RPC owner connects trusted scope to this worker after dispatch admission.
pub use owner::{
    InitialSavedDecisionsSources, ResumedSavedDecisionsSources, spawn_decisions_read,
    spawn_history_read, spawn_info_read, spawn_initial_saved_decisions_page,
    spawn_initial_saved_decisions_sources, spawn_projects_read, spawn_resumed_saved_decisions_page,
    spawn_resumed_saved_decisions_sources, spawn_runtime_only_decisions_page,
    spawn_selected_session_read, spawn_sessions_read,
};
#[allow(unused_imports)] // The RPC owner carries completed work through its socket boundary.
pub use permit::{RemoteReadBudget, RemoteReadCompleted, RemoteReadLimiter};
pub(crate) use rpc_owner::{is_operator_read_method, send_operator_read};
#[allow(unused_imports)]
// The RPC owner supplies an acquired Store lock in its blocking worker.
pub use store_budget::with_store_budget;
