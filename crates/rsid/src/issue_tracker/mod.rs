//! Issue tracker polling and automatic session dispatch.
//!
//! Polls external issue trackers (Linear) and dispatches sessions
//! for eligible issues via SessionManager::launch_session().

pub mod eligibility;
pub mod linear;
pub mod local;
pub mod manager;
pub mod poller;
pub mod tracker;
pub mod types;
