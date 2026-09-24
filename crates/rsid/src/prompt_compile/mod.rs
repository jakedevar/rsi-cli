//! Daemon-side prompt compilation pipeline.
//!
//! Hosts the `CompileEngine` that powers the `CompilePrompt` RPC: streaming
//! Ollama generation, post-processing (think-strip, contract parse, layer
//! validation), content-hash LRU caching, and supersede-by-caller cancellation.

pub mod engine;
pub mod post_process;
pub mod system_prompt;

pub use engine::{CallerId, CompileEngine};
