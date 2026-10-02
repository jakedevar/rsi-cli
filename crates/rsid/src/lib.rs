//! Provider execution authority is sealed inside `rsid`.
//!
//! External crates cannot name either the one-use capability or the provider
//! extension trait, so they cannot implement a raw-send provider that accepts
//! and drops model-control authority.
//!
//! ```compile_fail
//! use rsid::model_control::ModelExecutionCapability;
//! ```
//!
//! ```compile_fail
//! use rsid::session::harness::provider::ApiProvider;
//! ```

pub(crate) mod agent_issue_validation;
pub mod agy;
/// Daemon-global AppServer control plane (C-P2-15). Deliberately crate-private:
/// no external crate may name a control-plane capability or fenced request.
pub(crate) mod app_server_control;
/// The bounded control worker that commits the plane's in-memory seals to
/// durable storage (C-P2-15). Crate-private for the same reason as the plane.
pub(crate) mod app_server_seal_worker;
pub mod bedrock;
pub mod bus;
pub mod claude;
#[doc(hidden)]
pub mod closure_kernel;
pub mod cloud_spend;
pub mod codegraph;
pub mod codex;
pub mod codex_app_server;
pub mod command_frontmatter;
pub mod config;
pub mod dialectic;
pub mod dotenv;
pub mod dreamer;
pub use rsid_core::{error, path_safety};
pub mod governor;
pub mod graph_exec;
pub(crate) use store::daemon_restart_persistence;
pub(crate) mod idea_control;
pub mod instance_guard;
pub mod integration;
pub mod issue_tracker;
pub mod mcp_config;
pub mod memory;
pub mod model_control;
pub mod monitor;
pub mod observation;
pub mod ollama_client;
pub mod openai;
pub mod openrouter;
pub mod pioneer;
pub(crate) mod process_control;
pub mod shared_target_prune;
// Foundation module is intentionally unwired until the real scope lifecycle
// gate passes and the provider call sites have separate ownership.
#[allow(dead_code)]
pub mod log_rotation;
pub mod process_memory;
pub(crate) mod process_scope;
pub mod profiling;
pub(crate) mod program_run_control;
pub(crate) mod program_run_dispatch;
pub mod project_cache;
pub mod project_workflow;
pub mod prompt_compile;
pub mod provider;
pub(crate) mod provider_capabilities;
pub mod provider_capability_validation;
pub mod provider_cli;
pub(crate) mod provider_exhaustion;
pub mod queue;
pub mod reconciliation;
pub mod recursive_dag;
// Bounded source projections for operator-only Remote reads.
pub mod agent_jobs;
pub mod daemon_info;
pub mod deploy;
pub mod deploy_drain;
pub mod provider_status;
#[allow(dead_code)]
pub(crate) mod remote_read;
pub mod rolling_queue;
pub mod rpc;
pub mod sandbox;
#[allow(clippy::redundant_pub_crate)]
pub(crate) mod satellite;
pub mod scheduler;
pub mod session;
pub mod stall_classifier;
pub mod stall_detector;
pub mod store;
pub(crate) mod store_support;
pub mod store_worker;
pub mod terminal_cause;
pub(crate) mod terminal_output;
pub mod tool_registry;
pub(crate) mod topology;
pub mod turn_controller;
pub mod vault;
pub mod watch_service;
pub mod watchdog;

/// Start the hub's bounded satellite observation loop after the store is ready.
/// Peers remain inert until the operator enables a paired peer and a link.
pub fn start_satellite_hub_poller(
    store: std::sync::Arc<tokio::sync::Mutex<store::Store>>,
    runtime_config: std::sync::Arc<config::RuntimeConfig>,
) {
    tokio::spawn(satellite::hub::run_hub_poller(
        store,
        runtime_config,
        rsi_common::identity::data_path("satellites", "satellites"),
    ));
}
