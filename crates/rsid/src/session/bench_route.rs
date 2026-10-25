//! #3177 (program #3135 M3 S3): the post-run route audit of a bench session.
//!
//! The launch refuses a bench session that cannot run Harness-only
//! (`provider_spawn::bench_route_preflight`). This audit is the second proof,
//! taken from durable records after the session ends: every
//! `model_invocations` row of the session has purpose `session.launch.fresh` (the
//! first model call settles the launch's own admission), `session.harness.turn`
//! or `session.harness.compaction`, and the transcript holds no `route_fallback`
//! or `credential_cli_exposure` row (and no `api_route_selected` row naming an
//! engine other than the Harness). Anything else makes the task `invalid_route`
//! (S4 #3178 records the verdict and fails the run on any such task).
//!
//! The bench task also reads the session's `harness_build_digest` from here.

// Called by the bench task (`bench::task`, #3178); `task_state` is for run records (S5).
#![allow(dead_code)]

use crate::error::Result;
use crate::store::Store;
use crate::store::bench_route_audit::InvocationRoute;
use rsi_common::bench_profile::{
    BENCH_ALLOWED_INVOCATION_PURPOSES, BENCH_BUILD_DIGEST_KIND, CREDENTIAL_CLI_EXPOSURE_KIND,
    ROUTE_FALLBACK_KIND,
};
use rsi_common::types::{ConversationEvent, PROVIDER_HEALTH_METADATA_KEY};
use uuid::Uuid;

/// The audit verdict. `violations` is empty exactly when `valid`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RouteVerdict {
    pub valid: bool,
    /// One short code per finding: `purpose:<purpose>`, `event:<kind>` or
    /// `engine:<engine>`; sorted and deduplicated.
    pub violations: Vec<String>,
}

impl RouteVerdict {
    /// The task state a bench run records: `None` when valid, else
    /// `invalid_route`.
    #[must_use]
    pub(crate) fn task_state(&self) -> Option<&'static str> {
        (!self.valid).then_some(rsi_common::bench_profile::BENCH_INVALID_ROUTE)
    }
}

fn provider_health_of(event: &ConversationEvent) -> Option<&serde_json::Value> {
    event
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get(PROVIDER_HEALTH_METADATA_KEY))
}

/// The one purpose besides the Harness ones a bench session may carry (#3935): the resume that
/// writes a handoff (`resume_for_handoff_write`), which runs on the route the trial launched on.
const RESUME_PURPOSE: &str = "session.continue.resume";
const LAUNCH_PURPOSE: &str = "session.launch.fresh";

/// Judge a session from its invocations' routes and its transcript rows. A
/// `session.continue.resume` invocation is allowed only when its provider, model and backend
/// equal the route of the session's own `session.launch.fresh` invocation, the trial's pinned
/// route; every other purpose outside the allow-list still fails the session.
pub(crate) fn judge_invocation_routes(
    routes: &[InvocationRoute],
    events: &[ConversationEvent],
) -> RouteVerdict {
    let pinned = routes
        .iter()
        .find(|r| r.purpose == LAUNCH_PURPOSE && r.provider.is_some() && r.model.is_some());
    let purposes: Vec<String> = routes
        .iter()
        .filter(|r| {
            !(r.purpose == RESUME_PURPOSE
                && pinned.is_some_and(|p| {
                    p.provider == r.provider && p.model == r.model && p.backend == r.backend
                }))
        })
        .map(|r| r.purpose.clone())
        .collect();
    judge_route(&purposes, events)
}

/// Judge a session from its invocation purposes and its transcript rows.
pub(crate) fn judge_route(purposes: &[String], events: &[ConversationEvent]) -> RouteVerdict {
    let mut violations = std::collections::BTreeSet::new();
    for purpose in purposes {
        if !BENCH_ALLOWED_INVOCATION_PURPOSES.contains(&purpose.as_str()) {
            violations.insert(format!("purpose:{purpose}"));
        }
    }
    for event in events {
        let Some(health) = provider_health_of(event) else {
            continue;
        };
        match health.get("kind").and_then(serde_json::Value::as_str) {
            Some(kind @ (ROUTE_FALLBACK_KIND | CREDENTIAL_CLI_EXPOSURE_KIND)) => {
                violations.insert(format!("event:{kind}"));
            }
            Some(crate::api_route_selection::ROUTE_SELECTED_KIND) => {
                if let Some(engine) = health
                    .get("engine")
                    .and_then(serde_json::Value::as_str)
                    .filter(|engine| *engine != "harness")
                {
                    violations.insert(format!("engine:{engine}"));
                }
            }
            _ => {}
        }
    }
    let violations: Vec<String> = violations.into_iter().collect();
    RouteVerdict {
        valid: violations.is_empty(),
        violations,
    }
}

/// Audit a finished session against the durable store.
///
/// # Errors
/// A store read failure; the caller must not treat that as a pass.
pub(crate) fn audit_session_route(store: &Store, session_id: Uuid) -> Result<RouteVerdict> {
    let routes = store.session_model_invocation_routes(session_id)?;
    let events = store.load_events_since(session_id, None)?;
    Ok(judge_invocation_routes(&routes, &events))
}

