//! RSI-006 evaluation/replay harness library.
//!
//! Loads a frozen ticket corpus, drives a daemon through `LaunchSession` for
//! each ticket, polls every 2s until terminal status, and aggregates the
//! per-session telemetry into a baseline snapshot. The driver is serial; LLM
//! wall time dominates so per-replay overhead is negligible.
//!
//! Phases 4 lands the corpus loader, socket guard, and polling driver. Phase 5
//! adds the metrics collector, baseline I/O, and regression gate.

pub mod baseline;
pub mod corpus;
pub mod driver;
pub mod errors;
pub mod gate;
pub mod metrics;
pub mod recursive_dag;
pub mod report;
pub mod socket_guard;
