//! Background task queue for async memory operations.
//!
//! Provides a SQLite-backed queue with work unit grouping, token-threshold
//! batching, optimistic locking, and stale claim cleanup. Tasks are dispatched
//! to `TaskProcessor` implementations.

pub mod processor;
pub mod types;
pub mod worker;
