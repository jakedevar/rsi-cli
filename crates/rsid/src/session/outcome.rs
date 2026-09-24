//! Event-stream outcome parser for post-finalize telemetry.
//!
//! Walks a session's chronological `ConversationEvent` stream and extracts:
//! - `test_passed: Option<bool>` — from the final `cargo test` invocation.
//! - `clippy_passed: Option<bool>` — from the final `cargo clippy` invocation.
//!
//! # Pairing contract (ID-keyed, with chronological fallback)
//!
//! Since migration V96, `ConversationEvent` carries `tool_use_id`: a `ToolUse`
//! event stores the provider's `block.id`, and its `ToolResult` stores the
//! matching `block.tool_use_id`. We pair exactly by that key, so N parallel
//! tool calls whose results arrive interleaved (e.g. `cargo test` and
//! `cargo clippy` in flight simultaneously) are each attributed to the right
//! invocation. A `ToolResult` whose id matches no outstanding matching `ToolUse`
//! is simply ignored — it belongs to some other tool call.
//!
//! Rows written before V96 have `tool_use_id == None` and are unpairable by
//! ID. For those we keep the original strict-chronological heuristic as an
//! explicit fallback: a single pending slot holding the most-recent id-less
//! matching `ToolUse`, consumed by the next id-less `ToolResult`. Id-less
//! ToolUses never enter the ID map, so they cannot collide with each other
//! (or with real ids) there. In a mixed stream, an id-less `ToolResult` with
//! no id-less pending slot falls back to consuming the *oldest* outstanding
//! ID-keyed call, which reproduces the legacy in-order pairing.
//!
//! # Final-invocation-wins
//!
//! A session may run `cargo test` multiple times — only the final result
//! reflects the "outcome" the user cares about. We overwrite the
//! accumulated `last` result on every parseable matching ToolResult.
//! Unparseable results (garbled output, empty, unknown pattern) leave
//! the previous `last` value in place — they do not clobber a known-good
//! prior result to None.
//!
//! # Non-measurement
//!
//! When a session has no matching ToolUse (e.g. a session that never ran
//! `cargo test`), the returned field is `None`, NOT `Some(false)`. The
//! distinction is load-bearing: downstream analytics must be able to tell
//! "did not run tests" apart from "ran tests and they failed".

use rsi_common::types::{ConversationEvent, EventType};

pub(super) struct OutcomeProbe {
    pub test_passed: Option<bool>,
    pub clippy_passed: Option<bool>,
}

/// Scan a session's event stream for the final-invocation results of
/// `cargo test` and `cargo clippy`.
pub(super) fn probe_outcomes(events: &[ConversationEvent]) -> OutcomeProbe {
    OutcomeProbe {
        test_passed: scan_cargo(events, "cargo test", parse_test_result),
        clippy_passed: scan_cargo(events, "cargo clippy", parse_clippy_result),
    }
}

