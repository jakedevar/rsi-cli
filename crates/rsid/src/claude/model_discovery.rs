//! Live Claude model discovery from the installed Claude Code CLI.
//!
//! The CLI has no `models` subcommand, but its stream-json control protocol
//! answers an `initialize` control request with the model list the logged-in
//! account can use (the same call behind the Agent SDK's `supportedModels()`).
//! `initialize` sends no prompt and starts no model turn: `[observed]` against
//! `claude 2.1.283` with `--debug-file`, the probe makes no `/v1/messages`
//! request, finishes in about a second and emits about 8 KB.
//!
//! The probe is best-effort. Any failure (missing binary, older CLI, changed
//! wire shape, timeout) falls back to the last good probe for the same binary
//! and then to the compiled catalog in `rsi_common::claude_catalog`, so the
//! picker never goes empty. That catalog stays the source of context windows
//! and of the labels for models it knows; the probe decides which models are
//! offered.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::error::{DaemonError, Result};

/// Request id stamped on the probe's control request and matched on the reply.
pub(super) const PROBE_REQUEST_ID: &str = "rsi-model-discovery";

/// Argv after the binary. `-p` with stream-json input reads control requests
/// from stdin and exits at EOF without a turn. The remaining flags keep the
/// probe cheap and side-effect free: no MCP server connections, no session
/// transcript on disk, no skill loading. `--bare` is deliberately absent: it
/// skips OAuth, and the account's login is what decides the model list.
pub(super) const PROBE_ARGS: &[&str] = &[
    "-p",
    "--input-format",
    "stream-json",
    "--output-format",
    "stream-json",
    "--verbose",
    "--strict-mcp-config",
    "--no-session-persistence",
    "--disable-slash-commands",
];

/// A picker refresh re-uses a successful probe for this long, so opening the
/// model dropdown repeatedly spawns the CLI at most once per window.
pub(super) const CACHE_TTL: Duration = Duration::from_secs(5 * 60);

/// One NDJSON `initialize` control request, newline-terminated.
pub(super) fn probe_request() -> Vec<u8> {
    let mut request = serde_json::json!({
        "type": "control_request",
        "request_id": PROBE_REQUEST_ID,
        "request": { "subtype": "initialize" },
    })
    .to_string()
    .into_bytes();
    request.push(b'\n');
    request
}

/// Last successful probe for one binary.
struct CachedModels {
    observed_at: Instant,
    models: Vec<(String, String)>,
}

/// Keyed by the binary the probe ran. Held across a probe so concurrent
/// refreshes share one CLI spawn.
static CACHE: LazyLock<tokio::sync::Mutex<HashMap<PathBuf, CachedModels>>> =
    LazyLock::new(|| tokio::sync::Mutex::new(HashMap::new()));

/// Return the cached list when fresh, else run `probe` once and cache its
/// result. A failed probe answers with the last good list for this binary
/// (however old) and then with the compiled catalog.
pub(super) async fn cached_or_probe<F, Fut>(binary_path: &Path, probe: F) -> Vec<(String, String)>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Vec<(String, String)>>>,
{
    let mut cache = CACHE.lock().await;
    if let Some(cached) = cache.get(binary_path)
        && cached.observed_at.elapsed() < CACHE_TTL
    {
        return cached.models.clone();
    }
    match probe().await {
        Ok(models) => {
            cache.insert(
                binary_path.to_path_buf(),
                CachedModels {
                    observed_at: Instant::now(),
                    models: models.clone(),
                },
            );
            models
        }
        Err(error) => {
            let stale = cache.get(binary_path).map(|cached| cached.models.clone());
            tracing::warn!(
                error = %error,
                using_last_probe = stale.is_some(),
                "Claude CLI model probe failed; using the last probe or the compiled catalog",
            );
            stale.unwrap_or_else(catalog_models)
        }
    }
}

/// The compiled catalog as picker entries, in catalog order.
pub(super) fn catalog_models() -> Vec<(String, String)> {
    rsi_common::claude_catalog::CLAUDE_MODEL_MENU
        .iter()
        .map(|(id, name)| (id.to_string(), name.to_string()))
        .collect()
}

