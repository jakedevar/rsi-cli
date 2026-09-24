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
//! use rsid::harness::provider::ApiProvider;
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
pub mod codegraph;
pub mod codex;
pub mod codex_app_server;
pub mod command_frontmatter;
pub mod config;
pub(crate) mod daemon_restart_persistence;
pub mod dialectic;
pub mod dotenv;
pub mod dreamer;
pub mod error;
pub mod graph_exec;
pub mod harness;
pub(crate) mod idea_control;
pub mod instance_guard;
pub mod integration;
pub mod issue_tracker;
pub mod memory;
pub mod model_control;
pub mod monitor;
pub mod observation;
pub mod ollama_client;
pub mod openai;
pub mod openrouter;
pub mod path_safety;
pub mod pioneer;
pub(crate) mod process_control;
pub mod profiling;
pub(crate) mod program_run_control;
pub(crate) mod program_run_dispatch;
pub mod project_cache;
pub mod project_workflow;
pub mod prompt_compile;
pub mod provider;
pub(crate) mod provider_capabilities;
pub mod provider_capability_validation;
pub mod queue;
pub mod reconciliation;
pub mod recursive_dag;
pub mod rpc;
pub mod sandbox;
pub mod scheduler;
pub mod session;
pub mod stall_classifier;
pub mod stall_detector;
pub mod store;
pub mod store_worker;
pub(crate) mod terminal_output;
pub mod tool_registry;
pub(crate) mod topology;
pub mod turn_controller;
pub mod watch_service;
pub mod watchdog;
