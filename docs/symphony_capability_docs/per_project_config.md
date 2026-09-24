# Per-Project Configuration (MOTHERSHIP.md)

## Overview

Each project directory can contain a `MOTHERSHIP.md` file that provides per-project settings and a
prompt template. The daemon discovers, parses, and watches these files automatically. Settings from
`MOTHERSHIP.md` override daemon-global defaults at session launch time, and the Markdown body is
rendered as a minijinja template and injected into the session's context pipeline.

## Architecture

The system has four main parts that work together:

1. **Parsing** — `parse_rsi_md()` splits YAML front matter from the Markdown body and
   deserializes `ProjectSettings`. Environment variable references in the YAML are expanded before
   deserialization.

2. **Caching** — `ProjectWorkflowCache` holds one `ProjectWorkflow` per project UUID in memory,
   protected by an `Arc<RwLock<>>` for concurrent access. The cache is populated synchronously at
   daemon startup (via `load_project_workflow()`) and kept up to date by the polling watcher.

3. **Watching** — `spawn_watcher()` runs a background tokio task that polls every 1 second using
   three-part fingerprints (mtime, file size, content hash). On a fingerprint mismatch the file is
   re-read and re-parsed. A parse failure does not evict the last-known-good entry — only the
   `last_error` field is updated and a `SystemMessage(warn)` bus event is published.

4. **Session launch integration** — `launch_session()` reads the cache, applies `ProjectSettings`
   overrides to the `LaunchConfig`, renders the template body into a `workflow_content` string, and
   passes that string to the `ContextPipeline` assembler.

## MOTHERSHIP.md Format

A `MOTHERSHIP.md` file is a standard Markdown file with an optional YAML front matter block delimited
by `---` fences. The front matter carries typed settings; everything after the closing fence is the
template body.

```
---
provider: Claude
model: claude-opus-4-20250514
session_kind: Standard
stall_timeout_secs: 300
rotation_depth_limit: 2
rotation_enabled: true
---
# Project workflow

You are working on {{ project.name }} ({{ project.path }}).
Current branch: {{ session.git_branch }}

{% if session.provider == 'Claude' %}
Use the extended thinking budget.
{% endif %}
```

If no front matter block is present, the entire file content is treated as the template body and
default `ProjectSettings` (all fields `None`) are used.

Environment variables are expanded in the YAML before deserialization using `$VAR` and `${VAR}`
syntax. References to unset variables expand to an empty string.

Unknown YAML fields are silently ignored for forward compatibility.

### ProjectSettings Fields

All fields are `Option<T>`. An absent field falls through to the daemon-global default.

| Field | Type | Purpose |
|---|---|---|
| `provider` | `Option<SessionProvider>` | Override default provider (`Claude`, `Codex`, `OpenCode`, `Local`, `Gemini`) |
| `model` | `Option<String>` | Override default model name for new sessions |
| `session_kind` | `Option<SessionKind>` | Override default session kind (`Standard`, `TaskRabbit`, `Bug`) |
| `stall_timeout_secs` | `Option<u64>` | Override the daemon-global stall timeout in seconds |
| `rotation_depth_limit` | `Option<u32>` | Override the context rotation depth limit (daemon global default is 4) |
| `rotation_enabled` | `Option<bool>` | Enable or disable context rotation for sessions in this project |

## Key Components

### ProjectWorkflow

`ProjectWorkflow` is the struct stored per project in the cache. Fields:

- `settings: ProjectSettings` — deserialized YAML front matter
- `template_body: String` — raw Markdown body after the front matter fence (minijinja template source)
- `fingerprint: FileFingerprint` — three-part fingerprint at the time the file was last successfully read
- `loaded_at: DateTime<Utc>` — timestamp of the last successful parse
- `last_error: Option<String>` — set to a non-`None` error message when the most recent parse attempt failed; `None` means healthy

### ProjectWorkflowCache

