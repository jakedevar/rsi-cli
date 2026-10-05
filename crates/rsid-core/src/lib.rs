//! Leaf types shared by the `rsid` daemon crates: the daemon error type, the
//! working-directory path-safety helpers and the process-control, terminal
//! cause/output and provider-exhaustion primitives. No dependency on `rsid`.

pub mod error;
pub mod path_safety;
pub mod process_control;
pub mod provider_exhaustion;
pub mod terminal_cause;
pub mod terminal_output;
