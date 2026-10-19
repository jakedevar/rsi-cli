//! Admin overlays, part 1 (slice 13a, hub #3015): scheduled jobs, provider
//! keys and MCP servers. Each command validates its input and forwards exactly
//! one operator method the TUI client (`crates/rsi/src/client.rs`) calls.
//!
//! Credentials: a key or MCP secret travels only webview -> daemon, as the TUI
//! does with its write-only forms. Every response that can describe a
//! credential passes an allowlist here first (`redact_provider_credentials`,
//! `redact_mcp_servers`, `redact_credential_metadata`), so only the metadata
//! the TUI shows (state, the SHA-256 fingerprint prefix, check class, route)
//! can reach the webview, whatever field a daemon adds later.

use serde::Deserialize;
use serde_json::{Map, Value, json};
use tauri::State;

use crate::daemon::DaemonClient;
use crate::settings::{REDACTED, is_secret_field};
use crate::{AppState, CmdResult, PROVIDERS, forward, parse_uuid};

/// One daemon call on a client (the commands' bodies, testable without Tauri state).
pub(crate) async fn call(client: &DaemonClient, method: &str, params: Value) -> CmdResult {
    client.call(method, params).await.map_err(|e| e.to_string())
}

// ---------- shared helpers ----------

/// Copy only `keys` of an object (anything else is dropped).
pub(crate) fn pick(value: &Value, keys: &[&str]) -> Value {
    let mut out = Map::new();
    if let Value::Object(map) = value {
        for k in keys {
            if let Some(v) = map.get(*k) {
                out.insert((*k).to_string(), v.clone());
            }
        }
    }
    Value::Object(out)
}

/// Map `f` over an array field (a missing or non-array field becomes `[]`).
fn map_array(value: &Value, f: impl Fn(&Value) -> Value) -> Value {
    Value::Array(
        value
            .as_array()
            .map_or_else(Vec::new, |a| a.iter().map(f).collect()),
    )
}

pub(crate) fn bounded(field: &str, text: &str, max: usize) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err(format!("{field} is empty"));
    }
    if text.len() > max {
        return Err(format!("{field} is longer than {max} bytes"));
    }
    if text.contains('\0') {
        return Err(format!("{field} contains a NUL byte"));
    }
    Ok(())
}

// ---------- scheduled jobs ----------

const MAX_JOB_NAME: usize = 256;
const MAX_JOB_MESSAGE: usize = 64 * 1024;
const MAX_PATH: usize = 4096;
const RECURRENCES: &[&str] = &[
    "Once",
    "EverySeconds",
    "EveryMinutes",
    "EveryHours",
    "EveryDays",
    "EveryWeeks",
    "EveryMonths",
    "EveryYears",
];

/// The schedule form, as the webview sends it (`src/admin-model.ts`).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ScheduleForm {
    pub name: String,
    pub message: String,
    /// `Recurrence` tag (`Once`, `EveryHours`, ...).
    pub recurrence: String,
    /// The interval for every tag but `Once` (at least 1).
    #[serde(default)]
    pub interval: Option<u64>,
    /// RFC3339 UTC anchor, `YYYY-MM-DDTHH:MM:SSZ`.
    pub anchor: String,
    #[serde(default)]
    pub working_dir: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
}

fn valid_anchor(anchor: &str) -> bool {
    let b = anchor.as_bytes();
    b.len() == 20
        && b.iter().enumerate().all(|(i, c)| match i {
            4 | 7 => *c == b'-',
            10 => *c == b'T',
            13 | 16 => *c == b':',
            19 => *c == b'Z',
            _ => c.is_ascii_digit(),
        })
}

/// `{ recurrence: {type, value?}, anchor }` (serde's tagged `Recurrence`).
pub fn schedule_spec(form: &ScheduleForm) -> Result<Value, String> {
    if !RECURRENCES.contains(&form.recurrence.as_str()) {
        return Err(format!("{:?} is not a recurrence", form.recurrence));
    }
    if !valid_anchor(&form.anchor) {
        return Err("anchor must be YYYY-MM-DDTHH:MM:SSZ".into());
    }
    let recurrence = if form.recurrence == "Once" {
        json!({ "type": "Once" })
    } else {
        let n = form.interval.unwrap_or(1).max(1);
        let months_or_years = matches!(form.recurrence.as_str(), "EveryMonths" | "EveryYears");
        if months_or_years && n > u64::from(u32::MAX) {
            return Err("interval is too large".into());
        }
        json!({ "type": form.recurrence, "value": n })
    };
    Ok(json!({ "recurrence": recurrence, "anchor": form.anchor }))
}

fn job_text(form: &ScheduleForm) -> Result<(String, String), String> {
    let name = form.name.trim();
    let message = form.message.trim();
    bounded("name", name, MAX_JOB_NAME).map_err(|_| "Job name is required".to_string())?;
    bounded("message", message, MAX_JOB_MESSAGE)
        .map_err(|_| "Job message/prompt is required".to_string())?;
    Ok((name.to_string(), message.to_string()))
}

/// `CreateScheduledJob` params; the optional launch fields are sent only when set.
pub fn create_job_params(form: &ScheduleForm) -> Result<Value, String> {
    let (name, message) = job_text(form)?;
    let mut p = json!({ "name": name, "message": message, "schedule": schedule_spec(form)? });
    if let Some(dir) = form
        .working_dir
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
    {
        if !dir.starts_with('/') && !dir.contains(":\\") {
            return Err("working directory must be an absolute path".into());
        }
        bounded("working directory", dir, MAX_PATH)?;
        p["working_dir"] = json!(dir);
    }
    if let Some(provider) = form.provider.as_deref().filter(|p| !p.is_empty()) {
        if !PROVIDERS.contains(&provider) {
            return Err(format!("unknown provider {provider:?}"));
        }
        p["provider"] = json!(provider);
    }
    if let Some(model) = form
        .model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
    {
        bounded("model", model, 256)?;
        p["model"] = json!(model);
    }
    if let Some(project) = form.project_id.as_deref().filter(|p| !p.trim().is_empty()) {
        p["project_id"] = json!(parse_uuid("project_id", project)?);
    }
    Ok(p)
}

/// `UpdateScheduledJob` params: name, message and schedule, as the TUI form
/// edits them (the launch fields keep their stored values).
pub fn update_job_params(id: &str, form: &ScheduleForm) -> Result<Value, String> {
    let (name, message) = job_text(form)?;
    Ok(json!({
        "id": parse_uuid("id", id)?,
        "name": name,
        "message": message,
        "schedule": schedule_spec(form)?,
    }))
}

pub fn list_jobs_params(include_history: bool, cursor: Option<&str>) -> Result<Value, String> {
    let mut p = json!({ "include_history": include_history });
    if let Some(c) = cursor.filter(|c| !c.is_empty()) {
        bounded("cursor", c, 1024)?;
        p["cursor"] = json!(c);
    }
    Ok(p)
}