`ProjectWorkflowCache` is an in-memory `HashMap<Uuid, ProjectWorkflow>` keyed by project UUID.
It is wrapped in `Arc<RwLock<ProjectWorkflowCache>>` inside `SessionManager` so the polling
watcher, RPC handlers, and session launch code can all access it concurrently.

Public methods:

- `get(&Uuid) -> Option<&ProjectWorkflow>` — lookup by project ID
- `insert(Uuid, ProjectWorkflow)` — insert or replace an entry
- `remove(&Uuid)` — evict an entry (called when a project is deleted or its path is cleared)
- `set_error(&Uuid, String)` — record a parse error on an existing entry without evicting the
  last-known-good settings and template body

### File Parsing

`parse_rsi_md(content: &str) -> Result<(ProjectSettings, String), String>`

The function calls `split_front_matter()` to locate the YAML block. `split_front_matter()` requires
the file to start with `---`, then searches for `\n---` to find the closing fence. The YAML string
between the fences is passed through `expand_env_vars()` before being fed to `serde_yaml::from_str`.
The Markdown body is the content after the closing fence with leading newlines stripped.

When no front matter is found, `ProjectSettings::default()` is returned and the entire file content
becomes the template body.

### Template Rendering

`render_template(template_body: &str, ctx: &TemplateContext) -> Result<String, String>`

Uses `minijinja::Environment::render_str()`. Undefined variable references produce an error (strict
mode). The `TemplateContext` struct contains three top-level namespaces:

- `project` (`ProjectTemplateVars`) — `name`, `path`, `description`
- `session` (`SessionTemplateVars`) — `query`, `working_dir`, `git_branch`, `model`, `provider`,
  `kind`, `rotation_depth`
- `env` (`HashMap<String, String>`) — the full process environment at session launch time

The `git_branch` value is resolved by running `git rev-parse --abbrev-ref HEAD` in the session's
`working_dir` at launch time.

### File Watching

`spawn_watcher()` starts a tokio task with a 1-second `tokio::time::interval`. On each tick it:

1. Calls `projects_provider()` (a closure that performs a blocking SQLite read) to get the current
   `Vec<(Uuid, PathBuf)>` of all projects.
2. Removes fingerprints for projects that no longer exist.
3. For each project, checks whether `<project_path>/MOTHERSHIP.md` exists.
   - If the file was previously tracked but is now missing, the cache entry is removed.
   - If the file exists, `compute_fingerprint()` is called. A match means no action.
   - On a mismatch the file is re-read and re-parsed.
4. A successful parse updates the cache entry and logs `info`.
5. A failed parse calls `cache.set_error()` to preserve the last-known-good entry, updates the
   fingerprint (to avoid re-trying every second on a broken file), and publishes a
   `DaemonEvent::SystemMessage { level: "warn", ... }` bus event.

`FileFingerprint` compares three parts: `mtime_secs` (seconds since Unix epoch), `size` (file size
in bytes), and `content_hash` (a `DefaultHasher` hash of the raw file bytes). All three must match
for a file to be considered unchanged.

## Session Launch Integration

Inside `SessionManager::launch_session()`, after the project ID is resolved via the
`ProjectIndex`, the workflow cache is consulted at two points:

**Step 1 — Settings override** (lines 68–82 of `launch.rs`):

```rust
if let Some(pid) = resolved_project_id {
    let cache = self.workflow_config_cache.read().await;
    if let Some(wf) = cache.get(&pid) {
        let ps = &wf.settings;
        if config.provider.is_none() { config.provider = ps.provider; }
        if config.model.is_none()    { config.model = ps.model.clone(); }
        if config.session_kind.is_none() { config.session_kind = ps.session_kind; }
    }
}
```

Explicit values in the incoming `LaunchConfig` always win. Only absent (`None`) fields are filled
from `ProjectSettings`. Note that `stall_timeout_secs`, `rotation_depth_limit`, and
`rotation_enabled` are parsed and stored in the cache but are not yet applied in this code path
(they are available for future use).

**Step 2 — Template rendering** (lines 87–144 of `launch.rs`):

