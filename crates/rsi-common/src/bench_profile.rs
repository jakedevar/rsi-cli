//! #3177 (program #3135 M3 S3): the bench launch profile.
//!
//! A benchmark session runs one pinned, recorded harness configuration: its own
//! base prompt (the task-container prompt, not the git-worktree one), a closed
//! tool allowlist, per-launch loop knobs and a Harness-only route. This module
//! holds the data. Only daemon code builds a [`BenchLaunchProfile`]:
//! `LaunchSessionParams` and every agent verb have no field for it, so an agent
//! can neither select the bench prompt nor loosen the allowlist.
//!
//! The behaviour lives in `rsid`: prompt composition (`harness::base_prompt`),
//! the route assertion (`session::provider_spawn`), the post-run route audit
//! (`session::bench_route`) and the build digest (`harness::build_digest`).

use serde::{Deserialize, Serialize};

/// Typed refusal prefix of a bench launch that cannot be proven Harness-only
/// (`bench_route_required:<reason>`).
pub const BENCH_ROUTE_REQUIRED: &str = "bench_route_required";

/// Typed refusal prefix of a malformed bench launch
/// (`bench_profile_invalid:<reason>`).
pub const BENCH_PROFILE_INVALID: &str = "bench_profile_invalid";

/// The `invalid_route` verdict a post-run audit gives a trial whose session
/// did not run Harness-only.
pub const BENCH_INVALID_ROUTE: &str = "invalid_route";

/// The only `model_invocations.purpose` values a Harness-only bench session may
/// write: the launch row (the first model call settles the launch's own
/// admission, `session.launch.fresh`; a route fallback is caught by its own
/// events and the launch preflight, not by this row), the model turn and the
/// Harness compaction. Anything else, such as `session.continue.resume`, is a
/// session that did not run Harness-only.
pub const BENCH_ALLOWED_INVOCATION_PURPOSES: [&str; 3] = [
    "session.launch.fresh",
    "session.harness.turn",
    "session.harness.compaction",
];

/// `provider_health.kind` of the System row recording a Harness-to-Codex route
/// fallback (`route_fallback` stream event).
pub const ROUTE_FALLBACK_KIND: &str = "route_fallback";
/// `provider_health.kind` of the System row recording that a credential was
/// exposed to a CLI (`credential_cli_exposure` stream event).
pub const CREDENTIAL_CLI_EXPOSURE_KIND: &str = "credential_cli_exposure";
/// `provider_health.kind` of the System row carrying a bench session's
/// `harness_build_digest`.
pub const BENCH_BUILD_DIGEST_KIND: &str = "bench_build_digest";

/// The route a bench launch requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequiredRoute {
    /// RSI's own Harness loop: no CLI process, no fallback to one.
    #[default]
    Harness,
}

impl RequiredRoute {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Harness => "harness",
        }
    }
}

/// Per-launch loop knobs. Each `Some` overrides the daemon-global operator
/// setting for that one session only; no global setting is changed. A session
/// that sets none runs on the globals, exactly as before.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoopKnobs {
    /// Per-turn model-call cap (`harness_max_iterations_per_turn` for others).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_iterations: Option<u32>,
    /// Absolute prompt-token budget at which the Harness compacts, regardless
    /// of route (`openrouter_context_budget_tokens` for others).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_budget_tokens: Option<u64>,
    /// #4028: the output-token ceiling of the call that recovers a length stop
    /// (a reasoning model that spent its whole per-call cap and made no tool
    /// call). `None` is the loop's own default (32,768).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_max_output_tokens: Option<u32>,
    /// Context rotation never runs for this session
    /// (`context_rotation_enabled` for others).
    #[serde(default)]
    pub rotation_off: bool,
}

impl LoopKnobs {
    /// The most iterations a knob may ask for.
    pub const MAX_ITERATIONS_LIMIT: u32 = 10_000;
    /// The smallest and largest recovery ceiling a knob may ask for.
    pub const RECOVERY_MAX_OUTPUT_TOKENS_RANGE: std::ops::RangeInclusive<u32> = 1_024..=262_144;

    /// # Errors
    /// A zero or oversized iteration cap, or a zero compaction budget.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self
            .max_iterations
            .is_some_and(|cap| cap == 0 || cap > Self::MAX_ITERATIONS_LIMIT)
        {
            return Err("max_iterations_out_of_range");
        }
        if self.compaction_budget_tokens == Some(0) {
            return Err("compaction_budget_zero");
        }
        if self
            .recovery_max_output_tokens
            .is_some_and(|cap| !Self::RECOVERY_MAX_OUTPUT_TOKENS_RANGE.contains(&cap))
        {
            return Err("recovery_max_output_tokens_out_of_range");
        }
        Ok(())
    }
}

/// The daemon-set bench launch profile.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchLaunchProfile {
    /// The route the launch must take; a launch that cannot is refused before
    /// any process starts, and the finished session is audited against it.
    pub require_route: RequiredRoute,
    /// #3178: the per-task USD cap of the run this session serves, in millionths
    /// of a dollar. Before each model call the session's own spend plus an upper
    /// bound on the next call must stay below it; reaching it ends the session with
    /// `bench_task_budget_exhausted`. `None` for a bench session outside a run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_cap_micro_usd: Option<u64>,
    /// #3178: where the session's live, untruncated trace is written (one JSON
    /// object per line). `None` writes none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_path: Option<String>,
    /// #3191: the run overlay this session runs under (a Meta-Harness candidate):
    /// the editable prompt body and the narrowed tool policy. Its loop knobs are
    /// applied to [`LoopKnobs`] by the launcher. `None` is the unmodified bench
    /// profile, byte for byte.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlay: Option<crate::bench_overlay::BenchOverlay>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knobs_default_to_no_override_and_validate_their_ranges() {
        let none = LoopKnobs::default();
        assert_eq!(none.validate(), Ok(()));
        assert_eq!(serde_json::to_value(none).unwrap()["rotation_off"], false);
        for bad in [0, LoopKnobs::MAX_ITERATIONS_LIMIT + 1] {
            let knobs = LoopKnobs {
                max_iterations: Some(bad),
                ..LoopKnobs::default()
            };
            assert_eq!(knobs.validate(), Err("max_iterations_out_of_range"));
        }
        let zero_budget = LoopKnobs {
            compaction_budget_tokens: Some(0),
            ..LoopKnobs::default()
        };
        assert_eq!(zero_budget.validate(), Err("compaction_budget_zero"));
        let ok = LoopKnobs {
            max_iterations: Some(80),
            compaction_budget_tokens: Some(200_000),
            recovery_max_output_tokens: Some(65_536),
            rotation_off: true,
        };
        assert_eq!(ok.validate(), Ok(()));
        for bad in [0, 1_023, 262_145] {
            let knobs = LoopKnobs {
                recovery_max_output_tokens: Some(bad),
                ..LoopKnobs::default()
            };
            assert_eq!(
                knobs.validate(),
                Err("recovery_max_output_tokens_out_of_range")
            );
        }
    }

    #[test]
    fn the_profile_requires_the_harness_route_and_refuses_unknown_fields() {
        assert_eq!(
            BenchLaunchProfile::default().require_route,
            RequiredRoute::Harness
        );
        assert_eq!(RequiredRoute::Harness.as_str(), "harness");
        assert!(
            serde_json::from_value::<BenchLaunchProfile>(
                serde_json::json!({"require_route": "harness", "extra": 1})
            )
            .is_err()
        );
    }
}
