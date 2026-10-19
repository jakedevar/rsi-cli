//! The per-login live Codex model catalog (#2752).
//!
//! `codex debug models --bundled` lists the models a Codex *binary* ships with,
//! not the models the signed-in ChatGPT login can use. The server answers a
//! model the login lost with a 400 (#2735 types it after the fact). This module
//! reads the list the login can actually use: the Codex app-server `model/list`
//! request over stdio (no model turn), falling back to the non-bundled
//! `codex debug models`.
//!
//! * One probe is in flight per Codex binary (the per-binary cell's async lock
//!   is held across the probe); a result is cached for the operator setting
//!   `codex_live_catalog_ttl_secs` (0 = bundled only).
//! * A failed probe (spawn, timeout, exit, 401, malformed, empty) fails open:
//!   the view reports `Bundled` (or `Stale` when an older live list exists),
//!   `degraded`, and a reason code. Only a *fresh* `Live` view may refuse a
//!   launch; a failed probe never does.
//! * A `model_unavailable` stop reason calls [`request_refresh`], which makes
//!   the next lookup of every cell probe once regardless of the TTL.
//! * The probe uses `scrub_credential_env` and inherits `CODEX_HOME` exactly
//!   like a launch, so it reads the same login.
//! * Each model's `supportedReasoningEfforts` is kept (#2768): the ladders of
//!   the last good probe per binary are readable synchronously
//!   ([`known_effort_ladder`]), so the launch validates the reasoning effort
//!   against the login's own ladder and falls back to the compiled table for a
//!   model the live list does not describe.

use crate::process_control::{
    BoundedLines, CaptureLimits, PROVIDER_MAX_LINE_BYTES, ProcessContainment, capture_bounded,
    configure_tokio_process_group, terminate_process_group,
};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex as StdMutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::Mutex;

/// Reason codes a degraded view carries (stable strings).
pub(crate) const REASON_SPAWN_FAILED: &str = "probe_spawn_failed";
pub(crate) const REASON_TIMEOUT: &str = "probe_timeout";
pub(crate) const REASON_EXITED: &str = "probe_exited";
pub(crate) const REASON_UNAUTHORIZED: &str = "probe_unauthorized";
pub(crate) const REASON_RPC_ERROR: &str = "probe_rpc_error";
pub(crate) const REASON_MALFORMED: &str = "probe_malformed";
pub(crate) const REASON_EMPTY: &str = "live_catalog_empty";

/// Wall time of one app-server probe.
const APP_SERVER_PROBE_TIMEOUT: Duration = Duration::from_secs(8);
/// A failed probe is not repeated sooner than this (capped by the TTL), so a
/// broken CLI or a logged-out host costs one probe per window, not one per launch.
const FAILURE_BACKOFF_SECS: i64 = 30;
/// Pages of `model/list` read at most.
const MAX_PAGES: usize = 10;
/// Bound on one effort token taken from a live list.
const MAX_EFFORT_TOKEN_BYTES: usize = 32;

/// Where a discovery result came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CatalogSource {
    /// A fresh per-login list.
    Live,
    /// The bundled catalog (live disabled, or no live list yet).
    Bundled,
    /// An older live list kept because the refresh failed.
    Stale,
}

impl CatalogSource {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Bundled => "bundled",
            Self::Stale => "stale",
        }
    }
}

/// One model in the live list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LiveModel {
    pub(crate) id: String,
    pub(crate) display_name: Option<String>,
    pub(crate) hidden: bool,
    /// The model's `supportedReasoningEfforts` (#2768). `None` when the entry
    /// does not carry the field (its ladder stays the compiled one); an empty
    /// list means the model takes no reasoning effort.
    pub(crate) efforts: Option<Vec<String>>,
    /// The model's `defaultReasoningEffort`.
    pub(crate) default_effort: Option<String>,
}

/// What a lookup knows about this login's model list.
#[derive(Debug, Clone)]
pub(crate) struct CatalogView {
    pub(crate) source: CatalogSource,
    pub(crate) fetched_at: Option<DateTime<Utc>>,
    pub(crate) degraded: bool,
    pub(crate) degraded_reason: Option<String>,
    /// The live list; empty for a `Bundled` view.
    pub(crate) models: Vec<LiveModel>,
}

impl CatalogView {
    /// The bundled catalog, not degraded: live lookup is switched off.
    #[cfg(test)]
    pub(crate) fn disabled() -> Self {
        Self::bundled(None)
    }