fn id_params(id: &str) -> Result<Value, String> {
    Ok(json!({ "id": parse_uuid("id", id)? }))
}

/// One page of scheduled jobs (`H` adds old disabled history, `m` the next page).
#[tauri::command]
pub async fn list_scheduled_jobs(
    state: State<'_, AppState>,
    include_history: bool,
    cursor: Option<String>,
) -> CmdResult {
    scheduled_jobs(&state.client, include_history, cursor.as_deref()).await
}

pub async fn scheduled_jobs(
    client: &DaemonClient,
    include_history: bool,
    cursor: Option<&str>,
) -> CmdResult {
    let params = list_jobs_params(include_history, cursor)?;
    let page = call(client, "ListScheduledJobs", params).await?;
    // A daemon that predates paging answers a bare array: one final page.
    Ok(if page.is_array() {
        json!({ "jobs": page, "next_cursor": null, "include_history": include_history })
    } else {
        page
    })
}

/// Wakes the daemon is holding while their children run.
#[tauri::command]
pub async fn list_scheduled_job_holds(state: State<'_, AppState>) -> CmdResult {
    forward(&state, "ListScheduledJobHolds", Value::Null).await
}

#[tauri::command]
pub async fn create_scheduled_job(state: State<'_, AppState>, form: ScheduleForm) -> CmdResult {
    forward(&state, "CreateScheduledJob", create_job_params(&form)?).await
}

#[tauri::command]
pub async fn update_scheduled_job(
    state: State<'_, AppState>,
    id: String,
    form: ScheduleForm,
) -> CmdResult {
    forward(&state, "UpdateScheduledJob", update_job_params(&id, &form)?).await
}

#[tauri::command]
pub async fn delete_scheduled_job(state: State<'_, AppState>, id: String) -> CmdResult {
    forward(&state, "DeleteScheduledJob", id_params(&id)?).await
}

/// Enable or disable; the daemon answers `{enabled}`.
#[tauri::command]
pub async fn toggle_scheduled_job(state: State<'_, AppState>, id: String) -> CmdResult {
    forward(&state, "ToggleScheduledJob", id_params(&id)?).await
}

/// Run a job now.
#[tauri::command]
pub async fn trigger_scheduled_job(state: State<'_, AppState>, id: String) -> CmdResult {
    forward(&state, "TriggerScheduledJob", id_params(&id)?).await
}

// ---------- provider keys ----------

/// `ProviderCredentialSlot::ALL` (rsi-common `provider_credentials`).
pub const SLOTS: &[&str] = &[
    "openrouter",
    "anthropic",
    "openai",
    "bedrock",
    "pioneer",
    "inception",
    "deepseek",
    "groq",
    "mistral",
    "xai",
    "cerebras",
    "perplexity",
    "together",
    "fireworks",
    "cohere",
    "sambanova",
    "lepton",
    "nvidia",
    "siliconflow",
    "huggingface",
    "cloudflare",
    "anyscale",
    "octoai",
    "serper",
    "exa",
];
/// Longest secret the forms send.
pub const MAX_SECRET_BYTES: usize = 16 * 1024;

const CHECK_KEYS: &[&str] = &[
    "generation",
    "at",
    "class",
    "http_status",
    "credit_remaining",
    "detail_code",
    "fingerprint",
];
const CREDENTIAL_KEYS: &[&str] = &[
    "slot",
    "state",
    "fingerprint",
    "set_at",
    "rotated_from_fingerprint",
    "cleared_at",
    "generation",
    "route",
    "cli_exposure",
    "last_cli_exposure_at",
];

fn check_slot(slot: &str) -> Result<(), String> {
    if SLOTS.contains(&slot) {
        Ok(())
    } else {
        Err(format!("{slot:?} is not a provider key slot"))
    }
}

/// A write-only secret: present, bounded, no control characters.
pub fn check_secret(secret: &str) -> Result<(), String> {
    if secret.is_empty() {
        return Err("the credential is empty".into());
    }
    if secret.len() > MAX_SECRET_BYTES {
        return Err(format!(
            "the credential is longer than {MAX_SECRET_BYTES} bytes"
        ));
    }
    if secret.chars().any(char::is_control) {
        return Err("the credential contains a control character".into());
    }
    Ok(())
}

/// One slot's metadata, allowlisted (no field can carry key bytes).
pub fn redact_credential(meta: &Value) -> Value {
    let mut out = pick(meta, CREDENTIAL_KEYS);
    if let Some(check) = meta.get("check").filter(|c| c.is_object()) {
        out["check"] = pick(check, CHECK_KEYS);
    }
    out
}

/// `ListProviderCredentials`, allowlisted.
pub fn redact_provider_credentials(list: &Value) -> Value {
    let mut out = pick(list, &["env_compat", "check_ttl_secs"]);
    out["credentials"] = map_array(&list["credentials"], redact_credential);
    out
}

/// `ImportProviderCredentialsFromEnv`: slots, reasons, fingerprints and the env
/// variable names only.
pub fn redact_import(result: &Value) -> Value {
    let mut out = Map::new();
    if let Value::Object(map) = result {
        for (k, v) in map {
            let row = |r: &Value| pick(r, &["slot", "reason", "fingerprint", "env_var"]);
            let kept = match v {
                Value::Array(_) => map_array(v, row),
                Value::Number(_) | Value::Bool(_) => v.clone(),
                _ => continue,
            };
            out.insert(k.clone(), kept);
        }
    }
    Value::Object(out)
}

pub fn set_credential_params(slot: &str, secret: &str) -> Result<Value, String> {
    check_slot(slot)?;
    check_secret(secret)?;
    Ok(json!({ "slot": slot, "secret": secret }))
}

pub fn slot_params(slot: &str) -> Result<Value, String> {
    check_slot(slot)?;
    Ok(json!({ "slot": slot }))
}

#[tauri::command]
pub async fn list_provider_credentials(state: State<'_, AppState>) -> CmdResult {
    provider_credentials(&state.client).await
}

pub async fn provider_credentials(client: &DaemonClient) -> CmdResult {
    let list = call(client, "ListProviderCredentials", Value::Null).await?;
    Ok(redact_provider_credentials(&list))
}

pub async fn write_provider_credential(
    client: &DaemonClient,
    slot: &str,
    secret: &str,
    rotate: bool,
) -> CmdResult {
    let method = if rotate {
        "RotateProviderCredential"
    } else {
        "SetProviderCredential"
    };
    let meta = call(client, method, set_credential_params(slot, secret)?).await?;
    Ok(redact_credential(&meta))
}

