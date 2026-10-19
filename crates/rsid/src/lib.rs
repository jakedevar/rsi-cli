//! Provider execution authority is sealed inside the daemon crates.
//!
//! External crates cannot name the one-use capabilities, the admitted command
//! and request wrappers, the admission entry points or the provider extension
//! trait, so they cannot implement a raw-send provider that accepts and drops
//! model-control authority. The capability types live in `rsid-store`, whose
//! only permitted dependent is this crate (`tests/store_crate_seal.rs` enforces
//! it, and `test-seam` never reaches a release build); the facade below keeps
//! every item that was crate-private before the split crate-private here.
//!
//! ```compile_fail
//! use rsid::model_control::ModelExecutionCapability;
//! ```
//!
//! ```compile_fail
//! use rsid::model_control::CliExecutionCapability;
//! ```
//!
//! ```compile_fail
//! use rsid::model_control::AdmittedCliCommand;
//! ```
//!
//! ```compile_fail
//! use rsid::model_control::AdmittedHttpRequest;
//! ```
//!
//! ```compile_fail
//! use rsid::model_control::admit_launch_invocation;
//! ```
//!
//! ```compile_fail
//! use rsid::idea_control::BoundControllerWriteAuthority;
//! ```
//!
//! ```compile_fail
//! use rsid::session::harness::provider::ApiProvider;
//! ```

// A test shard (`--features test-shard-*`) compiles only that shard's tests, so
// every helper, import and fixture used solely by another shard's tests looks
// unused by construction. Those two lints are silenced for the shard-mode test
// unit only; the ordinary lib-test build (the warning-baseline gate) keeps both
// lints, so a helper no shard uses is still reported there (#2666).
#![cfg_attr(
    all(test, feature = "test-shard-mode"),
    allow(dead_code, unused_imports)
)]

pub(crate) mod agent_issue_validation;
pub mod agy;
/// Daemon-global AppServer control plane (C-P2-15). Deliberately crate-private:
/// no external crate may name a control-plane capability or fenced request.
pub(crate) mod app_server_control;
/// The bounded control worker that commits the plane's in-memory seals to
/// durable storage (C-P2-15). Crate-private for the same reason as the plane.
pub(crate) mod app_server_seal_worker;
pub use rsid_store::{bedrock, bedrock_setup, bus};
pub mod claude;
#[doc(hidden)]
pub mod closure_kernel;
pub mod cloud_spend;
pub mod codegraph;
pub mod codex;
pub mod codex_app_server;
pub(crate) mod codex_live_catalog;
pub mod codex_rate_limits;
pub mod command_frontmatter;
pub use rsid_store::config;
pub mod dialectic;
pub mod dotenv;
pub mod dreamer;
pub(crate) use rsid_core::process_control;
pub use rsid_core::{
    error, offline, path_safety, provider_exhaustion, terminal_cause, terminal_output,
};
pub mod governor;
pub mod graph_exec;
pub(crate) use rsid_store::idea_control;
pub mod agent_scratch_reclaim;
pub mod instance_guard;
pub mod integration;
pub mod issue_tracker;
pub mod lander_warm_target;
pub mod mcp_config;
pub mod memory;
/// `rsid_store::model_control` as seen from outside the daemon. Every item that
/// was crate-private before the `rsid-store` split (the capabilities, the
/// admitted command and request wrappers, the admission entry points) stays
/// crate-private to `rsid`: an explicit import shadows the glob.
pub mod model_control {
    pub use rsid_store::model_control::*;
    // The shadowing is the point: these items are public in `rsid-store` and stay private here.
    #[allow(hidden_glob_reexports)]
    pub(crate) use rsid_store::model_control::{
        AppServerDispatchOutcome, CapacityAdmissionDecision, CliExecutionCapability,
        ModelExecutionCapability, admit_capacity_invocation, admit_launch_invocation,
        classify_model_tier, resume_unexecuted_agent_successor_admission,
        resume_unexecuted_capacity_delivery_admission, resume_unexecuted_closure_launch_admission,
    };
}
mod local_build;
pub mod monitor;
pub mod observation;
pub mod ollama_client;
pub mod openai;
pub mod openrouter;
pub mod operator_config;
pub mod pioneer;
pub mod qa_target_trim;
pub mod release_target;
pub mod shared_target_prune;
pub mod shared_target_reclaim;
pub mod stray_target_reclaim;
pub mod structured_handoff;
pub mod target_pool;
// Foundation module is intentionally unwired until the real scope lifecycle
// gate passes and the provider call sites have separate ownership.
pub mod api_route_selection;
#[allow(dead_code)]
pub mod log_rotation;
pub mod process_memory;
pub(crate) mod process_scope;
pub mod profiling;
pub(crate) mod program_run_control;
pub(crate) mod program_run_dispatch;
pub mod project_cache;
/// Read-only, project-root-scoped file reads behind the operator-only
/// project-file RPCs (hub #3012).
pub(crate) mod project_files;
/// Daemon start: the project-file trash's one bounded 7-day prune pass
/// (local #120).
pub use project_files::write::prune_project_file_trash_at_start;
pub mod project_workflow;
pub mod prompt_compile;
/// The operator prompt-file store `~/.rsi/prompts` behind the operator-only
/// prompt-file RPCs (hub #3040).
pub(crate) mod prompt_files;
pub mod provider;
pub(crate) mod provider_capabilities;
pub mod provider_capability_validation;
pub mod provider_cli;
pub mod provider_contract;
pub mod provider_launch_overrides;
pub mod provider_login;
pub mod queue;
pub mod reconciliation;
pub mod recursive_dag;
pub mod remote_control;
pub mod session_start_preamble;
pub mod startup_clock;
pub mod supervisor_shutdown;
// Bounded source projections for operator-only Remote reads.
pub mod agent_jobs;
pub mod build_budget;
pub mod cold_archive_lifecycle; // #2384 review (#205): the fence between pass threads and a restart.
pub mod cold_compact; // #2384 S4: the compaction helper at a supervised restart.
pub mod cpu_andon;
pub mod daemon_info;
pub mod deploy;
pub mod deploy_drain;
pub mod deploy_operator;
pub mod deploy_staging_prune;
pub mod friction;
pub mod health_anomaly; // #2747: the `when.health_anomaly` evaluator.
pub mod host_load;
pub mod host_memory; // #2937: macOS memory source and the daemon-side build capacity gate.
pub mod host_pressure; // #2562: the shared thermal + disk-pressure admission predicate.
pub mod manager_wake; // #2494: when the manager coordinator runs a pass.
pub mod provider_status;
pub(crate) mod provider_stream_health;
#[allow(dead_code)]
pub(crate) mod remote_read;
pub mod rolling_queue;
pub mod rpc;
pub use rsid_store::sandbox;
#[allow(clippy::redundant_pub_crate)]
pub(crate) mod satellite;
pub mod scheduler;
pub mod session;
pub(crate) mod skill_delivery;
pub mod stall_classifier;
pub mod stall_detector;
pub use rsid_store::store;
pub(crate) use rsid_store::store_support;
pub mod launch_breadcrumbs;
pub(crate) mod store_lock;
#[cfg(test)]
mod store_split_tests;
pub mod store_worker;
#[cfg(test)]
pub(crate) mod test_support;
pub mod thread_stacks;
pub mod tool_registry;
pub(crate) mod topology;
pub mod turn_controller;
pub use rsid_store::vault;
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
