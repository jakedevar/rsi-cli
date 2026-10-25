//! Rust Terminal-Bench 2 runner, option B of program #3135 (design #3628). R1 (#3678) is the
//! single-trial oracle core: the reward reader ([`reward`]) and the trial state machine
//! ([`trial`]) over an injected environment. The remote Docker transport (R2, #3679) is
//! [`transport`] (commands, ssh, host and path validation), [`docker`] (the digest-pinned
//! Compose environment) and [`runner`] (preflight and bounded runs), reached by [`cli`]. Record
//! assembly (R3a: the daemon's `record.rs`, with the evaluator identity stamped by [`stamp`]), the
//! Harness client ([`client`], the run-local bench socket) and trajectory ([`trajectory`], ATIF and
//! the raw trace; R4, #3682) and the journal ([`journal`], with the resumable [`lifecycle`] and the evidence manifests of
//! [`artifacts`]; R5, #3683) build on it.
//!
//! The pin, split and leak-audit tooling is #3620's [`crate::tb2`]; this module only reuses its
//! [`crate::tb2::dataset::Task`] and never re-implements any of it.

pub mod artifacts;
pub mod cli;
pub mod client;
pub mod docker;
pub mod journal;
pub mod lifecycle;
pub mod reward;
pub mod runner;
pub mod stamp;
pub mod trajectory;
pub mod transport;
pub mod trial;

#[cfg(all(test, unix))]
mod client_tests;
#[cfg(test)]
mod journal_tests;
#[cfg(test)]
mod remote_tests;
#[cfg(test)]
mod tests;

use rsi_common::bench_runner::{PlannedTrial, TrialIdentity};

use crate::tb2::dataset::Task;

/// A deadline of whole seconds. Harbor scales fractional timeouts; the socket and this runner
/// use `u64`, so a fractional, zero, negative or non-finite value is refused, not truncated.
///
/// # Errors
/// The value is not a positive whole number of seconds.
pub fn whole_seconds(label: &str, value: f64) -> Result<u64, String> {
    if !value.is_finite() || value <= 0.0 || value.fract() != 0.0 || value >= 1.8e19 {
        return Err(format!(
            "{label} {value} is not a positive whole number of seconds"
        ));
    }
    // The checks above make the cast exact.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Ok(value as u64)
}

/// Plan one trial of a pinned task from its #3620 dataset row, using the task's own agent and
/// verifier deadlines (no multiplier, no run-wide maximum).
///
/// # Errors
/// A fractional or non-positive deadline.
pub fn planned_trial(task: &Task, identity: TrialIdentity) -> Result<PlannedTrial, String> {
    Ok(PlannedTrial {
        identity,
        image: task.image.clone(),
        image_digest: task.image_digest.clone(),
        cpus: task.cpus,
        memory_mb: task.memory_mb,
        agent_timeout_sec: whole_seconds("agent_timeout_sec", task.agent_timeout_sec)?,
        verifier_timeout_sec: whole_seconds("verifier_timeout_sec", task.verifier_timeout_sec)?,
        solution_env: std::collections::BTreeMap::new(),
        verifier_env: std::collections::BTreeMap::new(),
        agent_user: None,
        verifier_user: None,
    })
}