/// Set (`s`) or rotate (`r`) a key. The secret goes to the daemon only; the
/// answer is the slot's redacted metadata.
#[tauri::command]
pub async fn set_provider_credential(
    state: State<'_, AppState>,
    slot: String,
    secret: String,
    rotate: bool,
) -> CmdResult {
    write_provider_credential(&state.client, &slot, &secret, rotate).await
}

#[tauri::command]
pub async fn clear_provider_credential(state: State<'_, AppState>, slot: String) -> CmdResult {
    let meta = forward(&state, "ClearProviderCredential", slot_params(&slot)?).await?;
    Ok(redact_credential(&meta))
}

#[tauri::command]
pub async fn check_provider_credential(state: State<'_, AppState>, slot: String) -> CmdResult {
    let meta = forward(&state, "CheckProviderCredential", slot_params(&slot)?).await?;
    Ok(redact_credential(&meta))
}

#[tauri::command]
pub async fn import_provider_credentials(state: State<'_, AppState>) -> CmdResult {
    let result = forward(&state, "ImportProviderCredentialsFromEnv", json!({})).await?;
    Ok(redact_import(&result))
}

/// The CLI login state of `claude` and `codex` (`refresh` probes now).
#[tauri::command]
pub async fn get_provider_login_state(state: State<'_, AppState>, refresh: bool) -> CmdResult {
    let method = if refresh {
        "RefreshProviderLoginState"
    } else {
        "GetProviderLoginState"
    };
    forward(&state, method, Value::Null).await
}

/// The CLI contract canary state (`refresh` runs it now).
#[tauri::command]
pub async fn get_provider_contract_state(state: State<'_, AppState>, refresh: bool) -> CmdResult {
    let method = if refresh {
        "RefreshProviderContractState"
    } else {
        "GetProviderContractState"
    };
    forward(&state, method, Value::Null).await
}

// ---------- MCP servers ----------

const MCP_DEFINITION_KEYS: &[&str] = &[
    "id",
    "command",
    "args",
    "secret_env_names",
    "working_dir",
    "enabled",
];
const MCP_CREDENTIAL_KEYS: &[&str] = &[
    "id",
    "state",
    "fingerprint",
    "set_at",
    "rotated_from_fingerprint",
    "cleared_at",
    "generation",
];
const MAX_MCP_ARGS: usize = 32;
const MAX_MCP_ARG_BYTES: usize = 4096;
const MAX_MCP_ENV_NAMES: usize = 16;

/// The MCP server form (`McpServerDefinition`).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct McpServerForm {
    pub id: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub secret_env_names: Vec<String>,
    #[serde(default)]
    pub working_dir: Option<String>,
    pub enabled: bool,
}

/// `--token=abc` / `password=abc`: a secret-looking `name=value` argument;
/// `Authorization: Bearer abc` / `X-Api-Key: abc`: a credential header; else
/// URL user-info.
fn redact_arg(arg: &str) -> String {
    if let Some((name, _)) = arg.split_once('=')
        && secret_arg_name(name.trim_start_matches('-'))
    {
        return format!("{name}={REDACTED}");
    }
    if let Some(name) = credential_header(arg) {
        return format!("{name}: {REDACTED}");
    }
    mask_query_secrets(&mask_embedded_url_credentials(arg))
}

/// A secret-looking argument or query-parameter name: anything `is_secret_field`
/// matches, plus the short option names MCP servers use for a credential
/// (`--auth`, `--key`, `--access-key`, `--authorization`, hub #3113).
fn secret_arg_name(name: &str) -> bool {
    let name = name.trim().to_ascii_lowercase().replace('-', "_");
    is_secret_field(&name)
        || matches!(
            name.as_str(),
            "auth" | "key" | "access_key" | "secret_key" | "authorization" | "pass" | "pwd"
        )
}

/// `?token=abc` / `&api_key=abc`: the value of every secret-named query
/// parameter masked, up to the next `&`, `#` or whitespace (hub #3113, #3119).
fn mask_query_secrets(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(['?', '&']) {
        out.push_str(&rest[..=i]);
        rest = &rest[i + 1..];
        let name_end = rest
            .find(|c: char| matches!(c, '=' | '&' | '#' | ',' | '?') || c.is_whitespace())
            .unwrap_or(rest.len());
        if rest[name_end..].starts_with('=') && secret_arg_name(&rest[..name_end]) {
            out.push_str(&rest[..=name_end]);
            out.push_str(REDACTED);
            let after = &rest[name_end + 1..];
            // A comma belongs to a query value (`?token=a,b`): mask through it (hub #3119).
            let value_end = after
                .find(|c: char| matches!(c, '&' | '#') || c.is_whitespace())
                .unwrap_or(after.len());
            rest = &after[value_end..];
        }
    }
    out.push_str(rest);
    out
}

/// Every `scheme://user:pass@host` inside `text` with its user-info masked,
/// wherever it sits: a bare URL, an option value (`--url=https://u:p@h`) or
/// any other embedding (hub #3110). `mask_url_credentials` only handles a
/// string that starts with the scheme.
fn mask_embedded_url_credentials(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(sep) = rest.find("://") {
        let scheme_start = rest[..sep]
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-')))
            .map_or(0, |i| i + 1);
        let after = &rest[sep + 3..];
        let authority_end = after
            .find(|c: char| matches!(c, '/' | '?' | '#') || c.is_whitespace())
            .unwrap_or(after.len());
        out.push_str(&rest[..sep + 3]);
        match after[..authority_end].rfind('@') {
            Some(at) if scheme_start < sep => {
                out.push_str(REDACTED);
                rest = &after[at..];
            }
            _ => rest = after,
        }
    }
    out.push_str(rest);
    out
}

/// The header name when `arg` is a `Name: value` header carrying a credential:
/// an authorization header, a secret-looking name, or a Bearer/Basic value.
fn credential_header(arg: &str) -> Option<&str> {
    let (name, value) = arg.split_once(':')?;
    let name = name.trim();
    // `https://u:p@h` splits at the scheme: a header name has no '/' or space.
    if name.is_empty() || name.contains(['/', ' ', '\t']) {
        return None;
    }
    let value = value.trim_start().to_ascii_lowercase();
    let authorization = name.eq_ignore_ascii_case("authorization")
        || name.eq_ignore_ascii_case("proxy-authorization");
    (authorization
        || is_secret_field(&name.replace('-', "_"))
        || value.starts_with("bearer ")
        || value.starts_with("basic "))
    .then_some(name)
}

/// `--token abc`: a secret-looking flag whose value is the next argument.
fn secret_flag(arg: &str) -> bool {
    let name = arg.trim_start_matches('-');
    arg.starts_with('-') && !arg.contains('=') && !name.is_empty() && secret_arg_name(name)
}

