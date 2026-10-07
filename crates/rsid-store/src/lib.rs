//! The `rsid` daemon's durable state and the modules that sit next to it:
//! the SQLite `store` (schema migrations, every table accessor), runtime
//! `config`, the event `bus`, `model_control`, the sandbox custody layer, the
//! credential `vault` and `bedrock` auth. Issue #1021 S4: moved out of `rsid`
//! verbatim; `rsid` re-exports every module at its old path.
//!
//! Nothing here may depend on `rsid` (session, rpc, provider, scheduler, ...).
//!
//! # Authority boundary
//!
//! Capabilities, controller grants and the internal transfer actor were
//! `pub(crate)` in `rsid`; here the entry points `rsid` calls are `pub`. The
//! boundary is that `rsid` is this crate's only dependent (it is unpublished,
//! and `crates/rsid/tests/store_crate_seal.rs` fails the build on a second
//! dependent or on a non-dev edge that enables `test-seam`), that `rsid`
//! re-exports the authority items crate-private, and that the unforgeable types
//! keep private fields:
//!
//! ```compile_fail
//! let _ = rsid_store::model_control::ModelExecutionCapability {};
//! ```
//!
//! ```compile_fail
//! let _ = rsid_store::idea_control::BoundControllerWriteAuthority {};
//! ```

pub mod bedrock;
pub mod bedrock_setup;
pub mod bus;
pub mod config;
pub use rsid_core::{
    error, path_safety, process_control, provider_exhaustion, terminal_cause, terminal_output,
};
pub mod idea_control;
pub mod model_control;
pub mod sandbox;
pub mod store;
pub mod store_support;
#[cfg(any(test, feature = "test-seam"))]
pub mod test_support;
pub mod vault;

pub use store::daemon_restart_persistence;
