//! Contract test: does the installed `claude` binary still honour what rsid
//! actually depends on?
//!
//! # Why this exists
//!
//! Every defect found in the 2026-09-02 Claude provider modernization was a
//! drift between what rsid assumed about the CLI and what the CLI does. None of
//! them were caught by the (extensive) unit suite, because unit tests assert
//! rsid's behaviour against a *fake* claude binary — which by construction
//! agrees with rsid's assumptions. Concretely, the following all shipped:
//!
//! - a hardcoded model catalog that had silently diverged from the TUI's,
//! - a context-window table returning 128k for the default model because
//!   `system/init` reports `claude-opus-5[1m]` and no pattern matched it,
//! - no `--effort` validation, so an unsupported value was accepted by rsid and
//!   then silently downgraded to default effort by the CLI,
//! - two whole stream event types (`rate_limit_event`, `system/api_retry`)
//!   dropped on the floor.
//!
//! This test talks to the REAL binary and fails when an assumption stops
//! holding. It is the only test in the tree that can catch that class.
//!
//! # Running it
//!
//! Gated behind `RSI_CLI_CONTRACT=1` because it requires an authenticated
//! `claude` on PATH and spends a small amount of money (one short `-p` turn).
//! Intended for a nightly/CI job and for anyone touching the provider adapter:
//!
//! ```bash
//! RSI_CLI_CONTRACT=1 cargo test -p rsid --test claude_cli_contract -- --nocapture
//! ```
//!
//! # Reading a failure
//!
//! A failure here is not necessarily a bug in rsi — it usually means the CLI
//! changed. The assertion messages name the rsid code that depends on the thing
//! that moved, so the fix location is in the failure text.

use std::process::Command;

fn gated() -> bool {
    std::env::var("RSI_CLI_CONTRACT").unwrap_or_default() == "1"
}

fn claude_available() -> bool {
    Command::new("claude")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn help_text() -> String {
    let out = Command::new("claude")
        .arg("--help")
        .output()
        .expect("run `claude --help`");
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// The CLI rejects genuinely unknown flags.
///
/// This is the control that makes every other "the CLI still accepts X" claim
/// meaningful. Without it, a passing run could just mean the parser is
/// permissive. It also documents a real trap: `claude --help` is explicitly NOT
/// exhaustive, so a flag's absence from help does not mean it was removed —
/// `--max-turns` is absent from help and still works. Only an actual rejection
/// proves removal.
#[test]
fn unknown_flags_are_rejected_so_acceptance_means_something() {
    if !gated() || !claude_available() {
        return;
    }
    let out = Command::new("claude")
        .args(["--definitely-not-a-real-flag-xyz", "-p", "hi"])
        .output()
        .expect("spawn claude");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("unknown option"),
        "The CLI no longer rejects unknown flags, so this suite's other \
         acceptance assertions prove nothing. Got: {combined}"
    );
}

/// Every enum value rsid can emit is still accepted by the CLI.
///
/// `crates/rsid/src/claude.rs` validates `--effort` against this set and rejects
/// anything else at launch. If the CLI's set shrinks, rsid starts rejecting
/// launches the CLI would have accepted; if it grows, rsid rejects values the
/// operator legitimately wants. Either way this test is the early warning.
///
/// Note the accepted set is per-model — Opus 4.6 and Sonnet 4.6 do not take
/// `xhigh` — so this asserts the union advertised by `--help`, which is what the
/// flag parser itself accepts.
#[test]
fn effort_and_permission_mode_values_rsid_emits_are_still_advertised() {
    if !gated() || !claude_available() {
        return;
    }
    let help = help_text();

    for effort in ["low", "medium", "high", "xhigh", "max"] {
        assert!(
            help.contains(effort),
            "`--effort {effort}` is no longer advertised. \
             `validated_claude_effort` in crates/rsid/src/claude.rs pins this set."
        );
    }

    // rsid hardcodes `bypassPermissions`; losing it would break every agent
    // session, since `-p` otherwise starts in Manual mode on every plan.
    assert!(
        help.contains("bypassPermissions"),
        "`--permission-mode bypassPermissions` is gone. crates/rsid/src/claude.rs \
         passes it unconditionally and `-p` defaults to Manual without it."
    );
}

/// Flags rsid passes on every launch still exist.
///
/// Deliberately exercised rather than grepped: `--help` is not exhaustive, so
/// presence in help is sufficient evidence but absence is not evidence of
/// removal. For flags missing from help we fall back to a real invocation and
/// require only that the CLI does not reject them.
#[test]
fn flags_rsid_passes_on_every_launch_are_still_accepted() {
    if !gated() || !claude_available() {
        return;
    }
    let help = help_text();
    // Flags rsid always or conditionally passes. Keep in sync with
    // `ClaudeClient::launch` in crates/rsid/src/claude.rs.
    for flag in [
        "--print",
        "--output-format",
        "--verbose",
        "--permission-mode",
        "--model",
        "--system-prompt",
        "--resume",
        "--effort",
    ] {
        assert!(
            help.contains(flag),
            "`{flag}` is no longer advertised by `claude --help`; \
             crates/rsid/src/claude.rs passes it."
        );
    }

    // `--max-turns` is undocumented but live. Prove it is still ACCEPTED rather
    // than asserting it appears in help, which it does not.
    let out = Command::new("claude")
        .args(["--max-turns", "1", "--help"])
        .output()
        .expect("spawn claude");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !combined.contains("unknown option"),
        "`--max-turns` is now rejected. crates/rsid/src/claude.rs passes it as a \
         turn cap; removing it there needs a replacement, not a deletion."
    );
}