/// Extract the picker list from the probe's stdout.
///
/// Reads NDJSON lines until the `control_response` for [`PROBE_REQUEST_ID`];
/// other lines (and unparseable ones) are ignored. Each model's id is its
/// `resolvedModel`, falling back to `value`, so aliases such as `opus` become
/// the concrete id they point at and collapse into that id's entry. Only
/// `claude-*` ids are kept: bare aliases the CLI did not resolve are not
/// stable ids, and the TUI routes `claude-*` ids to this provider.
pub(super) fn parse_initialize_models(stdout: &[u8]) -> Result<Vec<(String, String)>> {
    let text = String::from_utf8_lossy(stdout);
    let response = text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .find(|line| {
            line.get("type").and_then(Value::as_str) == Some("control_response")
                && line.pointer("/response/request_id").and_then(Value::as_str)
                    == Some(PROBE_REQUEST_ID)
        })
        .ok_or_else(|| {
            DaemonError::Process(
                "Claude CLI emitted no control_response for the initialize probe".to_string(),
            )
        })?;
    let response = &response["response"];
    if response.get("subtype").and_then(Value::as_str) != Some("success") {
        let error = response
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        return Err(DaemonError::Process(format!(
            "Claude CLI refused the initialize probe: {error}"
        )));
    }
    let entries = response
        .pointer("/response/models")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            DaemonError::Process("Claude CLI initialize response carried no models".to_string())
        })?;

    let mut models: Vec<(String, String)> = Vec::new();
    let mut default_entries = Vec::new();
    for entry in entries {
        // The `default` alias is labelled "Default (recommended)", which is
        // not a model name. Take its label from any other entry for the same
        // id, and only fall back to it when nothing else names that model.
        if entry.get("value").and_then(Value::as_str) == Some("default") {
            default_entries.push(entry);
            continue;
        }
        push_entry(&mut models, entry, false);
    }
    for entry in default_entries {
        push_entry(&mut models, entry, true);
    }
    if models.is_empty() {
        return Err(DaemonError::Process(
            "Claude CLI initialize response listed no usable claude-* models".to_string(),
        ));
    }
    // Keep the picker's family grouping (the first entry is the TUI's default
    // selection); within a family keep the CLI's own order, newest first.
    models.sort_by_key(|(id, _)| family_rank(id));
    Ok(models)
}