    fn bundled(degraded_reason: Option<String>) -> Self {
        Self {
            source: CatalogSource::Bundled,
            fetched_at: None,
            degraded: degraded_reason.is_some(),
            degraded_reason,
            models: Vec::new(),
        }
    }

    /// The live ladders as `DiscoverModels` entries (#2768): one per model
    /// whose live entry carried `supportedReasoningEfforts`. Empty for a
    /// `Bundled` view; a `Stale` view still knows its login's ladders.
    pub(crate) fn effort_capabilities(
        &self,
    ) -> Vec<rsi_common::model_utils::ModelEffortCapabilities> {
        if self.source == CatalogSource::Bundled {
            return Vec::new();
        }
        self.models
            .iter()
            .filter_map(|model| {
                Some(rsi_common::model_utils::ModelEffortCapabilities {
                    model: model.id.clone(),
                    supported_efforts: model.efforts.clone()?,
                    default_effort: model.default_effort.clone(),
                })
            })
            .collect()
    }

    /// Whether `model` is in the live list. `None` unless this is a fresh
    /// `Live` view: bundled and stale views never decide availability.
    pub(crate) fn live_contains(&self, model: &str) -> Option<bool> {
        if self.source != CatalogSource::Live {
            return None;
        }
        let wanted = model.trim();
        Some(
            self.models
                .iter()
                .any(|live| live.id.eq_ignore_ascii_case(wanted)),
        )
    }
}

#[derive(Default)]
struct CellState {
    ok: Option<(Vec<LiveModel>, DateTime<Utc>)>,
    fail: Option<(String, DateTime<Utc>)>,
    seen_refresh: u64,
}

type Cell = Arc<Mutex<CellState>>;

static CELLS: LazyLock<StdMutex<HashMap<PathBuf, Cell>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));
/// The ladders of the last good probe per binary (lowercased model id to
/// ladder), readable from the synchronous launch path (#2768).
static LADDERS: LazyLock<StdMutex<HashMap<PathBuf, HashMap<String, Vec<String>>>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

/// The reasoning-effort ladder the last good live list of `binary` reported
/// for `model`, or `None` when no live list described it (live lookup off, no
/// probe yet, or the entry carried no ladder): the caller then uses the
/// compiled table.
pub(crate) fn known_effort_ladder(binary: &Path, model: &str) -> Option<Vec<String>> {
    LADDERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(binary)?
        .get(&model.trim().to_ascii_lowercase())
        .cloned()
}

/// Replace `binary`'s ladders with those of `models` (`None` forgets them).
fn record_ladders(binary: &Path, models: Option<&[LiveModel]>) {
    let mut ladders = LADDERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(models) = models else {
        ladders.remove(binary);
        return;
    };
    let index = models
        .iter()
        .filter_map(|model| Some((model.id.to_ascii_lowercase(), model.efforts.clone()?)))
        .collect();
    ladders.insert(binary.to_path_buf(), index);
}
static REFRESH_GENERATION: AtomicU64 = AtomicU64::new(0);

fn cell_for(binary: &Path) -> Cell {
    let mut cells = CELLS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Arc::clone(cells.entry(binary.to_path_buf()).or_default())
}

/// A provider `model_unavailable` refusal was just observed: the next lookup of
/// every live catalog probes once, whatever the TTL says.
pub(crate) fn request_refresh() {
    REFRESH_GENERATION.fetch_add(1, Ordering::SeqCst);
}

/// The live view for `binary`: cached for `ttl_secs`, at most one probe in
/// flight, fail-open. `ttl_secs == 0` is the bundled catalog only.
pub(crate) async fn live_view(binary: &Path, ttl_secs: u32) -> CatalogView {
    live_view_with_timeout(binary, ttl_secs, APP_SERVER_PROBE_TIMEOUT).await
}

pub(crate) async fn live_view_with_timeout(
    binary: &Path,
    ttl_secs: u32,
    probe_timeout: Duration,
) -> CatalogView {
    live_view_gated(
        binary,
        ttl_secs,
        probe_timeout,
        crate::offline::is_offline(),
    )
    .await
}

