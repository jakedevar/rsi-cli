//! Amazon Bedrock Responses API through Codex's custom-provider transport.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::process::Command;

pub const BEDROCK_ENV: &str = "AWS_BEARER_TOKEN_BEDROCK";
pub const BEDROCK_DEFAULT_MODEL: &str = "global.openai.gpt-5.6-sol";

pub fn credential() -> Result<String, String> {
    if let Some(value) = std::env::var(BEDROCK_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        return Ok(value);
    }

    let python = token_python();
    if !python.is_file() {
        return Err("Bedrock API key missing; set AWS_BEARER_TOKEN_BEDROCK or install aws-bedrock-token-generator in ~/.rsi/bedrock-token-venv".into());
    }
    let output = StdCommand::new(python)
        .args([
            "-c",
            "import signal; signal.alarm(10); from aws_bedrock_token_generator import provide_token; import sys; sys.stdout.write(provide_token(region=sys.argv[1]))",
            &region()?,
        ])
        .output()
        .map_err(|_| "Bedrock token generator could not start".to_string())?;
    if !output.status.success() {
        return Err("Bedrock token generation failed; check aws-bedrock-token-generator and AWS credentials".into());
    }
    let token = String::from_utf8(output.stdout)
        .map_err(|_| "Bedrock token generator returned invalid UTF-8".to_string())?;
    if !token.starts_with("bedrock-api-key-") || token.contains(char::is_whitespace) {
        return Err("Bedrock token generator returned an invalid key".into());
    }
    Ok(token)
}

fn token_python() -> std::path::PathBuf {
    rsi_common::identity::data_dir().join("bedrock-token-venv/bin/python")
}

pub fn region() -> Result<String, String> {
    let configured = std::env::var("AWS_REGION")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            std::env::var("AWS_DEFAULT_REGION")
                .ok()
                .filter(|value| !value.is_empty())
        })
        .or_else(|| {
            static CLI_REGION: OnceLock<Option<String>> = OnceLock::new();
            CLI_REGION
                .get_or_init(|| {
                    StdCommand::new("aws")
                        .args(["configure", "get", "region"])
                        .output()
                        .ok()
                        .filter(|output| output.status.success())
                        .and_then(|output| String::from_utf8(output.stdout).ok())
                        .map(|value| value.trim().to_owned())
                })
                .clone()
        })
        .ok_or_else(|| "Bedrock AWS region missing; set AWS_REGION".to_string())?;
    if !region_valid(&configured) {
        return Err("Bedrock AWS region is invalid".to_string());
    }
    Ok(configured)
}

fn region_valid(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

pub fn available(codex_available: bool) -> bool {
    codex_available
        && region().is_ok()
        && (std::env::var(BEDROCK_ENV).is_ok_and(|value| !value.trim().is_empty())
            || token_python().is_file())
}

pub struct CodexOverrides {
    values: Vec<String>,
}

impl CodexOverrides {
    /// Overrides for launching `model` through Codex at `codex_binary`.
    ///
    /// Includes a model catalog when [`codex_model_catalog`] can derive one.
    #[must_use]
    pub fn for_launch(region: &str, codex_binary: &Path, model: Option<&str>) -> Self {
        let mut overrides = Self::new(region);
        if let Some(path) = model.and_then(|model| codex_model_catalog(codex_binary, model)) {
            let path = serde_json::to_string(&path.to_string_lossy())
                .unwrap_or_else(|_| unreachable!("serializing a string cannot fail"));
            overrides.values.push(format!("model_catalog_json={path}"));
        }
        overrides
    }

    pub fn new(region: &str) -> Self {
        Self {
            values: vec![
                "model_provider=\"bedrock\"".into(),
                "model_providers.bedrock.name=\"Bedrock\"".into(),
                format!(
                    "model_providers.bedrock.base_url=\"https://bedrock-runtime.{region}.amazonaws.com/openai/v1\""
                ),
                format!("model_providers.bedrock.env_key=\"{BEDROCK_ENV}\""),
                "model_providers.bedrock.wire_api=\"responses\"".into(),
                "model_providers.bedrock.supports_websockets=false".into(),
                // Codex attaches its hosted web_search tool by default, and
                // Bedrock's Responses endpoint rejects the whole turn with
                // "web search is not supported for this request".
                "web_search=\"disabled\"".into(),
            ],
        }
    }

    pub fn values(&self) -> &[String] {
        &self.values
    }

    pub fn append_to(&self, command: &mut Command) {
        for value in &self.values {
            command.args(["-c", value]);
        }
    }
}

const CATALOG_MAX_BYTES: usize = 16 * 1024 * 1024;

/// Writes a Codex model catalog for a Bedrock profile ID and returns its path.
///
/// Codex keys model metadata (context window, prompt, tool set, reasoning
/// levels) by slug, so a Bedrock inference-profile ID such as
/// `us.openai.gpt-6-luna` misses its bundled `gpt-6-luna` entry and runs on
/// generic fallback metadata. Write a one-model catalog that re-keys the
/// bundled entry under the Bedrock ID and return its path. `None` (unknown
/// model or any failure) keeps Codex's fallback behavior.
pub fn codex_model_catalog(codex_binary: &Path, model: &str) -> Option<PathBuf> {
    if !model_id_valid(model) {
        return None;
    }
    let output = StdCommand::new(codex_binary)
        .args(["debug", "models", "--bundled"])
        .output()
        .ok()
        .filter(|output| output.status.success() && output.stdout.len() <= CATALOG_MAX_BYTES)?;
    let encoded = catalog_for(&output.stdout, model)?;
    let dir = rsi_common::identity::data_dir().join("model-catalogs");
    let path = dir.join(format!("bedrock-{model}.json"));
    let written = (|| -> std::io::Result<()> {
        std::fs::create_dir_all(&dir)?;
        let mut temporary = tempfile::Builder::new()
            .prefix(".bedrock-catalog-")
            .tempfile_in(&dir)?;
        temporary.write_all(&encoded)?;
        temporary.as_file_mut().sync_all()?;
        temporary.persist(&path).map_err(|error| error.error)?;
        Ok(())
    })();
    match written {
        Ok(()) => Some(path),
        Err(error) => {
            tracing::warn!(model, error = %error, "Bedrock Codex model catalog write failed");
            None
        }
    }
}

fn model_id_valid(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= 128
        && !model.starts_with('.')
        && model.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b':')
        })
}

