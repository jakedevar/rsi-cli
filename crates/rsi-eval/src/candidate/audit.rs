//! The two audits `verify` runs beyond the schema: the leak audit over candidate text (the
//! native Rust audit, held-out terms on every file and generality terms on the files
//! that reach the model) and the proposer read audit over an `AgentReadSessionEvents` export.
//!
//! Findings name the file, line and kind, never the held-out term or task, so a finding is safe
//! to commit in `verify.json` and to show to the proposer.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{RECORDS_PREFIX, Repo};

// ---------------------------------------------------------------------------------------------
// Leak audit
// ---------------------------------------------------------------------------------------------

fn run_leak_audit(
    repo: &Repo,
    terms: &str,
    files: &[PathBuf],
    checkout: Option<&Path>,
) -> Result<Vec<String>, String> {
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let mode = match terms {
        "held_out" => crate::tb2::audit::Terms::HeldOut,
        "search" => crate::tb2::audit::Terms::Search,
        _ => return Err("invalid audit terms".into()),
    };
    crate::tb2::audit::audit_files(&repo.root, files, checkout, mode)
        .map(|findings| findings.iter().map(ToString::to_string).collect())
        .map_err(|e| format!("native leak audit failed: {e}"))
}

/// Held-out terms over every candidate file, then generality terms over the model-facing ones.
/// `Err` is an audit that could not run (a failed check, never a pass).
pub(crate) fn leak_audit(
    repo: &Repo,
    all_files: &[PathBuf],
    model_files: &[PathBuf],
    checkout: Option<&Path>,
) -> Result<Vec<String>, String> {
    let mut findings = run_leak_audit(repo, "held_out", all_files, checkout)?;
    findings.extend(run_leak_audit(repo, "search", model_files, checkout)?);
    Ok(findings)
}

// ---------------------------------------------------------------------------------------------
// Proposer read audit
// ---------------------------------------------------------------------------------------------

/// What the read audit saw.
#[derive(Debug, Default)]
pub(crate) struct ReadAuditReport {
    pub findings: Vec<String>,
    pub tool_calls: usize,
    /// Tool calls the export clipped: their unseen tail is not audited.
    pub truncated_tool_calls: usize,
}

/// The event list of an `AgentReadSessionEvents` result: the bare result, an RPC envelope with a
/// `result` member, or a plain array of events.
fn events_of(value: &Value) -> Option<&Vec<Value>> {
    value
        .as_array()
        .or_else(|| value.get("events").and_then(Value::as_array))
        .or_else(|| {
            value
                .get("result")
                .and_then(|r| r.get("events"))
                .and_then(Value::as_array)
        })
}