/// [`live_view_with_timeout`] with the offline mode (#130) injected: offline,
/// no probe starts and the view is the bundled catalog, degraded
/// [`crate::offline::REASON_OFFLINE`] (fail-open: it never refuses a model).
pub(crate) async fn live_view_gated(
    binary: &Path,
    ttl_secs: u32,
    probe_timeout: Duration,
    offline: bool,
) -> CatalogView {
    if offline {
        record_ladders(binary, None);
        return CatalogView::bundled(Some(crate::offline::REASON_OFFLINE.to_string()));
    }
    if ttl_secs == 0 {
        // Bundled only: a ladder from an earlier live list no longer decides.
        record_ladders(binary, None);
        return CatalogView::bundled(None);
    }
    let cell = cell_for(binary);
    let mut state = cell.lock().await;
    let now = Utc::now();
    let ttl = chrono::Duration::seconds(i64::from(ttl_secs));
    let generation = REFRESH_GENERATION.load(Ordering::SeqCst);
    let forced = generation > state.seen_refresh;
    if !forced {
        if let Some((models, fetched_at)) = &state.ok
            && now.signed_duration_since(*fetched_at) < ttl
        {
            return live_from(models, *fetched_at);
        }
        if let Some((reason, failed_at)) = &state.fail
            && now.signed_duration_since(*failed_at)
                < ttl.min(chrono::Duration::seconds(FAILURE_BACKOFF_SECS))
        {
            return fallback_view(&state, reason.clone());
        }
    }
    state.seen_refresh = generation;
    match probe(binary, probe_timeout).await {
        Ok(models) => {
            let fetched_at = Utc::now();
            let view = live_from(&models, fetched_at);
            record_ladders(binary, Some(&models));
            state.ok = Some((models, fetched_at));
            state.fail = None;
            view
        }
        Err(reason) => {
            tracing::warn!(
                reason,
                "Codex live model catalog probe failed; falling back to the bundled catalog"
            );
            state.fail = Some((reason.to_string(), Utc::now()));
            fallback_view(&state, reason.to_string())
        }
    }
}

fn live_from(models: &[LiveModel], fetched_at: DateTime<Utc>) -> CatalogView {
    CatalogView {
        source: CatalogSource::Live,
        fetched_at: Some(fetched_at),
        degraded: false,
        degraded_reason: None,
        models: models.to_vec(),
    }
}

fn fallback_view(state: &CellState, reason: String) -> CatalogView {
    match &state.ok {
        Some((models, fetched_at)) => CatalogView {
            source: CatalogSource::Stale,
            fetched_at: Some(*fetched_at),
            degraded: true,
            degraded_reason: Some(reason),
            models: models.clone(),
        },
        None => CatalogView::bundled(Some(reason)),
    }
}

/// Age the cached live list of `binary` so the next lookup probes (tests only).
#[cfg(test)]
pub(crate) async fn expire_for_test(binary: &Path) {
    let cell = cell_for(binary);
    let mut state = cell.lock().await;
    let old = Utc::now() - chrono::Duration::days(2);
    if let Some((_, fetched_at)) = state.ok.as_mut() {
        *fetched_at = old;
    }
    state.fail = None;
}

async fn probe(binary: &Path, timeout: Duration) -> Result<Vec<LiveModel>, &'static str> {
    match probe_app_server(binary, timeout, drive_model_list).await {
        Ok(models) => Ok(models),
        // A rejected login or a hung server will not answer `debug models`
        // either; do not hold the cell for a second probe.
        Err(reason @ (REASON_UNAUTHORIZED | REASON_TIMEOUT | REASON_SPAWN_FAILED)) => Err(reason),
        Err(reason) => probe_debug_models(binary).await.map_err(|_| reason),
    }
}

pub(crate) fn probe_command(binary: &Path) -> Command {
    let mut command = Command::new(binary);
    command.env(
        rsi_common::identity::ENV_PROCESS_OWNERSHIP_NAMESPACE,
        rsi_common::identity::process_ownership_namespace(),
    );
    // Probes need no provider credential (#694 K1); `CODEX_HOME` is untouched,
    // so the probe reads the same login a launch does.
    crate::vault::scrub_credential_env(&mut command);
    command
}