/// The `harness_build_digest` a bench session recorded at launch: the digest of
/// the last `bench_build_digest` row, if the session wrote one.
///
/// # Errors
/// A store read failure.
pub(crate) fn read_build_digest(store: &Store, session_id: Uuid) -> Result<Option<String>> {
    Ok(read_build_digest_row(store, session_id)?
        .as_ref()
        .and_then(|health| health.get("harness_build_digest"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string))
}

/// The whole last `bench_build_digest` row a bench session recorded at launch: the digest plus
/// its provenance (`harness_source_digest`, `rsid_build_sha`, `effective_inputs`, #3192), which
/// the bench job copies onto the trial's record row.
///
/// # Errors
/// A store read failure.
pub(crate) fn read_build_digest_row(
    store: &Store,
    session_id: Uuid,
) -> Result<Option<serde_json::Value>> {
    Ok(store
        .load_events_since(session_id, None)?
        .iter()
        .rev()
        .filter_map(provider_health_of)
        .find(|health| {
            health.get("kind").and_then(serde_json::Value::as_str) == Some(BENCH_BUILD_DIGEST_KIND)
        })
        .cloned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::StreamEvent;
    use serde_json::json;

    fn purposes(list: &[&str]) -> Vec<String> {
        list.iter().map(|p| (*p).to_string()).collect()
    }

    /// The rows the monitor persists for a stream event.
    fn rows(events: &[StreamEvent]) -> Vec<ConversationEvent> {
        let session_id = Uuid::new_v4();
        let mut sequence = 0;
        events
            .iter()
            .flat_map(|event| {
                crate::session::SessionManager::convert_recognized_stream_event(
                    event,
                    session_id,
                    &mut sequence,
                )
                .unwrap_or_default()
            })
            .collect()
    }

    fn stream(kind: &str, data: serde_json::Value) -> StreamEvent {
        StreamEvent {
            event_type: kind.into(),
            data,
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn a_harness_only_session_is_valid() {
        let verdict = judge_route(
            &purposes(&["session.harness.compaction", "session.harness.turn"]),
            &[],
        );
        assert!(verdict.valid);
        assert!(verdict.violations.is_empty());
        assert_eq!(verdict.task_state(), None);
        // A session that made no call at all is not a route violation.
        assert!(judge_route(&[], &[]).valid);
        // The route note of a Harness launch is no violation.
        let selected = crate::provider_stream_health::api_route_event(
            Uuid::new_v4(),
            &mut 0,
            &crate::api_route_selection::RouteSelection {
                engine: "harness",
                configured_route: "harness",
                source: "default",
                fallback_cause: None,
            },
        );
        assert!(judge_route(&purposes(&["session.harness.turn"]), &[selected]).valid);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn any_other_purpose_or_a_fallback_event_makes_the_task_invalid_route() {
        let verdict = judge_route(
            &purposes(&["session.harness.turn", "session.launch_fresh"]),
            &[],
        );
        assert!(!verdict.valid);
        assert_eq!(verdict.violations, ["purpose:session.launch_fresh"]);
        assert_eq!(verdict.task_state(), Some("invalid_route"));

        let fallback = rows(&[
            stream(
                "route_fallback",
                json!({"from":"harness","to":"codex_cli","reason":"missing_credential"}),
            ),
            stream("credential_cli_exposure", json!({"reason":"fallback"})),
        ]);
        assert_eq!(fallback.len(), 2, "both stream events are persisted");
        let verdict = judge_route(&purposes(&["session.harness.turn"]), &fallback);
        assert_eq!(
            verdict.violations,
            ["event:credential_cli_exposure", "event:route_fallback"]
        );
        assert_eq!(verdict.task_state(), Some("invalid_route"));

        // A route note that names the Codex CLI is a violation on its own.
        let selected = crate::provider_stream_health::api_route_event(
            Uuid::new_v4(),
            &mut 0,
            &crate::api_route_selection::RouteSelection {
                engine: "codex_cli",
                configured_route: "harness",
                source: "default",
                fallback_cause: Some("missing_credential"),
            },
        );
        let verdict = judge_route(&[], &[selected]);
        assert_eq!(verdict.violations, ["engine:codex_cli"]);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn the_audit_reads_purposes_and_rows_from_the_store() {
        let store = Store::open_in_memory().unwrap();
        let session_id = Uuid::new_v4();
        store
            .insert_session(&rsid_store::test_support::test_session(
                session_id,
                std::path::PathBuf::from("/tmp/bench-route"),
            ))
            .unwrap();
        let insert_invocation = |n: usize, purpose: &str| {
            store
                .insert_invocation_purpose_for_test(session_id, n, purpose)
                .unwrap();
        };
        insert_invocation(0, "session.harness.turn");
        insert_invocation(1, "session.harness.compaction");
        assert!(audit_session_route(&store, session_id).unwrap().valid);
        assert_eq!(read_build_digest(&store, session_id).unwrap(), None);
        assert_eq!(read_build_digest_row(&store, session_id).unwrap(), None);

        // The digest row the launch writes reads back; the latest one wins.
        let mut sequence = 0;
        for digest in ["a".repeat(64), "b".repeat(64)] {
            let digest_rows = crate::session::SessionManager::convert_recognized_stream_event(
                &stream(
                    "bench_build_digest",
                    json!({
                        "harness_build_digest": digest,
                        "harness_source_digest": "sha256:src",
                        "rsid_build_sha": "f".repeat(40),
                        "inputs": {},
                    }),
                ),
                session_id,
                &mut sequence,
            )
            .unwrap();
            for row in digest_rows {
                store.insert_event(&row).unwrap();
            }
        }
        assert_eq!(
            read_build_digest(&store, session_id).unwrap(),
            Some("b".repeat(64))
        );
        // The whole row carries the provenance a bench record copies (#3192).
        let row = read_build_digest_row(&store, session_id).unwrap().unwrap();
        assert_eq!(row["harness_build_digest"], "b".repeat(64));
        assert_eq!(row["harness_source_digest"], "sha256:src");
        assert_eq!(row["rsid_build_sha"], "f".repeat(40));
        assert!(audit_session_route(&store, session_id).unwrap().valid);

        // A fallback row and a stray purpose each fail the session.
        let mut fallback_rows = crate::session::SessionManager::convert_recognized_stream_event(
            &stream("route_fallback", json!({"from":"harness","to":"codex_cli"})),
            session_id,
            &mut sequence,
        )
        .unwrap();
        store.insert_event(&fallback_rows.remove(0)).unwrap();
        assert_eq!(
            audit_session_route(&store, session_id).unwrap().violations,
            ["event:route_fallback"]
        );
        insert_invocation(2, "session.launch_fresh");
        assert_eq!(
            audit_session_route(&store, session_id).unwrap().violations,
            ["event:route_fallback", "purpose:session.launch_fresh"]
        );
    }

    fn route(purpose: &str, provider: &str, model: &str, backend: &str) -> InvocationRoute {
        InvocationRoute {
            purpose: purpose.into(),
            provider: Some(provider.into()),
            model: Some(model.into()),
            backend: Some(backend.into()),
        }
    }

    fn audit_routes(routes: &[InvocationRoute]) -> RouteVerdict {
        let store = Store::open_in_memory().unwrap();
        let session_id = Uuid::new_v4();
        store
            .insert_session(&rsid_store::test_support::test_session(
                session_id,
                std::path::PathBuf::from("/tmp/bench-route"),
            ))
            .unwrap();
        for (n, route) in routes.iter().enumerate() {
            store
                .insert_invocation_route_for_test(session_id, n, route)
                .unwrap();
        }
        audit_session_route(&store, session_id).unwrap()
    }

    /// #3935: the session that ran paid batch 1 (a fresh launch, Harness turns, one resume on
    /// the launch route) audits valid; a title call or an off-route resume still does not.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn a_resume_on_the_pinned_route_is_valid_and_a_title_or_off_route_resume_is_not() {
        let launch = route(
            "session.launch.fresh",
            "OpenRouter",
            "deepseek/deepseek-v4.1-flash",
            "OpenRouter",
        );
        let turn = route(
            "session.harness.turn",
            "Harness",
            "deepseek/deepseek-v4.1-flash",
            "openai",
        );
        let resume = route(
            "session.continue.resume",
            "OpenRouter",
            "deepseek/deepseek-v4.1-flash",
            "OpenRouter",
        );
        // A normal bench session: fresh launch, turns, one resume on the pinned route.
        let verdict = audit_routes(&[launch.clone(), turn.clone(), resume.clone()]);
        assert!(verdict.valid, "{:?}", verdict.violations);
        assert_eq!(verdict.task_state(), None);

        // A title generation (another model route) still fails the session.
        let title = route("session.title", "Codex", "gpt-6-luna", "codex_cli");
        let verdict = audit_routes(&[launch.clone(), turn.clone(), resume.clone(), title]);
        assert_eq!(verdict.violations, ["purpose:session.title"]);

        // A resume on another model, provider or backend is off-route.
        for other in [
            route(
                "session.continue.resume",
                "OpenRouter",
                "other/model",
                "OpenRouter",
            ),
            route(
                "session.continue.resume",
                "Codex",
                "deepseek/deepseek-v4.1-flash",
                "OpenRouter",
            ),
            route(
                "session.continue.resume",
                "OpenRouter",
                "deepseek/deepseek-v4.1-flash",
                "codex_cli",
            ),
        ] {
            let verdict = audit_routes(&[launch.clone(), turn.clone(), other]);
            assert_eq!(verdict.violations, ["purpose:session.continue.resume"]);
        }

        // A resume with no launch route to compare with is not pinned to anything.
        let verdict = audit_routes(&[turn, resume]);
        assert_eq!(verdict.violations, ["purpose:session.continue.resume"]);
    }
}