fn collect_strings(value: &Value, out: &mut String) {
    match value {
        Value::String(s) => {
            out.push_str(s);
            out.push('\n');
        }
        Value::Array(items) => items.iter().for_each(|v| collect_strings(v, out)),
        Value::Object(map) => map.values().for_each(|v| collect_strings(v, out)),
        _ => {}
    }
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Whether `name` occurs in `text` as a whole token (so `path-tracing` is not found in
/// `path-tracing-reverse`).
fn has_token(text: &str, name: &str) -> bool {
    text.match_indices(name).any(|(start, _)| {
        let before = text[..start].chars().next_back();
        let after = text[start + name.len()..].chars().next();
        !before.is_some_and(is_name_char) && !after.is_some_and(is_name_char)
    })
}

/// How a `.rsi/bench` mention refers to the bench run directory.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum BenchRef {
    /// `.rsi/bench/<run>`.
    Run(String),
    /// `.rsi/bench` itself (or a glob over it): the runs are listed, not named.
    Listing,
}

/// Every reference to the operator's bench run directory in `text`.
pub(crate) fn bench_refs(text: &str) -> Vec<BenchRef> {
    const MARK: &str = ".rsi/bench";
    let mut refs = Vec::new();
    for (start, _) in text.match_indices(MARK) {
        let rest = &text[start + MARK.len()..];
        match rest.chars().next() {
            Some('/') => {
                let run: String = rest[1..]
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
                    .collect();
                refs.push(if run.is_empty() {
                    BenchRef::Listing
                } else {
                    BenchRef::Run(run)
                });
            }
            Some(c) if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.') => {}
            _ => refs.push(BenchRef::Listing),
        }
    }
    refs
}

/// Whether the export's result object (bare or under an RPC `result` member) sets `key` true.
fn result_flag(export: &Value, key: &str) -> bool {
    [Some(export), export.get("result")]
        .into_iter()
        .flatten()
        .any(|v| v.get(key).and_then(Value::as_bool) == Some(true))
}

/// Flags tool calls of the proposer session that touch the committed records, a bench run the
/// candidate store does not list, or a held-out task name.
pub(crate) fn read_audit(
    export: &Value,
    held_out: &[String],
    listed_runs: &BTreeSet<String>,
) -> Result<ReadAuditReport, String> {
    let events = events_of(export)
        .ok_or_else(|| "no events array (expected an AgentReadSessionEvents export)".to_string())?;
    let mut report = ReadAuditReport::default();
    // A partial export cannot prove the session read nothing it should not have.
    if result_flag(export, "has_more") {
        report
            .findings
            .push("READ kind=incomplete_export cause=has_more".into());
    }
    if result_flag(export, "truncated") {
        report
            .findings
            .push("READ kind=incomplete_export cause=truncated".into());
    }
    for event in events {
        let kind = event
            .get("event_type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let tool_input = event.get("tool_input").filter(|v| !v.is_null());
        let is_call = tool_input.is_some()
            || kind
                .chars()
                .filter(char::is_ascii_alphabetic)
                .collect::<String>()
                .eq_ignore_ascii_case("tooluse");
        if !is_call {
            continue;
        }
        report.tool_calls += 1;
        let sequence = event.get("sequence").and_then(Value::as_i64).unwrap_or(-1);
        let tool = event
            .get("tool_name")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if event.get("truncated").and_then(Value::as_bool) == Some(true) {
            report.truncated_tool_calls += 1;
            report.findings.push(format!(
                "READ seq={sequence} tool={tool} kind=incomplete_export cause=truncated_tool_call"
            ));
        }
        let mut text = String::new();
        if let Some(input) = tool_input {
            collect_strings(input, &mut text);
        }
        if let Some(content) = event.get("content") {
            collect_strings(content, &mut text);
        }
        let at = format!("READ seq={sequence} tool={tool}");
        if text.contains(RECORDS_PREFIX.trim_end_matches('/')) {
            report.findings.push(format!("{at} kind=records_path"));
        }
        let mut flagged_runs = BTreeSet::new();
        let mut listing = false;
        for reference in bench_refs(&text) {
            match reference {
                BenchRef::Run(run) if !listed_runs.contains(&run) => {
                    flagged_runs.insert(run);
                }
                BenchRef::Run(_) => {}
                BenchRef::Listing => listing = true,
            }
        }
        for run in flagged_runs {
            report
                .findings
                .push(format!("{at} kind=bench_run run={run}"));
        }
        if listing {
            report.findings.push(format!("{at} kind=bench_dir_listing"));
        }
        if held_out.iter().any(|name| has_token(&text, name)) {
            report.findings.push(format!("{at} kind=held_out_task"));
        }
    }
    Ok(report)
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn whole_token_matching_keeps_a_task_apart_from_its_longer_sibling() {
        assert!(has_token("cat tests/path-tracing/x", "path-tracing"));
        assert!(!has_token("ls path-tracing-reverse", "path-tracing"));
        assert!(!has_token("ls my_path-tracing", "path-tracing"));
        assert!(has_token("path-tracing", "path-tracing"));
    }

    #[test]
    fn bench_refs_name_the_run_or_the_listing() {
        assert_eq!(
            bench_refs("cat ~/.rsi/bench/run-1/trace.jsonl"),
            vec![BenchRef::Run("run-1".into())]
        );
        assert_eq!(bench_refs("ls ~/.rsi/bench"), vec![BenchRef::Listing]);
        assert_eq!(bench_refs("ls ~/.rsi/bench/*"), vec![BenchRef::Listing]);
        assert!(bench_refs("ls ~/.rsi/bench-old/x").is_empty());
    }
}