async fn probe_debug_models(binary: &Path) -> Result<Vec<LiveModel>, &'static str> {
    let mut command = probe_command(binary);
    command.args(["debug", "models"]);
    let output = capture_bounded(
        command,
        CaptureLimits::catalog(),
        &tokio_util::sync::CancellationToken::new(),
    )
    .await
    .map_err(|_| REASON_EXITED)?;
    if !output.status.success() {
        return Err(REASON_EXITED);
    }
    let snapshot = crate::provider_capabilities::parse_codex_catalog_snapshot(
        "live-catalog",
        &output.stdout,
        Utc::now(),
    )
    .map_err(|_| REASON_MALFORMED)?;
    let models: Vec<LiveModel> = snapshot
        .models
        .iter()
        .map(|model| LiveModel {
            id: model.slug.clone(),
            display_name: model.display_name.clone(),
            hidden: model.visibility.as_deref() != Some("list"),
            // Same rule as the bundled catalog: a missing field keeps the
            // compiled ladder, an explicit empty list is no effort.
            efforts: model
                .raw_keys
                .contains("supported_reasoning_levels")
                .then(|| {
                    model
                        .supported_reasoning_levels
                        .iter()
                        .filter_map(|level| effort_token(&level.effort))
                        .collect()
                }),
            default_effort: model
                .default_reasoning_level
                .as_deref()
                .and_then(effort_token),
        })
        .collect();
    if models.is_empty() {
        return Err(REASON_EMPTY);
    }
    Ok(models)
}

pub(crate) async fn probe_app_server<T, F, Fut>(
    binary: &Path,
    timeout: Duration,
    drive: F,
) -> Result<T, &'static str>
where
    F: FnOnce(tokio::process::ChildStdin, tokio::process::ChildStdout) -> Fut,
    Fut: std::future::Future<Output = Result<T, &'static str>>,
{
    let mut command = probe_command(binary);
    command
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    configure_tokio_process_group(&mut command, ProcessContainment::Group)
        .map_err(|_| REASON_SPAWN_FAILED)?;
    let mut child = command.spawn().map_err(|_| REASON_SPAWN_FAILED)?;
    let pgid = child
        .id()
        .and_then(|pid| i32::try_from(pid).ok())
        .map(nix::unistd::Pid::from_raw);
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let result = match (stdin, stdout) {
        (Some(stdin), Some(stdout)) => {
            match tokio::time::timeout(timeout, drive(stdin, stdout)).await {
                Ok(result) => result,
                Err(_) => Err(REASON_TIMEOUT),
            }
        }
        _ => Err(REASON_SPAWN_FAILED),
    };
    // The app-server is a long-lived stdio server: stop its whole group.
    if let Some(pgid) = pgid {
        terminate_process_group(pgid);
    }
    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    result
}

pub(crate) async fn send(
    stdin: &mut tokio::process::ChildStdin,
    message: Value,
) -> Result<(), &'static str> {
    let mut line = message.to_string();
    line.push('\n');
    stdin
        .write_all(line.as_bytes())
        .await
        .map_err(|_| REASON_EXITED)?;
    stdin.flush().await.map_err(|_| REASON_EXITED)
}

/// Read lines until the response to request `id` (a message with that `id` and
/// no `method`); notifications and server requests are skipped.
pub(crate) async fn response_for(
    lines: &mut BoundedLines<tokio::process::ChildStdout>,
    id: u64,
) -> Result<Value, &'static str> {
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) | Err(_) => return Err(REASON_EXITED),
        };
        let Ok(message) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if message.get("method").is_none() && message.get("id").and_then(Value::as_u64) == Some(id)
        {
            return Ok(message);
        }
    }
}

pub(crate) fn rpc_error_reason(error: &Value) -> &'static str {
    let text = error.to_string().to_ascii_lowercase();
    if text.contains("401") || text.contains("unauthor") || text.contains("not logged in") {
        REASON_UNAUTHORIZED
    } else {
        REASON_RPC_ERROR
    }
}

