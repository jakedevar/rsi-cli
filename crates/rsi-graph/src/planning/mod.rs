//! Pure topology planning shared by the daemon's legacy graph runner and its
//! durable topology executor (#2415, #2308 cut 2).
//!
//! Nothing here touches the store, sessions or custody: every item is a
//! function of a workflow snapshot (plus a caller-supplied loop ceiling). The
//! daemon keeps validation, execution and authority, and maps this module's
//! `String` errors to its own error type at the boundary.

pub mod graph;
pub mod metadata;
pub mod steps;