/// Bedrock profile IDs are `[<geo>.]openai.<slug>`.
fn bundled_slug(model: &str) -> Option<&str> {
    let (_, slug) = model.split_once("openai.")?;
    (!slug.is_empty()).then_some(slug)
}

fn catalog_for(bundled: &[u8], model: &str) -> Option<Vec<u8>> {
    let slug = bundled_slug(model)?;
    let catalog: serde_json::Value = serde_json::from_slice(bundled).ok()?;
    let mut entry = catalog
        .get("models")?
        .as_array()?
        .iter()
        .find(|entry| entry.get("slug").and_then(|value| value.as_str()) == Some(slug))?
        .clone();
    let object = entry.as_object_mut()?;
    object.insert("slug".into(), model.into());
    // OpenAI service tiers (priority/"fast") are not Bedrock tiers; never
    // let a bundled default request one.
    object.insert("default_service_tier".into(), serde_json::Value::Null);
    object.insert("service_tiers".into(), serde_json::json!([]));
    object.insert("additional_speed_tiers".into(), serde_json::json!([]));
    let mut encoded = serde_json::to_vec(&serde_json::json!({ "models": [entry] })).ok()?;
    encoded.push(b'\n');
    Some(encoded)
}

/// The runtime Responses endpoint has no GET /models operation, so list the
/// region's system inference profiles from the control plane. This uses the
/// same bearer credential as launch rather than the AWS CLI, whose SigV4
/// profile resolution is independent of (and can break without) that key.
pub async fn discover_models() -> Result<Vec<(String, String)>, String> {
    let region = region()?;
    let credential = tokio::task::spawn_blocking(credential)
        .await
        .map_err(|_| "Bedrock credential resolution failed".to_string())??;
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "Bedrock model discovery HTTP client could not be configured".to_string())?;
    let url = format!("https://bedrock.{region}.amazonaws.com/inference-profiles");
    let mut models = Vec::new();
    let mut next_token: Option<String> = None;
    for _ in 0..DISCOVERY_MAX_PAGES {
        let mut request = http
            .get(&url)
            .timeout(Duration::from_secs(10))
            .bearer_auth(&credential)
            .header(reqwest::header::ACCEPT, "application/json")
            .query(&[("maxResults", "1000"), ("typeEquals", "SYSTEM_DEFINED")]);
        if let Some(token) = &next_token {
            request = request.query(&[("nextToken", token)]);
        }
        let response = request.send().await.map_err(|error| {
            if error.is_timeout() {
                "Bedrock model discovery timed out".to_string()
            } else {
                "Bedrock model discovery request failed".to_string()
            }
        })?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!(
                "Bedrock model discovery returned HTTP {status}; check the Bedrock API key and bedrock:ListInferenceProfiles permission"
            ));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|_| "Bedrock model discovery response could not be read".to_string())?;
        next_token = parse_profile_page(&bytes, &mut models)?;
        if next_token.is_none() {
            break;
        }
    }
    models.sort();
    models.dedup();
    if models.is_empty() {
        return Err(
            "Bedrock has no active GPT Responses inference profiles in selected region".into(),
        );
    }
    Ok(models)
}