async fn drive_model_list(
    mut stdin: tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
) -> Result<Vec<LiveModel>, &'static str> {
    let mut lines = BoundedLines::new(stdout, PROVIDER_MAX_LINE_BYTES);
    send(
        &mut stdin,
        json!({
            "id": 1,
            "method": "initialize",
            "params": {"clientInfo": {"name": "rsid", "version": env!("CARGO_PKG_VERSION")}},
        }),
    )
    .await?;
    let init = response_for(&mut lines, 1).await?;
    if let Some(error) = init.get("error") {
        return Err(rpc_error_reason(error));
    }
    send(&mut stdin, json!({"method": "initialized"})).await?;

    let mut models = Vec::new();
    let mut cursor: Option<String> = None;
    for page in 0..MAX_PAGES {
        let id = 2 + page as u64;
        let mut params = json!({"includeHidden": true});
        if let Some(cursor) = &cursor {
            params["cursor"] = json!(cursor);
        }
        send(
            &mut stdin,
            json!({"id": id, "method": "model/list", "params": params}),
        )
        .await?;
        let response = response_for(&mut lines, id).await?;
        if let Some(error) = response.get("error") {
            return Err(rpc_error_reason(error));
        }
        let data = response
            .pointer("/result/data")
            .and_then(Value::as_array)
            .ok_or(REASON_MALFORMED)?;
        for entry in data {
            let Some(model) = entry
                .get("id")
                .or_else(|| entry.get("model"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|model| !model.is_empty())
            else {
                continue;
            };
            models.push(LiveModel {
                id: model.to_string(),
                display_name: entry
                    .get("displayName")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                hidden: entry
                    .get("hidden")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                efforts: live_efforts(entry),
                default_effort: entry
                    .get("defaultReasoningEffort")
                    .and_then(Value::as_str)
                    .and_then(effort_token),
            });
        }
        cursor = response
            .pointer("/result/nextCursor")
            .and_then(Value::as_str)
            .map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }
    if models.is_empty() {
        return Err(REASON_EMPTY);
    }
    Ok(models)
}

/// One `model/list` entry's `supportedReasoningEfforts`: each item is a
/// `ReasoningEffortOption` (`{reasoningEffort, description}`) or a bare
/// string. `None` when the entry has no such array.
fn live_efforts(entry: &Value) -> Option<Vec<String>> {
    let levels = entry.get("supportedReasoningEfforts")?.as_array()?;
    Some(
        levels
            .iter()
            .filter_map(|level| {
                level
                    .as_str()
                    .or_else(|| level.get("reasoningEffort").and_then(Value::as_str))
                    .and_then(effort_token)
            })
            .collect(),
    )
}

/// One effort token as it may reach `model_reasoning_effort="..."`: a short
/// lowercase word (a letter, then letters, digits, `-` or `_`).
fn effort_token(raw: &str) -> Option<String> {
    let token = raw.trim();
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

/// A stub `codex` whose `app-server` answers `model/list` from `models.json`
/// (rewritten by a test to change the login's list) and counts probes.
#[cfg(test)]
pub(crate) struct StubCodex {
    dir: tempfile::TempDir,
}

#[cfg(test)]
impl StubCodex {
    /// `app_server` is the shell body run for `codex app-server`.
    pub(crate) fn with_app_server(app_server: &str) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("codex");
        let script = format!(
            r#"#!/bin/sh
case "$1" in
  --version) echo "codex-cli 0.0.0-stub" ;;
  app-server)
    echo probe >> "{dir}/probes"
    {app_server}
    ;;
  debug) exit 1 ;;
  *) exit 1 ;;
