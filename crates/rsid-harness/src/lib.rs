//! Pure conversation values, transformations and tools for the daemon's direct-API harness.
//!
//! Runtime ownership, provider admission and every tool that needs daemon authority
//! (process custody, the store, agent controls, memory) remain in `rsid`.
//! This crate has no dependency on the daemon or its sealed store/core crates.

pub mod agent_loop;
pub mod arguments;
pub mod bedrock_stream;
pub mod compaction;
pub mod errors;
pub mod idle;
pub mod normalize;
pub mod paths;
pub mod ports;
pub mod providers;
pub mod retry;
pub mod sse;
pub mod stream;
pub mod tools;
pub mod types;

#[cfg(test)]
mod test_support;
