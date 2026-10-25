//! Leaf types shared by the `rsid` daemon crates: the daemon error type, the
//! working-directory path-safety helpers and the process-control, terminal
//! cause/output and provider-exhaustion primitives. No dependency on `rsid`.

pub mod build_slice;
pub mod error;
pub mod health_thresholds;
#[cfg(target_os = "macos")]
pub mod libproc_macos;
pub mod offline;
pub mod pass_cost;
pub mod paste_retention_setting;
pub mod path_safety;
pub mod private_state;
pub mod process_control;
pub mod provider_exhaustion;
pub mod qa_target_cap;
pub mod store_lock;
pub mod terminal_cause;
pub mod terminal_output;
pub mod thermal_guard;
pub mod tool_result_cap;
pub mod worker_cpu_quota;
