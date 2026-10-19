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
//!
//! Each model entry also names its effort ladder (`supportedEffortLevels`,
//! #2768). The ladders of the last good probe per binary are kept in a
//! synchronous index ([`known_effort_ladder`]) so the launch can validate
//! `--effort` against the model's own ladder; a model the probe did not
//! describe falls back to the compiled `low..max` list.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex as StdMutex, PoisonError};
use std::time::{Duration, Instant};

use rsi_common::model_utils::ModelEffortCapabilities;
use serde_json::Value;

use crate::error::{DaemonError, Result};

/// Request id stamped on the probe's control request and matched on the reply.
pub(crate) const PROBE_REQUEST_ID: &str = "rsi-model-discovery";

/// Argv after the binary. `-p` with stream-json input reads control requests
/// from stdin and exits at EOF without a turn. The remaining flags keep the
/// probe cheap and side-effect free: no MCP server connections, no session
/// transcript on disk, no skill loading. `--bare` is deliberately absent: it
/// skips OAuth, and the account's login is what decides the model list.
pub(crate) const PROBE_ARGS: &[&str] = &[
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
pub(crate) fn probe_request() -> Vec<u8> {
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

/// The alias the CLI uses for the account's default model. A launch with no
/// `--model` runs it, so its ladder is the one such a launch is checked
/// against.
const DEFAULT_ALIAS: &str = "default";

/// Bound on one effort token taken from the reply.
const MAX_EFFORT_TOKEN_BYTES: usize = 32;

/// One probe's answer: the picker rows plus each described model's ladder.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaudeCatalog {
    pub(crate) models: Vec<(String, String)>,
    /// One entry per picker model whose reply named its ladder
    /// (`supportedEffortLevels`, or `supportsEffort: false` for an empty
    /// one). A model with neither is absent: its ladder is unknown.
    pub(crate) efforts: Vec<ModelEffortCapabilities>,
    /// Lookup key ([`ladder_key`] of the resolved id and of the alias
    /// `value`) to that model's ladder.
    ladders: HashMap<String, Vec<String>>,
}

impl ClaudeCatalog {
    /// The compiled catalog as picker entries, in catalog order, with no
    /// ladder: every model falls back to the compiled effort list.
    fn compiled() -> Self {
        Self {
            models: catalog_models(),
            ..Self::default()
        }
    }
}

/// Last successful probe for one binary.
struct CachedModels {
    observed_at: Instant,
    catalog: ClaudeCatalog,
}

/// Keyed by the binary the probe ran. Held across a probe so concurrent
/// refreshes share one CLI spawn.
static CACHE: LazyLock<tokio::sync::Mutex<HashMap<PathBuf, CachedModels>>> =
    LazyLock::new(|| tokio::sync::Mutex::new(HashMap::new()));

/// The ladders of the last good probe per binary, readable from the
/// synchronous launch path. Written only with [`CACHE`] held.
static LADDERS: LazyLock<StdMutex<HashMap<PathBuf, HashMap<String, Vec<String>>>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

/// Return the cached catalog when fresh, else run `probe` once and cache its
/// result. A failed probe answers with the last good catalog for this binary
/// (however old) and then with the compiled catalog.
pub(super) async fn cached_or_probe<F, Fut>(binary_path: &Path, probe: F) -> ClaudeCatalog
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<ClaudeCatalog>>,
{
    let mut cache = CACHE.lock().await;
    if let Some(cached) = cache.get(binary_path)
        && cached.observed_at.elapsed() < CACHE_TTL
    {
        return cached.catalog.clone();
    }
    match probe().await {
        Ok(catalog) => {
            LADDERS
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(binary_path.to_path_buf(), catalog.ladders.clone());
            cache.insert(
                binary_path.to_path_buf(),
                CachedModels {
                    observed_at: Instant::now(),
                    catalog: catalog.clone(),
                },
            );
            catalog
        }
        Err(error) => {
            let stale = cache.get(binary_path).map(|cached| cached.catalog.clone());
            tracing::warn!(
                error = %error,
                using_last_probe = stale.is_some(),
                "Claude CLI model probe failed; using the last probe or the compiled catalog",
            );
            stale.unwrap_or_else(ClaudeCatalog::compiled)
        }
    }
}

/// The effort ladder the last good probe of `binary_path` reported for
/// `model` (`None` = the CLI default model), or `None` when no probe of that
/// binary described the model: the caller then uses the compiled list.
pub(crate) fn known_effort_ladder(binary_path: &Path, model: Option<&str>) -> Option<Vec<String>> {
    let key = ladder_key(model.unwrap_or(DEFAULT_ALIAS));
    LADDERS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(binary_path)?
        .get(&key)
        .cloned()
}

/// Whether some probe of `binary_path` has succeeded since the daemon started.
pub(crate) fn has_probed(binary_path: &Path) -> bool {
    LADDERS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .contains_key(binary_path)
}

/// The compiled catalog as picker entries, in catalog order.
pub(super) fn catalog_models() -> Vec<(String, String)> {
    rsi_common::claude_catalog::CLAUDE_MODEL_MENU
        .iter()
        .map(|(id, name)| (id.to_string(), name.to_string()))
        .collect()
}

/// A model id or alias as a ladder lookup key: trimmed, context-variant tag
/// (`[1m]`) dropped, lowercased.
fn ladder_key(model: &str) -> String {
    rsi_common::claude_catalog::strip_context_variant_suffix(model.trim()).to_ascii_lowercase()
}

/// The ladder one model entry reports: its `supportedEffortLevels` (unusable
/// tokens dropped), an empty ladder for `supportsEffort: false`, else `None`
/// (the reply does not say, so the ladder stays unknown).
fn entry_ladder(entry: &Value) -> Option<Vec<String>> {
    if let Some(levels) = entry.get("supportedEffortLevels").and_then(Value::as_array) {
        return Some(levels.iter().filter_map(effort_token).collect());
    }
    if entry.get("supportsEffort").and_then(Value::as_bool) == Some(false) {
        return Some(Vec::new());
    }
    None
}

/// One effort token as it may reach `--effort=`: a short lowercase word
/// (a letter, then letters, digits, `-` or `_`).
fn effort_token(level: &Value) -> Option<String> {
    let token = level.as_str()?.trim();
    let usable = token.len() <= MAX_EFFORT_TOKEN_BYTES
        && token
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase())
        && token.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        });
    usable.then(|| token.to_string())
}

