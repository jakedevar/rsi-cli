//! #2698: a gate that observes a base red tells the manager, whether it
//! publishes or fails (#2705).
//!
//! The lander prints `base_red=<test>`, `known_red=<test>:...` and
//! `base_red_command=<test>\t<shard command>` for each failure the base shares.
//! A red the known-failure snapshot already owns (`#N` or `known?`) needs no
//! Issue; every other one is filed as (or deduped into) one `qa` Issue, and the
//! outcome detail names the Issue so the owner's wake carries it.

use super::line_value;
use crate::error::Result;
use crate::store::Store;
use crate::store::rolling_queue::GateBaseRed;
use chrono::Utc;
use rsi_common::rolling_queue::RollingQueueEntryV1;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

/// The test name a `known_red=` line annotates and whether the snapshot owns
/// it: the lander ends a line `:new`, `:known?:#N` or `:#N:class`.
fn known_red_line(rest: &str) -> Option<(&str, bool)> {
    if let Some(name) = rest.strip_suffix(":new") {
        return Some((name, false));
    }
    if let Some(at) = rest.rfind(":known?:") {
        return Some((&rest[..at], true));
    }
    let at = rest.rfind(":#")?;
    Some((&rest[..at], true))
}

/// The base reds of one lander report that no snapshot record owns, with the
/// shard command that observed each. `claimed` (a landing's own `fixes_tests`)
/// is excluded: that red is being repaired by the very landing.
pub(super) fn unowned_base_reds(stdout: &str, claimed: &BTreeSet<String>) -> Vec<GateBaseRed> {
    let mut owned = BTreeMap::new();
    let mut commands = BTreeMap::new();
    let mut names = Vec::new();
    for line in stdout.lines() {
        if let Some(name) = line.strip_prefix("base_red=") {
            names.push(name.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("known_red=") {
            if let Some((name, known)) = known_red_line(rest.trim()) {
                owned.insert(name.to_string(), known);
            }
        } else if let Some(rest) = line.strip_prefix("base_red_command=")
            && let Some((name, command)) = rest.split_once('\t')
        {
            commands.insert(name.to_string(), command.trim().to_string());
        }
    }
    let mut seen = BTreeSet::new();
    names
        .into_iter()
        .filter(|name| !name.is_empty() && !claimed.contains(name))
        .filter(|name| !owned.get(name).copied().unwrap_or(false))
        .filter(|name| seen.insert(name.clone()))
        .map(|test| GateBaseRed {
            command: commands.get(&test).cloned(),
            test,
        })
        .collect()
}

/// File (or dedupe into) the Issues for a gate's unowned base reds and return
/// the outcome-detail lines that name them. Best effort: the gate's verdict
/// stands whatever happens here, so a store failure only logs and names nothing.
pub(super) async fn file_base_reds(
    store: &Arc<tokio::sync::Mutex<Store>>,
    batch_id: Uuid,
    group: &[RollingQueueEntryV1],
    stdout: &str,
    log_path: Option<&Path>,
) -> Vec<String> {
    let claimed: BTreeSet<String> = group
        .iter()
        .flat_map(|entry| entry.fixes_tests.iter().cloned())
        .collect();
    let reds = unowned_base_reds(stdout, &claimed);
    let (Some(first), false) = (group.first(), reds.is_empty()) else {
        return Vec::new();
    };
    let entry_id = first.id;
    let base = line_value(stdout, "fetched_target_id").map(str::to_string);
    let log = log_path.map(|path| path.display().to_string());
    let filed: Result<_> = super::with_store(store, move |s| {
        s.report_gate_base_reds(
            batch_id,
            entry_id,
            base.as_deref(),
            log.as_deref(),
            &reds,
            Utc::now(),
        )
    })
    .await;
    match filed {
        Ok(issues) => issues
            .into_iter()
            .map(|issue| {
                format!(
                    "base_red_issue=#{} test={} ({})",
                    issue.issue,
                    issue.test,
                    if issue.filed { "filed" } else { "already open" }
                )
            })
            .collect(),
        Err(error) => {
            tracing::warn!(%error, "could not file the Issue for a gate's base red");
            Vec::new()
        }
    }
}

/// `detail` with the base-red Issue lines appended.
pub(super) fn with_issue_lines(detail: Option<String>, lines: &[String]) -> Option<String> {
    if lines.is_empty() {
        return detail;
    }
    let extra = lines.join("\n");
    Some(match detail {
        Some(text) => format!("{text}\n{extra}"),
        None => extra,
    })
}

#[cfg(test)]
mod tests {
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    const STDOUT: &str = "fetched_target_id=bbbb\nbase_red=demo::new_red\nbase_red=demo::owned\n\
        base_red=demo::maybe\nbase_red=demo::claimed\nbase_red=demo::nolines\n\
        known_red=demo::new_red:new\nknown_red=demo::owned:#77:regression\n\
        known_red=demo::maybe:known?:#8,#9\nknown_red=demo::claimed:new\n\
        base_red_command=demo::new_red\tcargo test -p demo --test t\n";

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn only_reds_no_signature_owns_are_unowned() {
        let claimed = BTreeSet::from(["demo::claimed".to_string()]);
        let reds = unowned_base_reds(STDOUT, &claimed);
        let names: Vec<&str> = reds.iter().map(|red| red.test.as_str()).collect();
        assert_eq!(names, ["demo::new_red", "demo::nolines"]);
        assert_eq!(
            reds[0].command.as_deref(),
            Some("cargo test -p demo --test t")
        );
        assert_eq!(reds[1].command, None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn a_name_that_prefixes_another_keeps_its_own_annotation() {
        let stdout = "base_red=a::b\nbase_red=a::b::c\nknown_red=a::b::c:#1:regression\n\
                      known_red=a::b:new\n";
        let reds = unowned_base_reds(stdout, &BTreeSet::new());
        let names: Vec<&str> = reds.iter().map(|red| red.test.as_str()).collect();
        assert_eq!(names, ["a::b"]);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn issue_lines_extend_the_outcome_detail() {
        let lines = vec!["base_red_issue=#5 test=t (filed)".to_string()];
        assert_eq!(
            with_issue_lines(Some("gate_timing=x".into()), &lines).as_deref(),
            Some("gate_timing=x\nbase_red_issue=#5 test=t (filed)")
        );
        assert_eq!(
            with_issue_lines(Some("d".into()), &[]).as_deref(),
            Some("d")
        );
        assert_eq!(
            with_issue_lines(None, &lines).as_deref(),
            Some("base_red_issue=#5 test=t (filed)")
        );
    }
}