const DISCOVERY_MAX_PAGES: usize = 10;

/// Appends one ListInferenceProfiles page's usable models; returns the page's
/// continuation token.
fn parse_profile_page(
    bytes: &[u8],
    models: &mut Vec<(String, String)>,
) -> Result<Option<String>, String> {
    let document: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|_| "Bedrock model discovery returned invalid JSON".to_string())?;
    let profiles = document["inferenceProfileSummaries"]
        .as_array()
        .ok_or_else(|| "Bedrock model discovery returned no profile list".to_string())?;
    models.extend(
        profiles
            .iter()
            .filter(|entry| entry["status"] == "ACTIVE")
            .filter_map(|entry| entry["inferenceProfileId"].as_str())
            // Codex requires Responses, and Bedrock model families differ by API.
            .filter(|id| id.contains(".openai.gpt-") && !id.contains("gpt-oss"))
            .map(|id| (id.to_string(), id.to_string())),
    );
    Ok(document["nextToken"]
        .as_str()
        .filter(|token| !token.is_empty())
        .map(str::to_owned))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overrides_target_runtime_without_secret() {
        let overrides = CodexOverrides::new("us-west-1");
        assert!(
            overrides
                .values
                .iter()
                .any(|value| value.contains("bedrock-runtime.us-west-1.amazonaws.com/openai/v1"))
        );
        assert!(
            overrides
                .values
                .iter()
                .any(|value| value.contains("env_key=\"AWS_BEARER_TOKEN_BEDROCK\""))
        );
        assert!(
            overrides
                .values
                .iter()
                .any(|value| value == "web_search=\"disabled\"")
        );
        assert!(region_valid("us-west-1"));
        assert!(!region_valid("us-west-1/evil"));
    }

    #[test]
    fn catalog_rekeys_bundled_entry_under_bedrock_profile_id() {
        let bundled = br#"{"models": [
            {"slug": "gpt-6-sol", "context_window": 1},
            {"slug": "gpt-6-luna", "context_window": 272000,
             "default_service_tier": "priority",
             "service_tiers": [{"id": "priority"}],
             "additional_speed_tiers": ["fast"]}
        ]}"#;
        let encoded = catalog_for(bundled, "us.openai.gpt-6-luna").unwrap();
        let catalog: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
        let models = catalog["models"].as_array().unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["slug"], "us.openai.gpt-6-luna");
        assert_eq!(models[0]["context_window"], 272000);
        assert_eq!(models[0]["default_service_tier"], serde_json::Value::Null);
        assert_eq!(models[0]["service_tiers"], serde_json::json!([]));
        assert_eq!(models[0]["additional_speed_tiers"], serde_json::json!([]));

        let global = catalog_for(bundled, "global.openai.gpt-6-sol").unwrap();
        let global: serde_json::Value = serde_json::from_slice(&global).unwrap();
        assert_eq!(global["models"][0]["slug"], "global.openai.gpt-6-sol");

        assert!(catalog_for(bundled, "us.openai.gpt-9-unknown").is_none());
        assert!(catalog_for(bundled, "us.anthropic.claude-sonnet-5").is_none());
        assert!(model_id_valid("us.openai.gpt-6-luna"));
        assert!(!model_id_valid("../evil"));
        assert!(!model_id_valid("us/openai.gpt"));
    }

    #[test]
    fn profile_page_keeps_active_gpt_responses_models() {
        let page = br#"{
            "inferenceProfileSummaries": [
                {"inferenceProfileId": "us.openai.gpt-6-sol", "status": "ACTIVE"},
                {"inferenceProfileId": "global.openai.gpt-5.6-luna", "status": "ACTIVE"},
                {"inferenceProfileId": "us.openai.gpt-oss-120b", "status": "ACTIVE"},
                {"inferenceProfileId": "us.anthropic.claude-sonnet-5", "status": "ACTIVE"},
                {"inferenceProfileId": "us.openai.gpt-6-luna", "status": "INACTIVE"}
            ],
            "nextToken": "page-2"
        }"#;
        let mut models = Vec::new();
        let next = parse_profile_page(page, &mut models).unwrap();
        assert_eq!(next.as_deref(), Some("page-2"));
        let ids: Vec<&str> = models.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["us.openai.gpt-6-sol", "global.openai.gpt-5.6-luna"]);

        let last = br#"{"inferenceProfileSummaries": []}"#;
        assert_eq!(parse_profile_page(last, &mut models).unwrap(), None);
    }
}