/// Extract the picker list and the effort ladders from the probe's stdout.
///
/// Reads NDJSON lines until the `control_response` for [`PROBE_REQUEST_ID`];
/// other lines (and unparseable ones) are ignored. Each model's id is its
/// `resolvedModel`, falling back to `value`, so aliases such as `opus` become
/// the concrete id they point at and collapse into that id's entry. Only
/// `claude-*` ids are kept: bare aliases the CLI did not resolve are not
/// stable ids, and the TUI routes `claude-*` ids to this provider. Every
/// entry's ladder is indexed under its alias `value` as well, so a launch
/// naming `opus` or no model (`default`) finds the ladder of the model the
/// alias resolves to.
pub(super) fn parse_initialize_models(stdout: &[u8]) -> Result<ClaudeCatalog> {
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
    let mut ladders: HashMap<String, Vec<String>> = HashMap::new();
    let mut default_entries = Vec::new();
    for entry in entries {
        index_ladder(&mut ladders, entry);
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
    let efforts = models
        .iter()
        .filter_map(|(id, _)| {
            let supported_efforts = ladders.get(&ladder_key(id))?.clone();
            Some(ModelEffortCapabilities {
                model: id.clone(),
                supported_efforts,
                // The reply names no per-model default effort.
                default_effort: None,
            })
        })
        .collect();
    Ok(ClaudeCatalog {
        models,
        efforts,
        ladders,
    })
}

/// Index `entry`'s ladder under its resolved id and its alias `value`. The
/// first entry naming a key wins, matching the picker's first-entry rule.
fn index_ladder(ladders: &mut HashMap<String, Vec<String>>, entry: &Value) {
    let Some(ladder) = entry_ladder(entry) else {
        return;
    };
    for key in ["resolvedModel", "value"] {
        let Some(name) = entry
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
        else {
            continue;
        };
        ladders
            .entry(ladder_key(name))
            .or_insert_with(|| ladder.clone());
    }
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
            pairs(&models.models),
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
        assert_eq!(pairs(&models.models), vec![("claude-sonnet-9", "Sonnet 9")]);
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
            pairs(&models.models),
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
        let probed = parse_initialize_models(&stub_stdout(&[serde_json::json!(
            {"value":"claude-opus-9","displayName":"Opus 9","supportedEffortLevels":["low","high"]}
        )]))
        .expect("parses");

        // Failure with no prior probe for this binary: compiled catalog, no
        // ladder known for any model.
        let fallback = cached_or_probe(&binary, || async {
            Err(DaemonError::Process("boom".to_string()))
        })
        .await;
        assert_eq!(fallback.models, catalog_models());
        assert_eq!(fallback.efforts, Vec::new());
        assert_eq!(known_effort_ladder(&binary, Some("claude-opus-9")), None);

        let first = cached_or_probe(&binary, || async { Ok(probed.clone()) }).await;
        assert_eq!(first, probed);
        assert_eq!(
            known_effort_ladder(&binary, Some("claude-opus-9")),
            Some(vec!["low".to_string(), "high".to_string()])
        );

        // Fresh cache: the probe must not run.
        let second = cached_or_probe(&binary, || async {
            panic!("fresh cache must not re-probe");
            #[allow(unreachable_code)]
            Ok(ClaudeCatalog::default())
        })
        .await;
        assert_eq!(second, probed);
    }

    /// One initialize reply carrying `models`.
    fn stub_stdout(models: &[Value]) -> Vec<u8> {
        serde_json::json!({
            "type": "control_response",
            "response": {"subtype": "success", "request_id": PROBE_REQUEST_ID, "response": {"models": models}}
        })
        .to_string()
        .into_bytes()
    }

    /// #2768: each model keeps the ladder the CLI reported, findable by its
    /// resolved id, its alias and (for the `default` alias) by no model.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn reply_ladders_are_kept_per_model_and_found_by_id_alias_and_default() {
        let catalog = parse_initialize_models(&stub_stdout(&[
            serde_json::json!({"value":"default","resolvedModel":"claude-opus-9","displayName":"Default (recommended)","description":"Opus 9 · Best","supportsEffort":true,"supportedEffortLevels":["low","medium","high","xhigh","max","ultracode"]}),
            serde_json::json!({"value":"sonnet","resolvedModel":"claude-sonnet-9","displayName":"Sonnet 9","supportsEffort":true,"supportedEffortLevels":["low","medium","high"]}),
            serde_json::json!({"value":"haiku","resolvedModel":"claude-haiku-9","displayName":"Haiku 9","supportsEffort":false}),
            serde_json::json!({"value":"claude-fable-9[1m]","resolvedModel":"claude-fable-9","displayName":"Fable 9"}),
        ]))
        .expect("parses");
        let ladder = |key: &str| catalog.ladders.get(&ladder_key(key)).cloned();
        let words = |levels: &[&str]| levels.iter().map(|l| l.to_string()).collect::<Vec<_>>();
        assert_eq!(
            ladder("claude-opus-9"),
            Some(words(&[
                "low",
                "medium",
                "high",
                "xhigh",
                "max",
                "ultracode"
            ]))
        );
        assert_eq!(ladder("default"), ladder("claude-opus-9"));
        assert_eq!(ladder("sonnet"), Some(words(&["low", "medium", "high"])));
        assert_eq!(
            ladder("claude-sonnet-9[1m]"),
            Some(words(&["low", "medium", "high"]))
        );
        assert_eq!(ladder("claude-haiku-9"), Some(Vec::new()));
        assert_eq!(ladder("claude-fable-9"), None);
        assert_eq!(
            catalog.efforts,
            vec![
                ModelEffortCapabilities {
                    model: "claude-opus-9".into(),
                    supported_efforts: words(&[
                        "low",
                        "medium",
                        "high",
                        "xhigh",
                        "max",
                        "ultracode"
                    ]),
                    default_effort: None,
                },
                ModelEffortCapabilities {
                    model: "claude-sonnet-9".into(),
                    supported_efforts: words(&["low", "medium", "high"]),
                    default_effort: None,
                },
                ModelEffortCapabilities {
                    model: "claude-haiku-9".into(),
                    supported_efforts: Vec::new(),
                    default_effort: None,
                },
            ]
        );
    }

    /// Only short lowercase words reach `--effort=`; anything else the reply
    /// carries in a ladder is dropped.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn ladder_tokens_are_short_lowercase_words() {
        let catalog = parse_initialize_models(&stub_stdout(&[serde_json::json!(
            {"value":"claude-opus-9","supportedEffortLevels":["high"," max ","--evil","Upper",7,"x".repeat(40),"ultracode"]}
        )]))
        .expect("parses");
        assert_eq!(
            catalog.ladders.get("claude-opus-9"),
            Some(&vec![
                "high".to_string(),
                "max".to_string(),
                "ultracode".to_string()
            ])
        );
    }
}
