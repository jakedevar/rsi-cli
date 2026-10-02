//! Read-only RSI Remote gateway (ADR D4/D5).
//!
//! A request reaches the daemon only after every layer allows it, in order:
//! root peer credentials on the ingress socket (`main.rs`), the strict request
//! parser and closed route list (`ingress`), fresh `LocalAPI` readiness plus a
//! per-request client `WhoIs` (`gate`), a bound browser session with origin
//! checks (`session`), and the closed first-page read adapter that enforces
//! project scope before dispatch (`reads`). Each layer's denials are tested to
//! cause zero daemon dispatch.

pub mod assets;
pub mod config;
pub mod gate;
pub mod ingress;
pub mod localapi;
pub mod localapi_client;
pub mod reads;
pub mod session;