/// An unsupported `--effort` value is a SILENT DOWNGRADE, not a rejection.
///
/// This is the justification for rsid validating effort itself. If the CLI ever
/// starts hard-failing instead, rsid's own validation becomes redundant and this
/// test tells us we can simplify. If it keeps warning-and-continuing, the
/// validation must stay.
#[test]
fn unsupported_effort_is_silently_downgraded_by_the_cli() {
    if !gated() || !claude_available() {
        return;
    }
    let out = Command::new("claude")
        .args(["--effort", "ultra", "--help"])
        .output()
        .expect("spawn claude");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let rejected = combined.contains("unknown option") || !out.status.success();
    assert!(
        !rejected,
        "The CLI now hard-rejects an unsupported --effort value. rsid's own \
         effort validation in crates/rsid/src/claude.rs was added because the \
         CLI silently downgraded instead; revisit whether it is still needed."
    );
}

/// The `stream-json` payload still carries every field rsid reads.
///
/// This is the assertion that would have caught the most expensive defects.
/// rsid derives a session's context window, cost, and token accounting from the
/// `result` event and its model identity from `system/init`; all of that is
/// read by key, so a rename is silent. Unknown event types were (until
/// recently) dropped without a diagnostic, which is how `rate_limit_event` and
/// `system/api_retry` went unnoticed.
///
/// COSTS MONEY: runs one short real turn. Gated with the rest of this file.
#[test]
fn stream_json_still_carries_every_field_rsid_reads() {
    if !gated() || !claude_available() {
        return;
    }
    let out = Command::new("claude")
        .args([
            "-p",
            "say only: ok",
            "--output-format",
            "stream-json",
            "--verbose",
            "--permission-mode",
            "bypassPermissions",
        ])
        .output()
        .expect("spawn claude");
    let stdout = String::from_utf8_lossy(&out.stdout);

    let events: Vec<serde_json::Value> = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    assert!(
        !events.is_empty(),
        "`--output-format stream-json` produced no parseable events. \
         crates/rsid/src/session/monitor.rs consumes this stream."
    );

    let init = events
        .iter()
        .find(|e| e["type"] == "system" && e["subtype"] == "init")
        .expect(
            "no `system`/`init` event. crates/rsid/src/session/monitor.rs takes \
             session identity and model authority from it.",
        );
    for field in ["session_id", "model"] {
        assert!(
            !init[field].is_null(),
            "`system/init.{field}` is gone; rsid reads it to bind session identity."
        );
    }
    // Captured by V99 (P1-A). Absence is a downgrade, not a hard break, so this
    // asserts the contract we chose to depend on rather than a crash risk.
    for field in ["claude_code_version", "capabilities"] {
        assert!(
            init.get(field).is_some(),
            "`system/init.{field}` is gone; rsid persists it for capability \
             negotiation (sessions.provider_cli_version / provider_capabilities)."
        );
    }

    let result = events
        .iter()
        .find(|e| e["type"] == "result")
        .expect("no `result` event; rsid settles a turn on it");

    assert!(
        !result["total_cost_usd"].is_null(),
        "`result.total_cost_usd` is gone; crates/rsid/src/monitor.rs persists it."
    );
    let usage = &result["usage"];
    for field in [
        "input_tokens",
        "output_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
    ] {
        assert!(
            !usage[field].is_null(),
            "`result.usage.{field}` is gone; crates/rsid/src/monitor.rs sums it \
             into the session's token accounting."
        );
    }

    // The single most load-bearing field: rsid prefers this over its hardcoded
    // table precisely because the true window is environment-dependent (the same
    // model id yields 1M or 200k depending on CLAUDE_CODE_DISABLE_1M_CONTEXT).
    let model_usage = result["modelUsage"]
        .as_object()
        .expect("`result.modelUsage` is gone; rsid reads the context window from it");
    let (key, entry) = model_usage
        .iter()
        .next()
        .expect("`result.modelUsage` is empty");
    assert!(
        !entry["contextWindow"].is_null(),
        "`modelUsage[{key}].contextWindow` is gone. crates/rsid/src/monitor.rs \
         treats this as authoritative; without it the hardcoded fallback table \
         becomes the only source and is wrong whenever 1M context is toggled."
    );
    assert!(
        !entry["canonicalModel"].is_null(),
        "`modelUsage[{key}].canonicalModel` is gone. rsid selects the right \
         entry by matching it, because the map KEY carries a variant suffix \
         (e.g. `claude-opus-5[1m]`) and a multi-model turn has several entries."
    );
}