esac
"#,
            dir = dir.path().display()
        );
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir }
    }

    /// A healthy stub: the login lists exactly `models`.
    pub(crate) fn listing(models: &[&str]) -> Self {
        let stub = Self::with_app_server(Self::ANSWERING_BODY);
        stub.set_models(models).unwrap();
        stub
    }

    pub(crate) fn path(&self) -> PathBuf {
        self.dir.path().join("codex")
    }

    /// Replace the list the stub answers with.
    pub(crate) fn set_models(&self, models: &[&str]) -> std::io::Result<()> {
        let data: Vec<Value> = models
            .iter()
            .map(|id| json!({"id": id, "model": id, "displayName": id, "hidden": false}))
            .collect();
        std::fs::write(
            self.dir.path().join("models.json"),
            json!({"id": 2, "result": {"data": data}}).to_string(),
        )
    }

    /// Replace the list with raw `model/list` entries (ladders included).
    pub(crate) fn set_entries(&self, entries: &[Value]) -> std::io::Result<()> {
        std::fs::write(
            self.dir.path().join("models.json"),
            json!({"id": 2, "result": {"data": entries}}).to_string(),
        )
    }

    pub(crate) fn probes(&self) -> usize {
        std::fs::read_to_string(self.dir.path().join("probes"))
            .map(|text| text.lines().count())
            .unwrap_or(0)
    }

    /// Answers from the list file beside the stub (`$0`), read at answer time.
    const ANSWERING_BODY: &'static str = r#"d=$(dirname "$0")
    while IFS= read -r line; do
      case "$line" in
        *'"initialize"'*) echo '{"id":1,"result":{}}' ;;
        *'"model/list"'*) cat "$d/models.json" || exit 5; echo ;;
      esac
    done"#;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #2768: each live entry keeps its `supportedReasoningEfforts` (option
    /// objects or bare strings) and default; an entry without the field stays
    /// undescribed. The ladders are readable synchronously per binary and are
    /// forgotten once live lookup is switched off.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn live_ladders_are_kept_per_model_and_dropped_when_live_lookup_is_off() {
        let stub = StubCodex::listing(&[]);
        stub.set_entries(&[
            json!({"id": "gpt-9-sol", "displayName": "Sol", "hidden": false,
                   "supportedReasoningEfforts": [
                       {"reasoningEffort": "low", "description": "Fast"},
                       {"reasoningEffort": "max", "description": "Deep"},
                       {"reasoningEffort": "ultra", "description": "Deepest"}],
                   "defaultReasoningEffort": "max"}),
            json!({"id": "gpt-9-luna", "supportedReasoningEfforts": ["low", "medium", "high"]}),
            json!({"id": "gpt-9-mini", "supportedReasoningEfforts": []}),
            json!({"id": "gpt-9-old"}),
        ])
        .unwrap();
        let words = |levels: &[&str]| levels.iter().map(|l| l.to_string()).collect::<Vec<_>>();

        let view = live_view(&stub.path(), 300).await;
        assert_eq!(view.source, CatalogSource::Live);
        assert_eq!(
            view.effort_capabilities(),
            vec![
                rsi_common::model_utils::ModelEffortCapabilities {
                    model: "gpt-9-sol".into(),
                    supported_efforts: words(&["low", "max", "ultra"]),
                    default_effort: Some("max".into()),
                },
                rsi_common::model_utils::ModelEffortCapabilities {
                    model: "gpt-9-luna".into(),
                    supported_efforts: words(&["low", "medium", "high"]),
                    default_effort: None,
                },
                rsi_common::model_utils::ModelEffortCapabilities {
                    model: "gpt-9-mini".into(),
                    supported_efforts: Vec::new(),
                    default_effort: None,
                },
            ]
        );
        assert_eq!(
            known_effort_ladder(&stub.path(), "GPT-9-SOL"),
            Some(words(&["low", "max", "ultra"]))
        );
        assert_eq!(
            known_effort_ladder(&stub.path(), "gpt-9-mini"),
            Some(Vec::new())
        );
        assert_eq!(known_effort_ladder(&stub.path(), "gpt-9-old"), None);
        assert_eq!(known_effort_ladder(&stub.path(), "gpt-6-astra"), None);

        let off = live_view(&stub.path(), 0).await;
        assert_eq!(off.source, CatalogSource::Bundled);
        assert_eq!(off.effort_capabilities(), Vec::new());
        assert_eq!(known_effort_ladder(&stub.path(), "gpt-9-sol"), None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn a_live_list_omitting_a_model_marks_it_absent_and_is_cached_for_the_ttl() {
        let stub = StubCodex::listing(&["gpt-6-sol", "gpt-6-luna"]);
        let before = REFRESH_GENERATION.load(Ordering::SeqCst);
        let view = live_view(&stub.path(), 300).await;
        assert_eq!(view.source, CatalogSource::Live);
        assert!(!view.degraded);
        assert!(view.fetched_at.is_some());
        assert_eq!(view.live_contains("gpt-6-sol"), Some(true));
        assert_eq!(view.live_contains("GPT-6-LUNA"), Some(true));
        assert_eq!(view.live_contains("gpt-6-astra"), Some(false));
        let again = live_view(&stub.path(), 300).await;
        assert_eq!(again.fetched_at, view.fetched_at);
        if REFRESH_GENERATION.load(Ordering::SeqCst) == before {
            assert_eq!(
                stub.probes(),
                1,
                "the second lookup is served from the cache"
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn a_later_live_list_that_contains_the_model_reopens_it_with_no_operator_action() {
        let stub = StubCodex::listing(&["gpt-6-sol"]);
        let first = live_view(&stub.path(), 300).await;
        assert_eq!(first.live_contains("gpt-6-astra"), Some(false));
        stub.set_models(&["gpt-6-sol", "gpt-6-astra"]).unwrap();
        // Still inside the TTL: the cached list decides.
        assert_eq!(
            live_view(&stub.path(), 300)
                .await
                .live_contains("gpt-6-astra"),
            Some(false)
        );
        expire_for_test(&stub.path()).await;
        let reopened = live_view(&stub.path(), 300).await;
        assert_eq!(reopened.source, CatalogSource::Live);
        assert_eq!(reopened.live_contains("gpt-6-astra"), Some(true));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn a_model_unavailable_stop_reason_forces_one_refresh_inside_the_ttl() {
        let stub = StubCodex::listing(&["gpt-6-sol", "gpt-6-astra"]);
        assert_eq!(
            live_view(&stub.path(), 300)
                .await
                .live_contains("gpt-6-astra"),
            Some(true)
        );
        stub.set_models(&["gpt-6-sol"]).unwrap();
        request_refresh();
        let refreshed = live_view(&stub.path(), 300).await;
        assert_eq!(refreshed.live_contains("gpt-6-astra"), Some(false));
        let probes = stub.probes();
        assert!(probes >= 2, "the refresh probed again ({probes})");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn ttl_zero_reads_only_the_bundled_catalog_and_never_probes() {
        let stub = StubCodex::listing(&["gpt-6-sol"]);
        let view = live_view(&stub.path(), 0).await;
        assert_eq!(view.source, CatalogSource::Bundled);
        assert!(!view.degraded);
        assert_eq!(view.live_contains("gpt-6-astra"), None);
        assert_eq!(stub.probes(), 0);
    }

    /// #130: an offline daemon never starts the `model/list` probe; the view
    /// is the bundled catalog marked degraded `offline`, which never refuses.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn an_offline_daemon_reads_the_bundled_catalog_and_never_probes() {
        let stub = StubCodex::listing(&["gpt-6-sol"]);
        let view = live_view_gated(&stub.path(), 300, APP_SERVER_PROBE_TIMEOUT, true).await;
        assert_eq!(view.source, CatalogSource::Bundled);
        assert!(view.degraded);
        assert_eq!(
            view.degraded_reason.as_deref(),
            Some(crate::offline::REASON_OFFLINE)
        );
        assert_eq!(view.live_contains("gpt-6-astra"), None);
        assert_eq!(stub.probes(), 0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn a_failed_probe_fails_open_to_the_bundled_catalog_with_a_reason() {
        // The CLI exits non-zero.
        let exits = StubCodex::with_app_server("exit 3");
        let view = live_view(&exits.path(), 300).await;
        assert_eq!(view.source, CatalogSource::Bundled);
        assert!(view.degraded);
        assert_eq!(view.degraded_reason.as_deref(), Some(REASON_EXITED));
        assert_eq!(
            view.live_contains("gpt-6-astra"),
            None,
            "a failure never refuses"
        );

        // The server answers 401.
        let unauthorized = StubCodex::with_app_server(
            r#"while IFS= read -r line; do
      case "$line" in
        *'"initialize"'*) echo '{"id":1,"result":{}}' ;;
        *'"model/list"'*) echo '{"id":2,"error":{"code":-32001,"message":"401 Unauthorized"}}' ;;
      esac
    done"#,
        );
        let view = live_view(&unauthorized.path(), 300).await;
        assert_eq!(view.source, CatalogSource::Bundled);
        assert!(view.degraded);
        assert_eq!(view.degraded_reason.as_deref(), Some(REASON_UNAUTHORIZED));

        // The server never answers.
        let hangs = StubCodex::with_app_server("sleep 30");
        let started = std::time::Instant::now();
        let view = live_view_with_timeout(&hangs.path(), 300, Duration::from_millis(600)).await;
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(view.source, CatalogSource::Bundled);
        assert_eq!(view.degraded_reason.as_deref(), Some(REASON_TIMEOUT));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn a_failed_refresh_keeps_the_older_live_list_as_stale_and_never_refuses() {
        let stub = StubCodex::listing(&["gpt-6-sol"]);
        assert_eq!(
            live_view(&stub.path(), 300).await.source,
            CatalogSource::Live
        );
        // The CLI breaks after the first good answer.
        std::fs::remove_file(stub.path().parent().unwrap().join("models.json")).unwrap();
        expire_for_test(&stub.path()).await;
        let view = live_view(&stub.path(), 300).await;
        assert_eq!(view.source, CatalogSource::Stale);
        assert!(view.degraded);
        assert!(view.fetched_at.is_some());
        assert_eq!(view.live_contains("gpt-6-astra"), None);
    }
}