/// Walk `events` in order and pair each `Bash`-tool `ToolUse` whose command
/// contains `needle` with its `ToolResult` — by `tool_use_id` when present,
/// else by the legacy chronological heuristic (see module docs). Apply
/// `parse` to that result's content; update the running `last` outcome only
/// when parse succeeds. Returns `None` when no matching ToolUse existed, or
/// when every matching ToolUse had an unparseable ToolResult.
fn scan_cargo(
    events: &[ConversationEvent],
    needle: &str,
    parse: fn(&str) -> Option<bool>,
) -> Option<bool> {
    let mut last: Option<bool> = None;
    // Outstanding matching ToolUse ids, oldest first. Insertion order is
    // preserved so an id-less ToolResult can fall back to the oldest.
    let mut pending_ids: Vec<&str> = Vec::new();
    // Legacy single slot: `true` when an id-less matching ToolUse awaits its
    // (also id-less) ToolResult. A newer id-less matching ToolUse overwrites
    // the slot, exactly as the pre-V81 heuristic did.
    let mut pending_legacy = false;

    for ev in events {
        match ev.event_type {
            EventType::ToolUse => {
                if ev.tool_name.as_deref() != Some("Bash") {
                    continue;
                }
                let cmd = ev
                    .tool_input
                    .as_ref()
                    .and_then(|v| v.get("command"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if !cmd.contains(needle) {
                    continue;
                }
                match ev.tool_use_id.as_deref() {
                    Some(id) => {
                        if !pending_ids.contains(&id) {
                            pending_ids.push(id);
                        }
                    }
                    None => pending_legacy = true,
                }
            }
            EventType::ToolResult => {
                let matched = match ev.tool_use_id.as_deref() {
                    // Exact ID pairing: a result for a call we are not
                    // tracking belongs to some other tool and is ignored.
                    Some(id) => match pending_ids.iter().position(|p| *p == id) {
                        Some(idx) => {
                            pending_ids.remove(idx);
                            true
                        }
                        None => false,
                    },
                    // Legacy path: prefer the id-less pending slot, else fall
                    // back to the oldest outstanding ID-keyed call.
                    None => {
                        if pending_legacy {
                            pending_legacy = false;
                            true
                        } else if pending_ids.is_empty() {
                            false
                        } else {
                            pending_ids.remove(0);
                            true
                        }
                    }
                };
                if matched {
                    if let Some(ok) = parse(&ev.content) {
                        last = Some(ok);
                    }
                }
            }
            _ => {}
        }
    }

    last
}

/// Parse the output of `cargo test` for a final pass/fail verdict.
///
/// Cargo emits one `test result: ok|FAILED` line per crate tested. The
/// *last* such line wins — a multi-crate run with one failure produces a
/// mix of lines, but the final aggregate is what cargo exits on.
///
/// Returns `None` when no `test result:` line is present (unparseable).
fn parse_test_result(result_content: &str) -> Option<bool> {
    // Find the LAST occurrence of "test result: ". `rfind` returns the
    // starting byte index of the last match; anything after that prefix
    // is the verdict token we compare.
    let start = result_content.rfind("test result: ")?;
    let tail = &result_content[start..];
    if tail.starts_with("test result: ok") {
        Some(true)
    } else if tail.starts_with("test result: FAILED") {
        Some(false)
    } else {
        None
    }
}

/// Parse the output of `cargo clippy` for a final pass/fail verdict.
///
/// Clippy run with `-D warnings` will emit `error:` lines for any warning
/// it finds; run without, only hard errors produce `error:`. In either
/// case an `error:`-prefixed line is a definite fail. Empty output (clippy
/// never ran or its stderr was silenced) yields `None`.
fn parse_clippy_result(result_content: &str) -> Option<bool> {
    if result_content.trim().is_empty() {
        return None;
    }
    let has_error = result_content.lines().any(|l| l.starts_with("error:"));
    Some(!has_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use rsi_common::types::{ConversationEvent, EventType, Role};
    use uuid::Uuid;

    /// Build a synthetic ToolUse event for a `cargo` Bash invocation.
    fn bash_use(seq: i32, cmd: &str) -> ConversationEvent {
        ConversationEvent {
            id: 0,
            session_id: Uuid::nil(),
            sequence: seq,
            event_type: EventType::ToolUse,
            role: Some(Role::Assistant),
            created_at: Utc::now(),
            content: String::new(),
            tool_name: Some("Bash".to_string()),
            tool_input: Some(Box::new(serde_json::json!({ "command": cmd }))),
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        }
    }

    /// Build a synthetic ToolResult event with the given content.
    fn tool_result(seq: i32, content: &str) -> ConversationEvent {
        ConversationEvent {
            id: 0,
            session_id: Uuid::nil(),
            sequence: seq,
            event_type: EventType::ToolResult,
            role: Some(Role::User),
            created_at: Utc::now(),
            content: content.to_string(),
            tool_name: None,
            tool_input: None,
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        }
    }

    /// ToolUse carrying a provider `block.id` (post-V81 rows).
    fn bash_use_id(seq: i32, cmd: &str, id: &str) -> ConversationEvent {
        let mut ev = bash_use(seq, cmd);
        ev.tool_use_id = Some(id.to_string());
        ev
    }

    /// ToolResult carrying a `block.tool_use_id` (post-V81 rows).
    fn tool_result_id(seq: i32, content: &str, id: &str) -> ConversationEvent {
        let mut ev = tool_result(seq, content);
        ev.tool_use_id = Some(id.to_string());
        ev
    }

    fn plain_message(seq: i32) -> ConversationEvent {
        ConversationEvent {
            id: 0,
            session_id: Uuid::nil(),
            sequence: seq,
            event_type: EventType::Message,
            role: Some(Role::Assistant),
            created_at: Utc::now(),
            content: "hello".to_string(),
            tool_name: None,
            tool_input: None,
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        }
    }

    #[test]
    fn empty_event_stream_returns_none_none() {
        let probe = probe_outcomes(&[]);
        assert_eq!(probe.test_passed, None);
        assert_eq!(probe.clippy_passed, None);
    }

    #[test]
    fn session_without_cargo_returns_none_none() {
        let events = vec![plain_message(1), plain_message(2)];
        let probe = probe_outcomes(&events);
        assert_eq!(probe.test_passed, None);
        assert_eq!(probe.clippy_passed, None);
    }

    #[test]
    fn passing_cargo_test_returns_some_true() {
        let events = vec![
            bash_use(1, "cargo test"),
            tool_result(
                2,
                "running 5 tests\n\
                 test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured\n",
            ),
        ];
        let probe = probe_outcomes(&events);
        assert_eq!(probe.test_passed, Some(true));
        assert_eq!(probe.clippy_passed, None);
    }

    #[test]
    fn failing_cargo_test_returns_some_false() {
        let events = vec![
            bash_use(1, "cargo test -p rsid"),
            tool_result(
                2,
                "test foo::bar ... FAILED\n\
                 test result: FAILED. 0 passed; 1 failed; 0 ignored\n",
            ),
        ];
        let probe = probe_outcomes(&events);
        assert_eq!(probe.test_passed, Some(false));
    }

    #[test]
    fn final_invocation_wins_over_earlier() {
        // First invocation passes, second fails → Some(false).
        let events = vec![
            bash_use(1, "cargo test"),
            tool_result(2, "test result: ok. 3 passed; 0 failed\n"),
            plain_message(3),
            bash_use(4, "cargo test --release"),
            tool_result(5, "test result: FAILED. 2 passed; 1 failed\n"),
        ];
        let probe = probe_outcomes(&events);
        assert_eq!(probe.test_passed, Some(false));
    }

    #[test]
    fn unparseable_result_preserves_earlier_known_value() {
        // First run passes; second run's result is unparseable garbage.
        // We must NOT overwrite the known Some(true) with None.
        let events = vec![
            bash_use(1, "cargo test"),
            tool_result(2, "test result: ok. 3 passed; 0 failed\n"),
            bash_use(3, "cargo test"),
            tool_result(4, "binary corrupted, no cargo output here"),
        ];
        let probe = probe_outcomes(&events);
        assert_eq!(probe.test_passed, Some(true));
    }

    #[test]
    fn clippy_with_error_line_is_failing() {
        let events = vec![
            bash_use(1, "cargo clippy --workspace -- -D warnings"),
            tool_result(
                2,
                "warning: something\n\
                 error: approximate value of PI found\n\
                 error: could not compile\n",
            ),
        ];
        let probe = probe_outcomes(&events);
        assert_eq!(probe.clippy_passed, Some(false));
    }

    #[test]
    fn clippy_with_only_warnings_is_passing() {
        let events = vec![
            bash_use(1, "cargo clippy"),
            tool_result(
                2,
                "warning: unused variable: `x`\n\
                 warning: redundant closure\n",
            ),
        ];
        let probe = probe_outcomes(&events);
        assert_eq!(probe.clippy_passed, Some(true));
    }

    #[test]
    fn clippy_empty_output_is_none() {
        let events = vec![bash_use(1, "cargo clippy"), tool_result(2, "   \n\t\n")];
        let probe = probe_outcomes(&events);
        assert_eq!(probe.clippy_passed, None);
    }

    #[test]
    fn non_bash_tools_ignored() {
        // A Read or Grep tool invocation that happens to contain "cargo test"
        // in its input must NOT be counted as a cargo test result.
        let mut non_bash = bash_use(1, "ignored");
        non_bash.tool_name = Some("Read".to_string());
        non_bash.tool_input = Some(Box::new(serde_json::json!({ "path": "cargo test.md" })));
        let events = vec![
            non_bash,
            tool_result(2, "test result: FAILED. 0 passed; 1 failed\n"),
        ];
        let probe = probe_outcomes(&events);
        assert_eq!(probe.test_passed, None);
    }

    #[test]
    fn tool_use_without_matching_result_is_dropped() {
        // A ToolUse with no following ToolResult — session interrupted mid-tool.
        // Must not accidentally fabricate a verdict.
        let events = vec![bash_use(1, "cargo test")];
        let probe = probe_outcomes(&events);
        assert_eq!(probe.test_passed, None);
    }

    #[test]
    fn parse_test_result_picks_last_line() {
        // Multi-crate run: first crate fails, second passes. cargo's overall
        // exit code reflects the union, but we picked the last `test result:`
        // line per the parsing contract. We intentionally report the tail.
        let content = "test result: FAILED. 0 passed; 1 failed\n\
                       test result: ok. 5 passed; 0 failed\n";
        assert_eq!(parse_test_result(content), Some(true));
    }

    #[test]
    fn parse_test_result_missing_returns_none() {
        assert_eq!(parse_test_result("random binary output"), None);
        assert_eq!(parse_test_result(""), None);
    }

    #[test]
    fn exact_id_pairing_ignores_unrelated_results() {
        // A result for some other tool call (id "zzz") must not be consumed
        // as the cargo test verdict; the real result (id "t1") must be.
        let events = vec![
            bash_use_id(1, "cargo test", "t1"),
            tool_result_id(2, "test result: FAILED. 0 passed; 1 failed\n", "zzz"),
            tool_result_id(3, "test result: ok. 5 passed; 0 failed\n", "t1"),
        ];
        let probe = probe_outcomes(&events);
        assert_eq!(probe.test_passed, Some(true));
    }

    #[test]
    fn interleaved_parallel_calls_attributed_by_id() {
        // Two cargo test calls dispatched in parallel; results arrive in the
        // REVERSE order they were issued, with a clippy result interleaved.
        // The old chronological heuristic kept one pending slot, so the first
        // result to arrive (t2's) was attributed to the last-seen ToolUse and
        // the second was dropped — here that would have yielded Some(true)
        // for clippy's content or the wrong test verdict. ID pairing gives
        // each call its own result, and final-invocation-wins still applies.
        let events = vec![
            bash_use_id(1, "cargo test -p rsid", "t1"),
            bash_use_id(2, "cargo clippy", "c1"),
            bash_use_id(3, "cargo test -p rsi-common", "t2"),
            tool_result_id(4, "test result: ok. 9 passed; 0 failed\n", "t2"),
            tool_result_id(5, "warning: unused variable\n", "c1"),
            tool_result_id(6, "test result: FAILED. 1 passed; 2 failed\n", "t1"),
        ];
        let probe = probe_outcomes(&events);
        // t1's result is the last parseable test result in stream order.
        assert_eq!(probe.test_passed, Some(false));
        assert_eq!(probe.clippy_passed, Some(true));
    }

    #[test]
    fn parallel_clippy_result_not_attributed_to_test() {
        // clippy's output contains no `test result:` line; if it were
        // misattributed to the pending cargo test call the verdict would be
        // dropped rather than read from the real test result.
        let events = vec![
            bash_use_id(1, "cargo test", "t1"),
            bash_use_id(2, "cargo clippy -- -D warnings", "c1"),
            tool_result_id(3, "error: approximate value of PI found\n", "c1"),
            tool_result_id(4, "test result: ok. 3 passed; 0 failed\n", "t1"),
        ];
        let probe = probe_outcomes(&events);
        assert_eq!(probe.test_passed, Some(true));
        assert_eq!(probe.clippy_passed, Some(false));
    }

    #[test]
    fn legacy_null_id_events_use_chronological_fallback() {
        // Pre-V81 rows: no ids anywhere. Behavior must be unchanged from the
        // original strict-chronological heuristic.
        let events = vec![
            bash_use(1, "cargo test"),
            tool_result(2, "test result: ok. 3 passed; 0 failed\n"),
            bash_use(3, "cargo test"),
            tool_result(4, "test result: FAILED. 2 passed; 1 failed\n"),
        ];
        let probe = probe_outcomes(&events);
        assert_eq!(probe.test_passed, Some(false));
    }

    #[test]
    fn legacy_null_id_results_do_not_collide() {
        // Two id-less matching ToolUses back to back overwrite the single
        // legacy slot (pre-V81 behavior): only one result is consumed, and
        // the trailing id-less result has nothing left to pair with.
        let events = vec![
            bash_use(1, "cargo test"),
            bash_use(2, "cargo test"),
            tool_result(3, "test result: ok. 3 passed; 0 failed\n"),
            tool_result(4, "test result: FAILED. 0 passed; 1 failed\n"),
        ];
        let probe = probe_outcomes(&events);
        assert_eq!(probe.test_passed, Some(true));
    }

    #[test]
    fn mixed_stream_id_less_result_falls_back_to_oldest_call() {
        // ToolUse has an id but its result row predates the migration.
        let events = vec![
            bash_use_id(1, "cargo test", "t1"),
            tool_result(2, "test result: ok. 4 passed; 0 failed\n"),
        ];
        let probe = probe_outcomes(&events);
        assert_eq!(probe.test_passed, Some(true));
    }

    #[test]
    fn orphan_call_with_no_result_is_none() {
        let events = vec![bash_use_id(1, "cargo test", "t1"), plain_message(2)];
        let probe = probe_outcomes(&events);
        assert_eq!(probe.test_passed, None);
    }

    #[test]
    fn orphan_result_with_no_call_is_none() {
        let events = vec![tool_result_id(
            1,
            "test result: FAILED. 0 passed; 1 failed\n",
            "t1",
        )];
        let probe = probe_outcomes(&events);
        assert_eq!(probe.test_passed, None);
        // Same for a fully id-less orphan result.
        let legacy = vec![tool_result(1, "test result: ok. 1 passed; 0 failed\n")];
        assert_eq!(probe_outcomes(&legacy).test_passed, None);
    }

    #[test]
    fn mixed_test_and_clippy_in_same_session() {
        let events = vec![
            bash_use(1, "cargo test"),
            tool_result(2, "test result: ok. 3 passed; 0 failed\n"),
            plain_message(3),
            bash_use(4, "cargo clippy -- -D warnings"),
            tool_result(5, "warning: something minor\n"),
        ];
        let probe = probe_outcomes(&events);
        assert_eq!(probe.test_passed, Some(true));
        assert_eq!(probe.clippy_passed, Some(true));
    }
}