fn push_entry(models: &mut Vec<(String, String)>, entry: &Value, is_default_alias: bool) {
    let text = |key: &str| {
        entry
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    let Some(id) = text("resolvedModel").or_else(|| text("value")) else {
        return;
    };
    if !id.starts_with("claude-") || models.iter().any(|(existing, _)| existing == id) {
        return;
    }
    let cli_label = if is_default_alias {
        // "Sonnet 5 · Efficient for routine tasks" -> "Sonnet 5".
        text("description")
            .and_then(|description| description.split(" · ").next())
            .map(str::trim)
            .filter(|label| !label.is_empty())
    } else {
        text("displayName")
    };
    let label = rsi_common::claude_catalog::claude_model_spec(
        rsi_common::claude_catalog::strip_context_variant_suffix(id),
    )
    .map(|spec| spec.display_name)
    .or(cli_label)
    .unwrap_or(id);
    models.push((id.to_string(), label.to_string()));
}

fn family_rank(id: &str) -> usize {
    ["fable", "opus", "sonnet", "haiku"]
        .iter()
        .position(|family| id.contains(family))
        .unwrap_or(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from a real `claude 2.1.283` initialize reply; the model
    /// entries are verbatim.
    fn observed_stdout() -> Vec<u8> {
        let response = serde_json::json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": PROBE_REQUEST_ID,
                "response": {
                    "commands": [],
                    "models": [
                        {"value":"default","resolvedModel":"claude-sonnet-5","displayName":"Default (recommended)","description":"Sonnet 5 · Efficient for routine tasks"},
                        {"value":"opus","resolvedModel":"claude-opus-5-5","displayName":"Opus 5.5","description":"For complex work and everyday tasks"},
                        {"value":"claude-fable-5-1","resolvedModel":"claude-fable-5-1","displayName":"Fable 5.1","description":"For your toughest challenges"},
                        {"value":"haiku","resolvedModel":"claude-haiku-4-5-20251001","displayName":"Haiku 4.5","description":"Fastest for quick answers"},
                        {"value":"sonnet","resolvedModel":"claude-sonnet-5","displayName":"Sonnet 5","description":"Efficient for routine tasks"},
                        {"value":"claude-opus-5","resolvedModel":"claude-opus-5","displayName":"Opus 5","description":"Best for everyday, complex tasks"},
                        {"value":"claude-fable-5","resolvedModel":"claude-fable-5","displayName":"Fable 5","description":"Most capable"},
                        {"value":"claude-opus-4-8","resolvedModel":"claude-opus-4-8","displayName":"Opus 4.8","description":"Best for everyday, complex tasks"},
                        {"value":"claude-sonnet-4-6","resolvedModel":"claude-sonnet-4-6","displayName":"Sonnet 4.6","description":"Efficient for routine tasks"}
                    ]
                }
            }
        });
        format!(
            "{}\nnot json\n{response}\n",
            serde_json::json!({"type": "system", "subtype": "hook_started"})
        )
        .into_bytes()
    }

    fn pairs(models: &[(String, String)]) -> Vec<(&str, &str)> {
        models
            .iter()
            .map(|(id, name)| (id.as_str(), name.as_str()))
            .collect()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn observed_reply_becomes_resolved_ids_grouped_by_family() {
        let models = parse_initialize_models(&observed_stdout()).expect("parses");
        assert_eq!(
            pairs(&models),
            vec![
                ("claude-fable-5-1", "Fable 5.1 (1M)"),
                ("claude-fable-5", "Fable 5"),
                ("claude-opus-5-5", "Opus 5.5 (1M)"),
                ("claude-opus-5", "Opus 5"),
                ("claude-opus-4-8", "Opus 4.8 (1M)"),
                ("claude-sonnet-5", "Sonnet 5 (1M)"),
                ("claude-sonnet-4-6", "Sonnet 4.6"),
                ("claude-haiku-4-5-20251001", "Haiku 4.5"),
            ]
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn default_alias_labels_from_description_when_it_is_the_only_entry() {
        let stdout = serde_json::json!({
            "type": "control_response",
            "response": {"subtype": "success", "request_id": PROBE_REQUEST_ID, "response": {"models": [
                {"value":"default","resolvedModel":"claude-sonnet-9","displayName":"Default (recommended)","description":"Sonnet 9 · Efficient"},
                {"value":"opus","displayName":"Opus"},
                {"value":"us.anthropic.claude-x","displayName":"Bedrock"}
            ]}}
        })
        .to_string();
        let models = parse_initialize_models(stdout.as_bytes()).expect("parses");
        assert_eq!(pairs(&models), vec![("claude-sonnet-9", "Sonnet 9")]);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn variant_suffixed_ids_keep_their_suffix_and_catalog_label() {
        let stdout = serde_json::json!({
            "type": "control_response",
            "response": {"subtype": "success", "request_id": PROBE_REQUEST_ID, "response": {"models": [
                {"value":"claude-fable-5-1[1m]","label":"Fable","displayName":"Fable"}
            ]}}
        })
        .to_string();
        let models = parse_initialize_models(stdout.as_bytes()).expect("parses");
        assert_eq!(
            pairs(&models),
            vec![("claude-fable-5-1[1m]", "Fable 5.1 (1M)")]
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn unusable_replies_are_errors() {
        let error_reply = serde_json::json!({
            "type": "control_response",
            "response": {"subtype": "error", "request_id": PROBE_REQUEST_ID, "error": "unknown subtype"}
        })
        .to_string();
        let other_request = serde_json::json!({
            "type": "control_response",
            "response": {"subtype": "success", "request_id": "other", "response": {"models": [
                {"value":"claude-opus-5","displayName":"Opus 5"}
            ]}}
        })
        .to_string();
        let no_claude_ids = serde_json::json!({
            "type": "control_response",
            "response": {"subtype": "success", "request_id": PROBE_REQUEST_ID, "response": {"models": [
                {"value":"default","displayName":"Default"}
            ]}}
        })
        .to_string();
        for stdout in [&error_reply, &other_request, &no_claude_ids, &String::new()] {
            assert!(
                parse_initialize_models(stdout.as_bytes()).is_err(),
                "{stdout}"
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn probe_request_is_one_initialize_line() {
        let request = probe_request();
        assert_eq!(request.last(), Some(&b'\n'));
        let value: Value = serde_json::from_slice(&request).expect("json");
        assert_eq!(value["type"], "control_request");
        assert_eq!(value["request_id"], PROBE_REQUEST_ID);
        assert_eq!(value["request"]["subtype"], "initialize");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn cache_serves_fresh_hits_and_falls_back_on_failure() {
        // Unique path so parallel tests sharing the process-wide cache cannot
        // collide with this binary key.
        let binary = PathBuf::from(format!("/nonexistent/claude-{}", uuid::Uuid::new_v4()));
        let probed = vec![("claude-opus-9".to_string(), "Opus 9".to_string())];

        // Failure with no prior probe for this binary: compiled catalog.
        let fallback = cached_or_probe(&binary, || async {
            Err(DaemonError::Process("boom".to_string()))
        })
        .await;
        assert_eq!(fallback, catalog_models());

        let first = cached_or_probe(&binary, || async { Ok(probed.clone()) }).await;
        assert_eq!(first, probed);

        // Fresh cache: the probe must not run.
        let second = cached_or_probe(&binary, || async {
            panic!("fresh cache must not re-probe");
            #[allow(unreachable_code)]
            Ok(Vec::new())
        })
        .await;
        assert_eq!(second, probed);
    }
}