If the project has a workflow entry with a non-empty `template_body`, `render_template()` is called
with a `TemplateContext` built from the resolved project data, the (potentially overridden) session
config, and the full process environment. The rendered string is passed to
`ContextPipeline::assemble()` as the `workflow_content` argument. A render failure logs a warning
and skips the injection — it does not abort the launch.

## RPC Methods

### GetProjectWorkflow

**Method**: `GetProjectWorkflow`
**Params**: `{ "id": "<project-uuid>" }`
**Response** (when MOTHERSHIP.md exists):
```json
{
  "exists": true,
  "healthy": true,
  "last_error": null,
  "loaded_at": "2026-03-29T12:34:56.789Z",
  "settings": { "provider": "Claude", "model": "opus", ... },
  "template_length": 512
}
```
**Response** (when no MOTHERSHIP.md):
```json
{ "exists": false, "healthy": true, "last_error": null }
```

The handler acquires a read lock on `workflow_config_cache`. The `template_length` field is the byte
length of the raw template body string, not the rendered output.

### ReloadProjectWorkflow

**Method**: `ReloadProjectWorkflow`
**Params**: `{ "id": "<project-uuid>" }`
**Response**: `{ "success": true }`

Forces an immediate re-read and re-parse of the project's `MOTHERSHIP.md` by calling
`load_project_workflow()` under a write lock. Useful after manually editing `MOTHERSHIP.md` without
waiting for the 1-second polling interval. Returns an RPC error if the project ID does not exist
in the database or the project has no path set.

## TUI Integration

### workflow_statuses Cache

`App.workflow_statuses: HashMap<Uuid, serde_json::Value>` (defined in
`crates/rsi/src/app/mod.rs`) caches the raw JSON responses from `GetProjectWorkflow`, keyed
by project UUID. The map starts empty and is populated lazily.

### Project Form Status Display

When `open_project_form_for_edit()` is called in `crates/rsi/src/overlay/project_form.rs`, it
calls `app.client.get_project_workflow(project_id)` after setting up the overlay. On success the
result is stored in `app.workflow_statuses`. Failures are silently ignored (best-effort).

The renderer `render_project_form()` in `crates/rsi/src/ui/overlay/project_form.rs` checks
`workflow_statuses` for the current `editing_id`. If the project has a MOTHERSHIP.md entry:

- **Healthy**: renders `FLYWHEEL: loaded <timestamp>` in green, with the timestamp truncated to
  `YYYY-MM-DDTHH:MM:SS` (first 19 characters).
- **Unhealthy** (`last_error` is set): renders `FLYWHEEL: <first 40 chars of error>...` in yellow.

The popup height increases from 12 to 13 rows when a workflow status is present to accommodate the
extra line.

## Source Files

| File | Role |
|---|---|
| `crates/rsid/src/project_workflow.rs` | `ProjectSettings`, `ProjectWorkflow`, `ProjectWorkflowCache`, `FileFingerprint`, `TemplateContext`, `parse_rsi_md()`, `render_template()`, `compute_fingerprint()`, `load_project_workflow()`, `spawn_watcher()` |
| `crates/rsid/src/session/mod.rs` | `SessionManager` field `workflow_config_cache`, initialization at startup, watcher spawn, `workflow_config_cache()` accessor |
| `crates/rsid/src/session/launch.rs` | Settings override and template rendering at session launch (`launch_session()`) |
| `crates/rsid/src/session/projects.rs` | Cache invalidation on `create_project()`, `update_project()`, `delete_project()` |
| `crates/rsid/src/rpc.rs` | `handle_get_project_workflow()`, `handle_reload_project_workflow()` RPC handlers |
| `crates/rsi/src/client.rs` | `get_project_workflow()`, `reload_project_workflow()` TUI client methods |
| `crates/rsi/src/app/mod.rs` | `App.workflow_statuses` field definition and initialization |
| `crates/rsi/src/overlay/project_form.rs` | Fetches and stores workflow status when opening edit form |
| `crates/rsi/src/ui/overlay/project_form.rs` | Renders MOTHERSHIP.md status line in the project form popup |
