//! The recursive DAG browser's inspector readbacks (gap sweep G4, hub #3000;
//! crates/rsi/src/overlay/recursive_dag.rs): the bounded artifact preview
//! (`p`), the artifact lookup with its links (Enter on an artifact row), the
//! validation issues of a live validation, and a live attempt's artifact
//! buckets (Enter in the Live panel). Each command builds the TUI's own params
//! type from `rsi_common::rpc` (so an id that is not a UUID is refused before
//! any call), with the TUI's bounds, and forwards exactly one daemon method.

use rsi_common::rpc::{
    GetRecursiveExecutionArtifactParams, GetRecursiveLiveAttemptArtifactsParams,
    ListRecursiveLiveValidationIssuesParams, PreviewRecursiveExecutionArtifactParams,
};
use serde_json::{Value, json};
use tauri::State;

use crate::{AppState, CmdResult, forward};

pub const METHOD_PREVIEW: &str = "PreviewRecursiveExecutionArtifact";
pub const METHOD_ARTIFACT: &str = "GetRecursiveExecutionArtifact";
pub const METHOD_ISSUES: &str = "ListRecursiveLiveValidationIssues";
pub const METHOD_LIVE_ARTIFACTS: &str = "GetRecursiveLiveAttemptArtifacts";

/// The TUI's preview bounds (`RECURSIVE_DAG_ARTIFACT_PREVIEW_MAX_*`) and issue page.
pub const PREVIEW_MAX_BYTES: u32 = 16 * 1024;
pub const PREVIEW_MAX_LINES: u32 = 80;
pub const VALIDATION_ISSUE_LIMIT: u32 = 24;

fn typed<T: serde::de::DeserializeOwned + serde::Serialize>(v: Value) -> Result<Value, String> {
    let t: T = serde_json::from_value(v).map_err(|e| format!("invalid request: {e}"))?;
    serde_json::to_value(t).map_err(|e| e.to_string())
}

pub fn preview_params(graph_id: &str, artifact_id: i64) -> Result<Value, String> {
    typed::<PreviewRecursiveExecutionArtifactParams>(json!({
        "graph_id": graph_id.trim(),
        "artifact_id": artifact_id,
        "byte_offset": null,
        "line_offset": null,
        "max_bytes": PREVIEW_MAX_BYTES,
        "max_lines": PREVIEW_MAX_LINES,
        "render_hint": null,
        "require_complete": false,
    }))
}

pub fn artifact_params(graph_id: &str, artifact_id: i64) -> Result<Value, String> {
    typed::<GetRecursiveExecutionArtifactParams>(json!({
        "graph_id": graph_id.trim(),
        "artifact_id": artifact_id,
        "include_links": true,
    }))
}

/// A validation's issues by its id, else the live attempt's.
pub fn issues_params(
    validation_id: Option<&str>,
    live_attempt_id: Option<&str>,
) -> Result<Value, String> {
    let validation = validation_id.map(str::trim).filter(|s| !s.is_empty());
    let attempt = live_attempt_id.map(str::trim).filter(|s| !s.is_empty());
    if validation.is_none() && attempt.is_none() {
        return Err("a validation id or a live attempt id is required".into());
    }
    typed::<ListRecursiveLiveValidationIssuesParams>(json!({
        "validation_id": validation,
        "live_attempt_id": if validation.is_none() { attempt } else { None },
        "limit": VALIDATION_ISSUE_LIMIT,
    }))
}

/// Every bucket, as the TUI loads them on Enter.
pub fn live_artifacts_params(live_attempt_id: &str) -> Result<Value, String> {
    typed::<GetRecursiveLiveAttemptArtifactsParams>(json!({
        "live_attempt_id": live_attempt_id.trim(),
        "include_prompt": true,
        "include_raw_output": true,
        "include_normalized_output": true,
        "include_validation_report": true,
        "include_diff": true,
        "include_tests": true,
        "include_produced_artifacts": true,
    }))
}

#[tauri::command]
pub async fn dag_artifact_preview(
    state: State<'_, AppState>,
    graph_id: String,
    artifact_id: i64,
) -> CmdResult {
    forward(
        &state,
        METHOD_PREVIEW,
        preview_params(&graph_id, artifact_id)?,
    )
    .await
}

#[tauri::command]
pub async fn dag_artifact_lookup(
    state: State<'_, AppState>,
    graph_id: String,
    artifact_id: i64,
) -> CmdResult {
    forward(
        &state,
        METHOD_ARTIFACT,
        artifact_params(&graph_id, artifact_id)?,
    )
    .await
}

#[tauri::command]
pub async fn dag_validation_issues(
    state: State<'_, AppState>,
    validation_id: Option<String>,
    live_attempt_id: Option<String>,
) -> CmdResult {
    forward(
        &state,
        METHOD_ISSUES,
        issues_params(validation_id.as_deref(), live_attempt_id.as_deref())?,
    )
    .await
}

#[tauri::command]
pub async fn dag_live_artifacts(state: State<'_, AppState>, live_attempt_id: String) -> CmdResult {
    forward(
        &state,
        METHOD_LIVE_ARTIFACTS,
        live_artifacts_params(&live_attempt_id)?,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: &str = "e2cae1d4-1833-48eb-a7e8-6f1dd480fdae";

    #[test]
    fn the_preview_is_bounded_like_the_tui() {
        let p = preview_params(G, 7).unwrap();
        assert_eq!(p["artifact_id"], 7);
        assert_eq!(p["max_bytes"], 16 * 1024);
        assert_eq!(p["max_lines"], 80);
        assert_eq!(p["require_complete"], false);
        assert!(
            preview_params("nope", 7).is_err(),
            "a graph id must be a UUID"
        );
    }

    #[test]
    fn lookup_and_buckets_ask_for_everything_the_inspector_shows() {
        assert_eq!(artifact_params(G, 3).unwrap()["include_links"], true);
        let b = live_artifacts_params(G).unwrap();
        for k in [
            "include_prompt",
            "include_raw_output",
            "include_normalized_output",
            "include_validation_report",
            "include_diff",
            "include_tests",
            "include_produced_artifacts",
        ] {
            assert_eq!(b[k], true, "{k}");
        }
        assert!(live_artifacts_params("x").is_err());
    }

    #[test]
    fn issues_name_the_validation_else_the_attempt() {
        let v = issues_params(Some(G), Some(G)).unwrap();
        assert_eq!(v["validation_id"], G);
        assert_eq!(v["live_attempt_id"], Value::Null);
        assert_eq!(v["limit"], 24);
        let a = issues_params(None, Some(G)).unwrap();
        assert_eq!(a["live_attempt_id"], G);
        assert!(issues_params(None, None).is_err());
    }
}
