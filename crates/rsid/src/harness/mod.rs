//! Self-contained agent harness for direct LLM API calls.
//!
//! Bridges provider APIs, tool execution, context compaction, and the
//! conversation loop. Emits `StreamEvent`s compatible with the existing
//! `monitor_session()` pipeline. Zero new dependencies beyond reqwest/tokio/serde.

pub mod agent_loop;
pub mod api_key;
pub mod client;
pub mod compatible_table;
pub mod provider;
pub mod providers;
pub mod sse;
pub mod tools;
pub mod types;