/// Every argument redacted on its own, and the value that follows a split
/// secret flag (`--token`, `--api-key`, `--password` ...) replaced whole. The
/// form refuses to save an argument still holding the sentinel, so a
/// redaction is never written back (`upsert_mcp_params`).
fn redact_args(args: &[Value]) -> Vec<Value> {
    // Monotone (hub #3127): every argument gets the full per-argument masking,
    // and the adjacent-flag rule only adds to it. The argument after a secret
    // flag is hidden whole whatever its shape (dash-prefixed included); if that
    // argument is itself a secret flag, the one after it is hidden too. No
    // branch ever returns an argument more visible than redact_arg alone would.
    let mut out = Vec::with_capacity(args.len());
    let mut hide_next = false;
    for arg in args {
        let arg = arg.as_str().unwrap_or_default();
        let masked = redact_arg(arg);
        out.push(json!(if hide_next {
            REDACTED.to_string()
        } else {
            masked
        }));
        hide_next = secret_flag(arg);
    }
    out
}

pub fn redact_mcp_summary(summary: &Value) -> Value {
    let mut definition = pick(&summary["definition"], MCP_DEFINITION_KEYS);
    if let Some(args) = definition.get("args").and_then(Value::as_array) {
        definition["args"] = Value::Array(redact_args(args));
    }
    json!({ "definition": definition, "credential": pick(&summary["credential"], MCP_CREDENTIAL_KEYS) })
}

/// `ListMcpServers`, allowlisted.
pub fn redact_mcp_servers(list: &Value) -> Value {
    json!({ "servers": map_array(&list["servers"], redact_mcp_summary) })
}

fn valid_mcp_id(id: &str) -> bool {
    let b = id.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b[0] != b'-'
        && b[b.len() - 1] != b'-'
        && !id.contains("--")
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

fn valid_env_name(name: &str) -> bool {
    const DENIED: &[&str] = &[
        "PATH",
        "HOME",
        "SHELL",
        "IFS",
        "ENV",
        "BASH_ENV",
        "USER",
        "LOGNAME",
        "PWD",
        "OLDPWD",
        "TMPDIR",
        "TERM",
        "LANG",
        "NODE_OPTIONS",
        "NODE_PATH",
        "PYTHONPATH",
        "PYTHONHOME",
        "PYTHONSTARTUP",
        "RUBYLIB",
        "RUBYOPT",
        "PERL5LIB",
        "PERL5OPT",
        "JAVA_TOOL_OPTIONS",
        "_JAVA_OPTIONS",
        "CLASSPATH",
        "GOFLAGS",
        "CARGO_HOME",
        "RUSTFLAGS",
        "RUSTC_WRAPPER",
    ];
    const DENIED_PREFIXES: &[&str] = &[
        "LD_",
        "DYLD_",
        "RSI_",
        "CLAUDE",
        "CODEX",
        "OPENAI_",
        "ANTHROPIC_",
        "AWS_",
        "GIT_",
        "SSH_",
        "XDG_",
        "LC_",
    ];
    !name.is_empty()
        && name.len() <= 64
        && !DENIED.contains(&name)
        && !DENIED_PREFIXES.iter().any(|p| name.starts_with(p))
        && name
            .bytes()
            .enumerate()
            .all(|(i, c)| c.is_ascii_uppercase() || c == b'_' || (i > 0 && c.is_ascii_digit()))
}

/// `UpsertMcpServer {server}` (rsi-common `validate_mcp_server_definition`).
pub fn upsert_mcp_params(form: &McpServerForm) -> Result<Value, String> {
    if !valid_mcp_id(&form.id) {
        return Err("mcp server id must be canonical lowercase ascii".into());
    }
    if !form.command.starts_with('/') || form.command.len() > 1024 || form.command.contains('\0') {
        return Err("mcp command must be an absolute path".into());
    }
    if form.args.len() > MAX_MCP_ARGS
        || form
            .args
            .iter()
            .any(|a| a.contains('\0') || a.len() > MAX_MCP_ARG_BYTES)
    {
        return Err("mcp args exceed the bounded count or length".into());
    }
    if form.args.iter().any(|a| a.contains(REDACTED)) {
        return Err(format!(
            "an argument still holds {REDACTED}; type its value again"
        ));
    }
    if form.secret_env_names.len() > MAX_MCP_ENV_NAMES
        || !form.secret_env_names.iter().all(|n| valid_env_name(n))
    {
        return Err("mcp secret environment names are invalid or exceed bounds".into());
    }
    let mut names = form.secret_env_names.clone();
    names.sort_unstable();
    if names.windows(2).any(|w| w[0] == w[1]) {
        return Err("mcp secret environment names must be unique".into());
    }
    let working_dir = form
        .working_dir
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty());
    if let Some(dir) = working_dir
        && (!dir.starts_with('/') || dir.len() > MAX_PATH)
    {
        return Err("mcp working_dir must be an absolute path within bounds".into());
    }
    Ok(json!({ "server": {
        "id": form.id,
        "command": form.command,
        "args": form.args,
        "secret_env_names": form.secret_env_names,
        "working_dir": working_dir,
        "enabled": form.enabled,
    }}))
}

fn mcp_id_params(id: &str) -> Result<Value, String> {
    if !valid_mcp_id(id) {
        return Err("mcp server id must be canonical lowercase ascii".into());
    }
    Ok(json!({ "id": id }))
}

pub fn mcp_secret_params(id: &str, secret: &str) -> Result<Value, String> {
    let mut p = mcp_id_params(id)?;
    check_secret(secret)?;
    p["secret"] = json!(secret);
    Ok(p)
}

#[tauri::command]
pub async fn list_mcp_servers(state: State<'_, AppState>) -> CmdResult {
    mcp_servers(&state.client).await
}

pub async fn mcp_servers(client: &DaemonClient) -> CmdResult {
    let list = call(client, "ListMcpServers", Value::Null).await?;
    Ok(redact_mcp_servers(&list))
}

/// `UpsertMcpServer` and `SetMcpServerEnabled` answer with the full stored
/// `McpServerSummary`; it passes the same allowlist and argument redaction as
/// the listing before it reaches the webview (hub #3109).
#[tauri::command]
pub async fn upsert_mcp_server(state: State<'_, AppState>, server: McpServerForm) -> CmdResult {
    upsert_mcp(&state.client, &server).await
}

pub async fn upsert_mcp(client: &DaemonClient, server: &McpServerForm) -> CmdResult {
    let summary = call(client, "UpsertMcpServer", upsert_mcp_params(server)?).await?;
    Ok(redact_mcp_summary(&summary))
}

#[tauri::command]
pub async fn set_mcp_server_enabled(
    state: State<'_, AppState>,
    id: String,
    enabled: bool,
) -> CmdResult {
    set_mcp_enabled(&state.client, &id, enabled).await
}

pub async fn set_mcp_enabled(client: &DaemonClient, id: &str, enabled: bool) -> CmdResult {
    let mut p = mcp_id_params(id)?;
    p["enabled"] = json!(enabled);
    let summary = call(client, "SetMcpServerEnabled", p).await?;
    Ok(redact_mcp_summary(&summary))
}

/// Set or rotate an MCP server's credential (write-only; metadata comes back).
#[tauri::command]
pub async fn set_mcp_server_secret(
    state: State<'_, AppState>,
    id: String,
    secret: String,
    rotate: bool,
) -> CmdResult {
    let method = if rotate {
        "RotateMcpServerSecret"
    } else {
        "SetMcpServerSecret"
    };
    let meta = forward(&state, method, mcp_secret_params(&id, &secret)?).await?;
    Ok(pick(&meta, MCP_CREDENTIAL_KEYS))
}

#[tauri::command]
pub async fn clear_mcp_server_secret(state: State<'_, AppState>, id: String) -> CmdResult {
    let meta = forward(&state, "ClearMcpServerSecret", mcp_id_params(&id)?).await?;
    Ok(pick(&meta, MCP_CREDENTIAL_KEYS))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const ID: &str = "E2CAE1D4-1833-48EB-A7E8-6F1DD480FDAE";
    const LID: &str = "e2cae1d4-1833-48eb-a7e8-6f1dd480fdae";

    fn form() -> ScheduleForm {
        ScheduleForm {
            name: " nightly ".into(),
            message: " run the suite ".into(),
            recurrence: "EveryDays".into(),
            interval: Some(2),
            anchor: "2026-10-18T03:00:00Z".into(),
            working_dir: None,
            provider: None,
            model: None,
            project_id: None,
        }
    }

    #[test]
    fn create_job_params_match_the_tui_request() {
        assert_eq!(
            create_job_params(&form()).unwrap(),
            json!({
                "name": "nightly",
                "message": "run the suite",
                "schedule": {
                    "recurrence": { "type": "EveryDays", "value": 2 },
                    "anchor": "2026-10-18T03:00:00Z",
                },
            })
        );
        let mut f = form();
        f.recurrence = "Once".into();
        f.working_dir = Some("/srv/repo".into());
        f.provider = Some("Claude".into());
        f.model = Some("opus".into());
        f.project_id = Some(ID.into());
        let p = create_job_params(&f).unwrap();
        assert_eq!(p["schedule"]["recurrence"], json!({ "type": "Once" }));
        assert_eq!(p["working_dir"], "/srv/repo");
        assert_eq!(p["provider"], "Claude");
        assert_eq!(p["model"], "opus");
        assert_eq!(p["project_id"], LID);
    }

    #[test]
    fn update_job_params_edit_name_message_and_schedule_only() {
        let p = update_job_params(ID, &form()).unwrap();
        assert_eq!(
            p,
            json!({
                "id": LID,
                "name": "nightly",
                "message": "run the suite",
                "schedule": {
                    "recurrence": { "type": "EveryDays", "value": 2 },
                    "anchor": "2026-10-18T03:00:00Z",
                },
            })
        );
    }

    #[test]
    fn job_params_refuse_bad_input() {
        let mut f = form();
        f.name = "  ".into();
        assert_eq!(create_job_params(&f).unwrap_err(), "Job name is required");
        let mut f = form();
        f.message = String::new();
        assert!(create_job_params(&f).unwrap_err().contains("message"));
        for bad in [
            "2026-10-18",
            "2026-10-18 03:00:00Z",
            "2026-10-18T03:00:00+01:00",
        ] {
            let mut f = form();
            f.anchor = bad.into();
            assert!(create_job_params(&f).is_err(), "{bad}");
        }
        let mut f = form();
        f.recurrence = "Hourly".into();
        assert!(create_job_params(&f).is_err());
        let mut f = form();
        f.provider = Some("Nope".into());
        assert!(create_job_params(&f).is_err());
        let mut f = form();
        f.working_dir = Some("relative/dir".into());
        assert!(create_job_params(&f).is_err());
        assert!(update_job_params("nope", &form()).is_err());
        let mut f = form();
        f.interval = Some(0);
        assert_eq!(
            create_job_params(&f).unwrap()["schedule"]["recurrence"]["value"],
            1
        );
    }

    #[test]
    fn list_and_id_params() {
        assert_eq!(
            list_jobs_params(false, None).unwrap(),
            json!({ "include_history": false })
        );
        assert_eq!(
            list_jobs_params(true, Some("c1")).unwrap(),
            json!({ "include_history": true, "cursor": "c1" })
        );
        assert_eq!(id_params(ID).unwrap(), json!({ "id": LID }));
        assert!(id_params("x").is_err());
    }

    #[test]
    fn provider_credential_lists_never_carry_a_secret() {
        let raw = json!({
            "env_compat": true,
            "check_ttl_secs": 600,
            "credentials": [{
                "slot": "openrouter",
                "state": "vault",
                "fingerprint": "1a2b3c4d",
                "secret": "sk-or-live-SECRET",
                "value": "sk-or-live-SECRET",
                "generation": 3,
                "route": "harness",
                "cli_exposure": "none",
                "check": {
                    "at": "2026-10-18T10:00:00Z",
                    "class": "valid",
                    "detail_code": "ok",
                    "credit_remaining": 4.5,
                    "raw_body": "sk-or-live-SECRET",
                },
            }],
            "debug_dump": "sk-or-live-SECRET",
        });
        let out = redact_provider_credentials(&raw);
        let text = out.to_string();
        assert!(!text.contains("SECRET"), "{text}");
        assert_eq!(out["env_compat"], true);
        assert_eq!(out["credentials"][0]["slot"], "openrouter");
        assert_eq!(out["credentials"][0]["fingerprint"], "1a2b3c4d");
        assert_eq!(out["credentials"][0]["check"]["class"], "valid");
        assert_eq!(out["credentials"][0]["check"]["credit_remaining"], 4.5);
        assert!(out["credentials"][0].get("secret").is_none());
        let single = redact_credential(&raw["credentials"][0]);
        assert!(!single.to_string().contains("SECRET"));
    }

    #[test]
    fn import_results_keep_slots_and_drop_anything_else() {
        let out = redact_import(&json!({
            "imported": [{ "slot": "groq", "fingerprint": "ab12", "env_var": "GROQ_API_KEY", "secret": "gsk-SECRET" }],
            "skipped": [{ "slot": "openai", "reason": "present_in_vault" }],
            "note": "gsk-SECRET",
        }));
        assert!(!out.to_string().contains("SECRET"));
        assert_eq!(out["imported"][0]["env_var"], "GROQ_API_KEY");
        assert_eq!(out["skipped"][0]["reason"], "present_in_vault");
    }

    #[test]
    fn credential_writes_validate_slot_and_secret() {
        assert_eq!(
            set_credential_params("groq", "gsk-123").unwrap(),
            json!({ "slot": "groq", "secret": "gsk-123" })
        );
        assert!(set_credential_params("nope", "x").is_err());
        assert!(set_credential_params("groq", "").is_err());
        assert!(set_credential_params("groq", "a\nb").is_err());
        assert!(set_credential_params("groq", &"x".repeat(MAX_SECRET_BYTES + 1)).is_err());
        assert_eq!(slot_params("exa").unwrap(), json!({ "slot": "exa" }));
        assert!(slot_params("Exa").is_err());
        assert_eq!(SLOTS.len(), 25);
    }

    #[test]
    fn mcp_lists_are_allowlisted_and_secret_args_masked() {
        let out = redact_mcp_servers(&json!({ "servers": [{
            "definition": {
                "id": "search",
                "command": "/usr/bin/srv",
                "args": ["--port=8080", "--api-token=tok-SECRET", "https://u:pw-SECRET@h/x"],
                "secret_env_names": ["SEARCH_KEY"],
                "working_dir": null,
                "enabled": true,
                "env": { "SEARCH_KEY": "SECRET" },
            },
            "credential": { "id": "search", "state": "vault", "fingerprint": "ffee", "secret": "SECRET" },
        }]}));
        let text = out.to_string();
        assert!(!text.contains("SECRET"), "{text}");
        let d = &out["servers"][0]["definition"];
        assert_eq!(d["args"][0], "--port=8080");
        assert_eq!(d["args"][1], format!("--api-token={REDACTED}"));
        assert_eq!(out["servers"][0]["credential"]["fingerprint"], "ffee");
    }

    #[test]
    fn mcp_split_secret_flags_and_credential_headers_are_masked() {
        // Synthetic values only (hub #3102).
        let out = redact_mcp_servers(&json!({ "servers": [{
            "definition": {
                "id": "search",
                "command": "/usr/bin/srv",
                "args": [
                    "serve", "--token", "synthetic-SECRET-1", "--port", "8080",
                    "--api-key", "synthetic-SECRET-2", "-password", "synthetic-SECRET-3",
                    "--header", "Authorization: Bearer synthetic-SECRET-4",
                    "--header", "X-Api-Key: synthetic-SECRET-5",
                    "--header", "Accept: application/json", "https://example.test/mcp",
                ],
                "secret_env_names": [],
                "working_dir": null,
                "enabled": true,
            },
            "credential": { "id": "search", "state": "none" },
        }]}));
        let text = out.to_string();
        assert!(!text.contains("SECRET"), "{text}");
        let args = &out["servers"][0]["definition"]["args"];
        assert_eq!(
            args,
            &json!([
                "serve",
                "--token",
                REDACTED,
                "--port",
                "8080",
                "--api-key",
                REDACTED,
                "-password",
                REDACTED,
                "--header",
                format!("Authorization: {REDACTED}"),
                "--header",
                format!("X-Api-Key: {REDACTED}"),
                "--header",
                "Accept: application/json",
                "https://example.test/mcp",
            ])
        );
        // Editing the listed definition cannot write the sentinel back.
        let form = McpServerForm {
            args: args
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a.as_str().unwrap().to_string())
                .collect(),
            ..mcp()
        };
        assert!(upsert_mcp_params(&form).unwrap_err().contains("again"));
    }

    #[test]
    fn url_userinfo_is_masked_inside_option_values_and_embeddings() {
        // Synthetic values only (hub #3110).
        for (arg, want) in [
            (
                "--url=https://synthetic-user:synthetic-password@example.test/mcp",
                format!("--url=https://{REDACTED}@example.test/mcp"),
            ),
            (
                "https://synthetic-user:synthetic-password@example.test/mcp",
                format!("https://{REDACTED}@example.test/mcp"),
            ),
            (
                "endpoint:wss://u:synthetic-password@example.test:443/x?y=1",
                format!("endpoint:wss://{REDACTED}@example.test:443/x?y=1"),
            ),
            (
                "--mirror=https://a:synthetic-password@one.test,https://b:synthetic-password@two.test",
                format!("--mirror=https://{REDACTED}@one.test,https://{REDACTED}@two.test"),
            ),
            (
                "--url=https://example.test/a@b",
                "--url=https://example.test/a@b".to_string(),
            ),
            ("--port=8080", "--port=8080".to_string()),
            ("://odd", "://odd".to_string()),
        ] {
            assert_eq!(redact_arg(arg), want, "{arg}");
        }
        let form = McpServerForm {
            args: vec![format!("--url=https://{REDACTED}@example.test/mcp")],
            ..mcp()
        };
        assert!(upsert_mcp_params(&form).unwrap_err().contains("again"));
    }

    #[test]
    fn query_secrets_and_short_credential_flags_are_masked() {
        // Synthetic values only (hub #3113).
        let out = redact_mcp_servers(&json!({ "servers": [{
            "definition": {
                "id": "search", "command": "/usr/bin/srv",
                "args": [
                    "https://h.example/mcp?token=synthetic-SECRET-1&region=eu",
                    "--url=https://h.example/mcp?api_key=synthetic-SECRET-2#frag",
                    "--auth", "synthetic-SECRET-3",
                    "--key", "synthetic-SECRET-4",
                    "--access-key", "synthetic-SECRET-5",
                    "--authorization", "Bearer synthetic-SECRET-6",
                    "--key=synthetic-SECRET-7",
                    "https://h.example/search?q=rust&page=2",
                    "--keyboard-layout", "us",
                ],
                "secret_env_names": [], "working_dir": null, "enabled": true,
            },
            "credential": { "id": "search", "state": "none" },
        }]}));
        let text = out.to_string();
        assert!(!text.contains("SECRET"), "{text}");
        assert_eq!(
            out["servers"][0]["definition"]["args"],
            json!([
                format!("https://h.example/mcp?token={REDACTED}&region=eu"),
                format!("--url=https://h.example/mcp?api_key={REDACTED}#frag"),
                "--auth",
                REDACTED,
                "--key",
                REDACTED,
                "--access-key",
                REDACTED,
                "--authorization",
                REDACTED,
                format!("--key={REDACTED}"),
                "https://h.example/search?q=rust&page=2",
                "--keyboard-layout",
                "us",
            ])
        );
        // Saving the masked definition back is refused.
        let form = McpServerForm {
            args: vec![format!("https://h.example/mcp?token={REDACTED}")],
            ..mcp()
        };
        assert!(upsert_mcp_params(&form).unwrap_err().contains("again"));
    }

    #[test]
    fn adjacent_secret_flags_and_comma_query_values_stay_masked() {
        // Synthetic values only (hub #3118, #3119).
        let args = |a: &[&str]| -> Vec<Value> { a.iter().map(|x| json!(x)).collect() };
        for (input, want) in [
            (
                vec!["--auth", "--token", "synthetic-SECRET"],
                json!(["--auth", REDACTED, REDACTED]),
            ),
            (
                vec!["--key", "--api-key", "synthetic-SECRET"],
                json!(["--key", REDACTED, REDACTED]),
            ),
            (
                vec!["--auth", "synthetic-SECRET", "--port", "1"],
                json!(["--auth", REDACTED, "--port", "1"]),
            ),
            (vec!["--key", "--verbose"], json!(["--key", REDACTED])),
            // Hub #3127's probes: a flag-shaped password and a credential header
            // after a secret flag stay hidden.
            (
                vec!["--password", "--synthetic-SECRET-42"],
                json!(["--password", REDACTED]),
            ),
            (
                vec!["--auth", "--secret-header: Bearer synthetic-SECRET"],
                json!(["--auth", REDACTED]),
            ),
            (
                vec!["--secret-header: Bearer synthetic-SECRET"],
                json!([format!("--secret-header: {REDACTED}")]),
            ),
            (
                vec!["--auth", "--token=synthetic-SECRET"],
                json!(["--auth", REDACTED]),
            ),
            (
                vec!["https://example.test/mcp?token=public,synthetic-SECRET&region=eu"],
                json!([format!(
                    "https://example.test/mcp?token={REDACTED}&region=eu"
                )]),
            ),
            (
                vec!["--url=https://example.test/mcp?api_key=a,b,synthetic-SECRET#f"],
                json!([format!(
                    "--url=https://example.test/mcp?api_key={REDACTED}#f"
                )]),
            ),
        ] {
            let out = Value::Array(redact_args(&args(&input)));
            assert!(!out.to_string().contains("SECRET"), "{out}");
            assert_eq!(out, want, "{input:?}");
        }
    }

    /// Property (hub #3127): over many generated argument lists, list context
    /// never shows an argument more than per-argument redaction does, and the
    /// argument after a secret flag is always hidden whole.
    #[test]
    fn list_redaction_is_monotone_over_generated_argument_lists() {
        const POOL: &[&str] = &[
            "--port",
            "8080",
            "serve",
            "--verbose",
            "-v",
            "--auth",
            "--key",
            "--token",
            "--password",
            "--api-key",
            "--access-key",
            "--authorization",
            "-password",
            "--keyboard-layout",
            "--token=synthetic-SECRET",
            "--url=https://u:synthetic-SECRET@h.test/x",
            "https://h.test/m?token=a,synthetic-SECRET&b=1",
            "Authorization: Bearer synthetic-SECRET",
            "--secret-header: Bearer synthetic-SECRET",
            "--synthetic-SECRET-42",
            "-synthetic-SECRET",
            "synthetic-SECRET",
            "Bearer synthetic-SECRET",
            "--header",
            "x=1",
            "",
        ];
        let mut seed: u64 = 0x5eed_3127;
        let mut next = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as usize
        };
        for _ in 0..5000 {
            let len = 1 + next() % 7;
            let args: Vec<&str> = (0..len).map(|_| POOL[next() % POOL.len()]).collect();
            let out = redact_args(&args.iter().map(|a| json!(a)).collect::<Vec<_>>());
            for (i, arg) in args.iter().enumerate() {
                let alone = redact_arg(arg);
                let got = out[i].as_str().unwrap();
                assert!(
                    got == alone || got == REDACTED,
                    "{args:?} -> {out:?} at {i}"
                );
                if alone != *arg {
                    assert_ne!(
                        got, *arg,
                        "{args:?} -> {out:?}: index {i} unmasked in context"
                    );
                }
                if i > 0 && secret_flag(args[i - 1]) {
                    assert_eq!(
                        got, REDACTED,
                        "{args:?} -> {out:?}: value after a secret flag at {i}"
                    );
                }
            }
        }
    }

    fn mcp() -> McpServerForm {
        McpServerForm {
            id: "search".into(),
            command: "/usr/bin/srv".into(),
            args: vec!["--port=8080".into()],
            secret_env_names: vec!["SEARCH_KEY".into()],
            working_dir: Some(" ".into()),
            enabled: true,
        }
    }

    #[test]
    fn upsert_mcp_params_match_the_definition() {
        assert_eq!(
            upsert_mcp_params(&mcp()).unwrap(),
            json!({ "server": {
                "id": "search", "command": "/usr/bin/srv", "args": ["--port=8080"],
                "secret_env_names": ["SEARCH_KEY"], "working_dir": null, "enabled": true,
            }})
        );
        for (f, why) in [
            (
                McpServerForm {
                    id: "Search".into(),
                    ..mcp()
                },
                "id",
            ),
            (
                McpServerForm {
                    id: "a--b".into(),
                    ..mcp()
                },
                "id",
            ),
            (
                McpServerForm {
                    command: "srv".into(),
                    ..mcp()
                },
                "absolute",
            ),
            (
                McpServerForm {
                    secret_env_names: vec!["PATH".into()],
                    ..mcp()
                },
                "environment",
            ),
            (
                McpServerForm {
                    secret_env_names: vec!["AWS_KEY".into()],
                    ..mcp()
                },
                "environment",
            ),
            (
                McpServerForm {
                    secret_env_names: vec!["K".into(), "K".into()],
                    ..mcp()
                },
                "unique",
            ),
            (
                McpServerForm {
                    working_dir: Some("rel".into()),
                    ..mcp()
                },
                "working_dir",
            ),
            (
                McpServerForm {
                    args: vec![format!("--token={REDACTED}")],
                    ..mcp()
                },
                "again",
            ),
        ] {
            let err = upsert_mcp_params(&f).unwrap_err();
            assert!(err.contains(why), "{err} / {why}");
        }
        assert_eq!(
            mcp_secret_params("search", "s3").unwrap(),
            json!({ "id": "search", "secret": "s3" })
        );
        assert!(mcp_secret_params("search", "").is_err());
        assert!(mcp_secret_params("-x", "s").is_err());
    }

    #[cfg(unix)]
    pub(crate) mod round_trip {
        use super::*;
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        pub(crate) type Log = Arc<Mutex<Vec<Value>>>;

        /// A daemon that answers each method with `reply(method, params)` and
        /// records every request line.
        pub(crate) fn fake_daemon(
            dir: &std::path::Path,
            reply: fn(&str, &Value) -> Value,
        ) -> (DaemonClient, Log) {
            let sock = dir.join("d.sock");
            let listener = UnixListener::bind(&sock).unwrap();
            let log: Log = Arc::default();
            let seen = Arc::clone(&log);
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        return;
                    };
                    let (r, mut w) = stream.into_split();
                    let mut line = String::new();
                    BufReader::new(r).read_line(&mut line).await.unwrap();
                    let req: Value = serde_json::from_str(&line).unwrap();
                    seen.lock().unwrap().push(req.clone());
                    let result = reply(req["method"].as_str().unwrap_or_default(), &req["params"]);
                    // `{"__drop": true}` closes the connection without a reply (the write
                    // may have committed); `{"__rpc_error": msg}` answers with an error.
                    if result.get("__drop").is_some() {
                        continue;
                    }
                    let out = match result.get("__rpc_error") {
                        Some(msg) => json!({ "jsonrpc": "2.0", "id": req["id"],
                            "error": { "code": -32602, "message": msg } }),
                        None => json!({ "jsonrpc": "2.0", "id": req["id"], "result": result }),
                    };
                    w.write_all(format!("{out}\n").as_bytes()).await.unwrap();
                }
            });
            (DaemonClient::new(&sock), log)
        }

        fn leaky_daemon(method: &str, params: &Value) -> Value {
            match method {
                "ListProviderCredentials" => json!({
                    "env_compat": false, "check_ttl_secs": 600,
                    "credentials": [{ "slot": "groq", "state": "vault", "fingerprint": "ab12",
                        "route": "harness", "cli_exposure": "none", "secret": "gsk-SECRET" }],
                }),
                // A daemon that (wrongly) echoed the secret back.
                "SetProviderCredential" | "RotateProviderCredential" => json!({
                    "slot": params["slot"], "state": "vault", "fingerprint": "cd34",
                    "route": "harness", "cli_exposure": "none", "secret": params["secret"],
                }),
                "ListMcpServers" => json!({ "servers": [{
                    "definition": { "id": "docs", "command": "/bin/mcp", "args": ["--token=tok-SECRET"],
                        "secret_env_names": [], "working_dir": null, "enabled": true },
                    "credential": { "id": "docs", "state": "absent", "secret": "SECRET" },
                }]}),
                "ListScheduledJobs" => json!([{ "id": "j" }]),
                // The daemon answers a save or an enable toggle with the full stored summary.
                "UpsertMcpServer" | "SetMcpServerEnabled" => json!({
                    "definition": { "id": "docs", "command": "/bin/mcp",
                        "args": ["--token", "synthetic-SECRET-1", "--header", "Authorization: Bearer synthetic-SECRET-2",
                            "--url=https://synthetic-user:synthetic-SECRET-3@example.test/mcp", "--port", "8080",
                            "--auth", "--token", "synthetic-SECRET-6", "https://example.test/m?token=a,synthetic-SECRET-7"],
                        "secret_env_names": ["DOCS_KEY"], "working_dir": null,
                        "enabled": params.get("enabled").cloned().unwrap_or(json!(true)),
                        "env": { "DOCS_KEY": "synthetic-SECRET-4" } },
                    "credential": { "id": "docs", "state": "vault", "fingerprint": "ffee", "secret": "synthetic-SECRET-5" },
                }),
                _ => Value::Null,
            }
        }

        #[tokio::test]
        async fn credentials_reach_the_daemon_but_never_come_back() {
            let dir = tempfile::tempdir().unwrap();
            let (client, log) = fake_daemon(dir.path(), leaky_daemon);
            let list = provider_credentials(&client).await.unwrap();
            assert_eq!(list["credentials"][0]["fingerprint"], "ab12");
            let set = write_provider_credential(&client, "groq", "gsk-typed-SECRET", false)
                .await
                .unwrap();
            assert_eq!(set["fingerprint"], "cd34");
            let rotated = write_provider_credential(&client, "groq", "gsk-new-SECRET", true)
                .await
                .unwrap();
            let mcp = mcp_servers(&client).await.unwrap();
            for out in [&list, &set, &rotated, &mcp] {
                assert!(!out.to_string().contains("SECRET"), "{out}");
            }
            let sent = log.lock().unwrap().clone();
            let methods: Vec<&str> = sent.iter().map(|r| r["method"].as_str().unwrap()).collect();
            assert_eq!(
                methods,
                [
                    "ListProviderCredentials",
                    "SetProviderCredential",
                    "RotateProviderCredential",
                    "ListMcpServers"
                ]
            );
            // The request carries the typed secret exactly once, to the daemon.
            assert_eq!(
                sent[1]["params"],
                json!({ "slot": "groq", "secret": "gsk-typed-SECRET" })
            );
            assert!(sent.iter().all(|r| r.get("session_token").is_none()));
        }

        #[tokio::test]
        async fn mcp_save_and_enable_responses_are_redacted_like_the_listing() {
            // Hub #3109: the mutation responses carry the stored definition.
            let dir = tempfile::tempdir().unwrap();
            let (client, log) = fake_daemon(dir.path(), leaky_daemon);
            let form = McpServerForm {
                id: "docs".into(),
                command: "/bin/mcp".into(),
                args: vec!["--token".into(), "synthetic-typed-SECRET".into()],
                secret_env_names: vec!["DOCS_KEY".into()],
                working_dir: None,
                enabled: true,
            };
            let saved = upsert_mcp(&client, &form).await.unwrap();
            let disabled = set_mcp_enabled(&client, "docs", false).await.unwrap();
            for out in [&saved, &disabled] {
                assert!(!out.to_string().contains("SECRET"), "{out}");
                assert_eq!(
                    out["definition"]["args"],
                    json!([
                        "--token",
                        REDACTED,
                        "--header",
                        format!("Authorization: {REDACTED}"),
                        format!("--url=https://{REDACTED}@example.test/mcp"),
                        "--port",
                        "8080",
                        "--auth",
                        REDACTED,
                        REDACTED,
                        format!("https://example.test/m?token={REDACTED}")
                    ])
                );
                assert_eq!(out["definition"]["secret_env_names"], json!(["DOCS_KEY"]));
                assert_eq!(out["credential"]["fingerprint"], "ffee");
            }
            assert_eq!(disabled["definition"]["enabled"], false);
            let sent = log.lock().unwrap().clone();
            assert_eq!(sent[0]["method"], "UpsertMcpServer");
            // The typed value goes to the daemon once, and never comes back.
            assert_eq!(
                sent[0]["params"]["server"]["args"],
                json!(["--token", "synthetic-typed-SECRET"])
            );
            assert_eq!(sent[1]["method"], "SetMcpServerEnabled");
            assert_eq!(sent[1]["params"], json!({ "id": "docs", "enabled": false }));
        }

        #[tokio::test]
        async fn a_bare_array_job_list_is_one_final_page() {
            let dir = tempfile::tempdir().unwrap();
            let (client, log) = fake_daemon(dir.path(), leaky_daemon);
            let page = scheduled_jobs(&client, true, Some("c")).await.unwrap();
            assert_eq!(
                page,
                json!({ "jobs": [{ "id": "j" }], "next_cursor": null, "include_history": true })
            );
            assert_eq!(
                log.lock().unwrap()[0]["params"],
                json!({ "include_history": true, "cursor": "c" })
            );
        }
    }
}
