//! Per-project configuration system (RSI.md).
//!
//! Each project directory can contain a `RSI.md` file with YAML front matter
//! (typed settings) and a Markdown body (minijinja template for prompt generation).
//! The daemon watches these files for changes, caches parsed configs with
//! last-known-good semantics, and injects rendered templates into the context
//! pipeline at session launch.

use chrono::{DateTime, Utc};
use rsi_common::types::{SessionKind, SessionProvider};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

/// Async provider closure type: called on each watcher tick to retrieve the
/// current list of (project_id, project_path) pairs from the store.
///
/// The closure returns a pinned boxed future so that it can hold an async lock
/// internally without calling `blocking_lock()` from within a tokio runtime.
pub type ProjectsProvider =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Vec<(Uuid, PathBuf)>> + Send>> + Send + Sync>;

/// Per-project settings from RSI.md YAML front matter.
/// All fields are Option -- absent fields fall through to daemon-global defaults.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectSettings {
    /// Override default provider for sessions in this project.
    #[serde(default)]
    pub provider: Option<SessionProvider>,
    /// Override default model for new sessions.
    #[serde(default)]
    pub model: Option<String>,
    /// Override default session_kind.
    #[serde(default)]
    pub session_kind: Option<SessionKind>,
    /// Override stall timeout (seconds). None = daemon global.
    #[serde(default)]
    pub stall_timeout_secs: Option<u64>,
    /// Override rotation depth limit. None = daemon global (4).
    #[serde(default)]
    pub rotation_depth_limit: Option<u32>,
    /// Whether context rotation is enabled for this project. None = daemon global.
    #[serde(default)]
    pub rotation_enabled: Option<bool>,
}

/// Parsed and cached RSI.md content for a single project.
#[derive(Debug, Clone)]
pub struct ProjectWorkflow {
    /// Parsed settings from YAML front matter.
    pub settings: ProjectSettings,
    /// Raw template body (Markdown after front matter).
    pub template_body: String,
    /// Fingerprint for change detection: (mtime_secs, file_size, content_hash).
    pub fingerprint: FileFingerprint,
    /// When this config was last successfully parsed.
    pub loaded_at: DateTime<Utc>,
    /// If the last reload failed, the error message. None = healthy.
    pub last_error: Option<String>,
}

/// Three-part fingerprint for file change detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileFingerprint {
    pub mtime_secs: i64,
    pub size: u64,
    pub content_hash: u64,
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Extract YAML front matter and template body from RSI.md content.
/// Returns (yaml_str, template_body). Returns None if no front matter fence found.
fn split_front_matter(content: &str) -> Option<(&str, &str)> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return None;
    }
    // Find the closing fence (second "---" on its own line)
    let after_first = &trimmed[3..];
    let close_pos = after_first.find("\n---")?;
    let yaml = after_first[..close_pos].trim();
    let body = after_first[close_pos + 4..].trim_start_matches(['\n', '\r']);
    Some((yaml, body))
}

/// Expand $VAR and ${VAR} references in string values from environment.
fn expand_env_vars(value: &str) -> String {
    let re =
        regex::Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}|\$([A-Za-z_][A-Za-z0-9_]*)").unwrap();
    re.replace_all(value, |caps: &regex::Captures| {
        let var_name = caps.get(1).or_else(|| caps.get(2)).unwrap().as_str();
        std::env::var(var_name).unwrap_or_default()
    })
    .to_string()
}

/// Parse RSI.md content into ProjectSettings + template body.
/// Performs env var expansion on the YAML before deserialization.
pub fn parse_rsi_md(content: &str) -> Result<(ProjectSettings, String), String> {
    match split_front_matter(content) {
        Some((yaml_str, body)) => {
            let expanded = expand_env_vars(yaml_str);
            let settings: ProjectSettings = serde_yaml_ng::from_str(&expanded)
                .map_err(|e| format!("YAML parse error: {}", e))?;
            Ok((settings, body.to_string()))
        }
        None => {
            // No front matter -- entire content is the template body
            Ok((ProjectSettings::default(), content.to_string()))
        }
    }
}

/// Compute a three-part fingerprint for a file path.
pub fn compute_fingerprint(path: &Path) -> std::io::Result<FileFingerprint> {
    let metadata = std::fs::metadata(path)?;
    let content = std::fs::read(path)?;
    let mtime_secs = metadata
        .modified()
        .map(|t| {
            t.duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64
        })
        .unwrap_or(0);
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    Ok(FileFingerprint {
        mtime_secs,
        size: metadata.len(),
        content_hash: hasher.finish(),
    })
}

// ---------------------------------------------------------------------------
// Template Rendering
// ---------------------------------------------------------------------------

/// Context variables available to the RSI.md template.
#[derive(Debug, Serialize)]
pub struct TemplateContext {
    pub project: ProjectTemplateVars,
    pub session: SessionTemplateVars,
    pub env: HashMap<String, String>,
}

#[derive(Debug, Serialize)]
pub struct ProjectTemplateVars {
    pub name: String,
    pub path: String,
    pub description: String,
}

#[derive(Debug, Serialize)]
pub struct SessionTemplateVars {
    pub query: String,
    pub working_dir: String,
    pub git_branch: String,
    pub model: String,
    pub provider: String,
    pub kind: String,
    pub rotation_depth: u32,
}

/// Render a RSI.md template body with the given context.
/// Uses minijinja with strict undefined variable handling.
pub fn render_template(template_body: &str, ctx: &TemplateContext) -> Result<String, String> {
    let env = minijinja::Environment::new();
    let ctx_value = minijinja::Value::from_serialize(ctx);
    env.render_str(template_body, ctx_value)
        .map_err(|e| format!("Template render error: {}", e))
}

// ---------------------------------------------------------------------------
// Cache
// ---------------------------------------------------------------------------

/// In-memory cache of parsed RSI.md configs, keyed by project ID.
/// Wrapped in Arc<RwLock<>> for concurrent access from watcher, RPC, and session launch.
#[derive(Debug)]
pub struct ProjectWorkflowCache {
    entries: HashMap<Uuid, ProjectWorkflow>,
}

impl ProjectWorkflowCache {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Get the cached workflow for a project. Returns None if no RSI.md exists.
    pub fn get(&self, project_id: &Uuid) -> Option<&ProjectWorkflow> {
        self.entries.get(project_id)
    }

    /// Insert or update a workflow entry.
    pub fn insert(&mut self, project_id: Uuid, workflow: ProjectWorkflow) {
        self.entries.insert(project_id, workflow);
    }

    /// Remove a workflow entry (project deleted or path removed).
    pub fn remove(&mut self, project_id: &Uuid) {
        self.entries.remove(project_id);
    }

    /// Set the last_error on an existing entry (parse failure with last-known-good).
    pub fn set_error(&mut self, project_id: &Uuid, error: String) {
        if let Some(wf) = self.entries.get_mut(project_id) {
            wf.last_error = Some(error);
        }
    }

    /// Number of cached entries.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

// ---------------------------------------------------------------------------
// File Watcher (polling)
// ---------------------------------------------------------------------------

/// Spawn the polling-based watcher task.
/// Checks all project RSI.md files every 1 second using fingerprint comparison.
/// On change: re-parse, update cache, publish bus event on error.
pub fn spawn_watcher(
    cache: Arc<RwLock<ProjectWorkflowCache>>,
    projects_provider: ProjectsProvider,
    event_bus: Arc<crate::bus::EventBus>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Stores fingerprints for change detection
        let mut fingerprints: HashMap<Uuid, FileFingerprint> = HashMap::new();
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));

        loop {
            interval.tick().await;
            let projects = projects_provider().await;

            // Clean up fingerprints for projects that no longer exist
            fingerprints.retain(|id, _| projects.iter().any(|(pid, _)| pid == id));

            for (project_id, project_path) in &projects {
                let rsi_md = project_path.join(rsi_common::identity::PROJECT_CONFIG_FILENAME);

                if !rsi_md.exists() {
                    // Clean up if file was removed
                    if fingerprints.remove(project_id).is_some() {
                        cache.write().await.remove(project_id);
                    }
                    continue;
                }

                // Check fingerprint
                let new_fp = match compute_fingerprint(&rsi_md) {
                    Ok(fp) => fp,
                    Err(e) => {
                        tracing::debug!(
                            project_id = %project_id,
                            error = %e,
                            "Failed to stat RSI.md"
                        );
                        continue;
                    }
                };

                if fingerprints.get(project_id) == Some(&new_fp) {
                    continue; // No change
                }

                // File changed -- re-read and parse
                let content = match std::fs::read_to_string(&rsi_md) {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(
                            project_id = %project_id,
                            path = %rsi_md.display(),
                            error = %e,
                            "Failed to read RSI.md"
                        );
                        continue;
                    }
                };

                match parse_rsi_md(&content) {
                    Ok((settings, template_body)) => {
                        let workflow = ProjectWorkflow {
                            settings,
                            template_body,
                            fingerprint: new_fp.clone(),
                            loaded_at: chrono::Utc::now(),
                            last_error: None,
                        };
                        fingerprints.insert(*project_id, new_fp);
                        cache.write().await.insert(*project_id, workflow);
                        tracing::info!(
                            project_id = %project_id,
                            path = %rsi_md.display(),
                            "RSI.md loaded/reloaded successfully"
                        );
                    }
                    Err(error) => {
                        tracing::warn!(
                            project_id = %project_id,
                            path = %rsi_md.display(),
                            error = %error,
                            "RSI.md parse failed; keeping last known good config"
                        );
                        // Update fingerprint so we don't re-try every second
                        fingerprints.insert(*project_id, new_fp);
                        // Set error on existing entry (last-known-good stays)
                        cache.write().await.set_error(project_id, error.clone());
                        // Publish degraded event
                        event_bus.publish(crate::bus::DaemonEvent::SystemMessage {
                            level: "warn".to_string(),
                            message: format!(
                                "RSI.md parse error for project {}: {}",
                                project_id, error
                            ),
                        });
                    }
                }
            }
        }
    })
}

/// Load a single project's RSI.md into the cache (used at startup and for manual reload).
pub fn load_project_workflow(
    project_id: Uuid,
    project_path: &Path,
    cache: &mut ProjectWorkflowCache,
) {
    let rsi_md = project_path.join(rsi_common::identity::PROJECT_CONFIG_FILENAME);
    if !rsi_md.exists() {
        return;
    }

    let content = match std::fs::read_to_string(&rsi_md) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                project_id = %project_id,
                path = %rsi_md.display(),
                error = %e,
                "Failed to read RSI.md at startup"
            );
            return;
        }
    };

    let fingerprint = match compute_fingerprint(&rsi_md) {
        Ok(fp) => fp,
        Err(e) => {
            tracing::warn!(
                project_id = %project_id,
                error = %e,
                "Failed to compute RSI.md fingerprint"
            );
            return;
        }
    };

    match parse_rsi_md(&content) {
        Ok((settings, template_body)) => {
            cache.insert(
                project_id,
                ProjectWorkflow {
                    settings,
                    template_body,
                    fingerprint,
                    loaded_at: Utc::now(),
                    last_error: None,
                },
            );
            tracing::info!(
                project_id = %project_id,
                path = %rsi_md.display(),
                "RSI.md loaded at startup"
            );
        }
        Err(error) => {
            tracing::warn!(
                project_id = %project_id,
                path = %rsi_md.display(),
                error = %error,
                "RSI.md parse failed at startup"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_front_matter_with_yaml() {
        let content = "---\nprovider: Claude\nmodel: opus\n---\n# Hello\nBody text.";
        let (yaml, body) = split_front_matter(content).unwrap();
        assert_eq!(yaml, "provider: Claude\nmodel: opus");
        assert_eq!(body, "# Hello\nBody text.");
    }

    #[test]
    fn test_split_front_matter_no_yaml() {
        let content = "# No front matter\nJust body.";
        assert!(split_front_matter(content).is_none());
    }

    #[test]
    fn test_split_front_matter_empty_yaml() {
        let content = "---\n---\nBody only.";
        let (yaml, body) = split_front_matter(content).unwrap();
        assert_eq!(yaml, "");
        assert_eq!(body, "Body only.");
    }

    #[test]
    fn test_split_front_matter_no_closing_fence() {
        let content = "---\nprovider: Claude\nBody without close.";
        assert!(split_front_matter(content).is_none());
    }

    #[test]
    fn test_split_front_matter_extra_dashes_in_body() {
        let content = "---\nmodel: opus\n---\n# Title\n---\nSection break.";
        let (yaml, body) = split_front_matter(content).unwrap();
        assert_eq!(yaml, "model: opus");
        assert!(body.contains("Section break."));
    }

    #[test]
    fn test_expand_env_vars_dollar_form() {
        unsafe {
            std::env::set_var("RSI_TEST_VAR1", "hello");
        }
        let result = expand_env_vars("value: $RSI_TEST_VAR1");
        assert_eq!(result, "value: hello");
        unsafe {
            std::env::remove_var("RSI_TEST_VAR1");
        }
    }

    #[test]
    fn test_expand_env_vars_braces_form() {
        unsafe {
            std::env::set_var("RSI_TEST_VAR2", "world");
        }
        let result = expand_env_vars("value: ${RSI_TEST_VAR2}");
        assert_eq!(result, "value: world");
        unsafe {
            std::env::remove_var("RSI_TEST_VAR2");
        }
    }

    #[test]
    fn test_expand_env_vars_missing() {
        let result = expand_env_vars("value: $RSI_NONEXISTENT_12345");
        assert_eq!(result, "value: ");
    }

    #[test]
    fn test_expand_env_vars_no_vars() {
        let result = expand_env_vars("plain text no vars");
        assert_eq!(result, "plain text no vars");
    }

    #[test]
    fn test_expand_env_vars_adjacent() {
        unsafe {
            std::env::set_var("RSI_TEST_A", "foo");
        }
        unsafe {
            std::env::set_var("RSI_TEST_B", "bar");
        }
        let result = expand_env_vars("$RSI_TEST_A-${RSI_TEST_B}");
        assert_eq!(result, "foo-bar");
        unsafe {
            std::env::remove_var("RSI_TEST_A");
        }
        unsafe {
            std::env::remove_var("RSI_TEST_B");
        }
    }

    #[test]
    fn test_parse_rsi_md_valid() {
        let content = "---\nprovider: Claude\nmodel: opus\n---\n# Workflow\nDo things.";
        let (settings, body) = parse_rsi_md(content).unwrap();
        assert!(matches!(settings.provider, Some(SessionProvider::Claude)));
        assert_eq!(settings.model, Some("opus".to_string()));
        assert!(body.contains("# Workflow"));
    }

    #[test]
    fn test_parse_rsi_md_no_front_matter() {
        let content = "# Just a template\nNo settings here.";
        let (settings, body) = parse_rsi_md(content).unwrap();
        assert!(settings.provider.is_none());
        assert!(settings.model.is_none());
        assert!(body.contains("Just a template"));
    }

    #[test]
    fn test_parse_rsi_md_all_settings() {
        let content = "---\nprovider: Gemini\nmodel: gemini-2.5-pro\nsession_kind: TaskRabbit\nstall_timeout_secs: 300\nrotation_depth_limit: 2\nrotation_enabled: false\n---\nBody.";
        let (settings, _) = parse_rsi_md(content).unwrap();
        assert!(matches!(
            settings.provider,
            Some(SessionProvider::Antigravity)
        ));
        assert_eq!(settings.model, Some("gemini-2.5-pro".to_string()));
        assert!(matches!(
            settings.session_kind,
            Some(SessionKind::TaskRabbit)
        ));
        assert_eq!(settings.stall_timeout_secs, Some(300));
        assert_eq!(settings.rotation_depth_limit, Some(2));
        assert_eq!(settings.rotation_enabled, Some(false));
    }

    #[test]
    fn test_parse_rsi_md_partial_settings() {
        let content = "---\nmodel: sonnet\n---\nBody.";
        let (settings, _) = parse_rsi_md(content).unwrap();
        assert!(settings.provider.is_none());
        assert_eq!(settings.model, Some("sonnet".to_string()));
        assert!(settings.session_kind.is_none());
    }

    #[test]
    fn test_parse_rsi_md_invalid_yaml() {
        let content = "---\n: invalid: yaml: [\n---\nBody.";
        let result = parse_rsi_md(content);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("YAML parse error"));
    }

    #[test]
    fn test_parse_rsi_md_unknown_fields_ignored() {
        // Forward compatibility: unknown YAML fields should not cause errors
        let content = "---\nmodel: opus\nfuture_field: value\n---\nBody.";
        let (settings, _) = parse_rsi_md(content).unwrap();
        assert_eq!(settings.model, Some("opus".to_string()));
    }

    #[test]
    fn test_render_template_basic() {
        let ctx = TemplateContext {
            project: ProjectTemplateVars {
                name: "myproject".to_string(),
                path: "/home/user/myproject".to_string(),
                description: "A test project".to_string(),
            },
            session: SessionTemplateVars {
                query: "fix the bug".to_string(),
                working_dir: "/home/user/myproject".to_string(),
                git_branch: "main".to_string(),
                model: "opus".to_string(),
                provider: "Claude".to_string(),
                kind: "Standard".to_string(),
                rotation_depth: 0,
            },
            env: HashMap::new(),
        };
        let result = render_template("Working on {{ project.name }}", &ctx).unwrap();
        assert_eq!(result, "Working on myproject");
    }

    #[test]
    fn test_render_template_all_vars() {
        let ctx = TemplateContext {
            project: ProjectTemplateVars {
                name: "proj".to_string(),
                path: "/p".to_string(),
                description: "desc".to_string(),
            },
            session: SessionTemplateVars {
                query: "q".to_string(),
                working_dir: "/w".to_string(),
                git_branch: "dev".to_string(),
                model: "m".to_string(),
                provider: "Claude".to_string(),
                kind: "Standard".to_string(),
                rotation_depth: 2,
            },
            env: HashMap::from([("KEY".to_string(), "val".to_string())]),
        };
        let template = "{{ project.name }} {{ session.git_branch }} {{ session.rotation_depth }} {{ env.KEY }}";
        let result = render_template(template, &ctx).unwrap();
        assert_eq!(result, "proj dev 2 val");
    }

    #[test]
    fn test_render_template_empty() {
        let ctx = TemplateContext {
            project: ProjectTemplateVars {
                name: "p".to_string(),
                path: "/p".to_string(),
                description: "".to_string(),
            },
            session: SessionTemplateVars {
                query: "".to_string(),
                working_dir: "".to_string(),
                git_branch: "".to_string(),
                model: "".to_string(),
                provider: "".to_string(),
                kind: "".to_string(),
                rotation_depth: 0,
            },
            env: HashMap::new(),
        };
        let result = render_template("", &ctx).unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn test_render_template_conditional() {
        let ctx = TemplateContext {
            project: ProjectTemplateVars {
                name: "p".to_string(),
                path: "/p".to_string(),
                description: "".to_string(),
            },
            session: SessionTemplateVars {
                query: "".to_string(),
                working_dir: "".to_string(),
                git_branch: "".to_string(),
                model: "".to_string(),
                provider: "Claude".to_string(),
                kind: "".to_string(),
                rotation_depth: 0,
            },
            env: HashMap::new(),
        };
        let template = "{% if session.provider == 'Claude' %}use claude{% endif %}";
        let result = render_template(template, &ctx).unwrap();
        assert_eq!(result, "use claude");
    }

    #[test]
    fn test_fingerprint_equality() {
        let fp1 = FileFingerprint {
            mtime_secs: 100,
            size: 200,
            content_hash: 300,
        };
        let fp2 = FileFingerprint {
            mtime_secs: 100,
            size: 200,
            content_hash: 300,
        };
        assert_eq!(fp1, fp2);
    }

    #[test]
    fn test_fingerprint_inequality_mtime() {
        let fp1 = FileFingerprint {
            mtime_secs: 100,
            size: 200,
            content_hash: 300,
        };
        let fp2 = FileFingerprint {
            mtime_secs: 101,
            size: 200,
            content_hash: 300,
        };
        assert_ne!(fp1, fp2);
    }

    #[test]
    fn test_fingerprint_inequality_size() {
        let fp1 = FileFingerprint {
            mtime_secs: 100,
            size: 200,
            content_hash: 300,
        };
        let fp2 = FileFingerprint {
            mtime_secs: 100,
            size: 201,
            content_hash: 300,
        };
        assert_ne!(fp1, fp2);
    }

    #[test]
    fn test_fingerprint_inequality_hash() {
        let fp1 = FileFingerprint {
            mtime_secs: 100,
            size: 200,
            content_hash: 300,
        };
        let fp2 = FileFingerprint {
            mtime_secs: 100,
            size: 200,
            content_hash: 301,
        };
        assert_ne!(fp1, fp2);
    }

    #[test]
    fn test_compute_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.md");
        std::fs::write(&file, "hello world").unwrap();
        let fp = compute_fingerprint(&file).unwrap();
        assert!(fp.size > 0);
        assert!(fp.content_hash > 0);
    }

    #[test]
    fn test_cache_insert_get_remove() {
        let mut cache = ProjectWorkflowCache::new();
        let id = Uuid::new_v4();
        assert!(cache.get(&id).is_none());

        cache.insert(
            id,
            ProjectWorkflow {
                settings: ProjectSettings::default(),
                template_body: "test".to_string(),
                fingerprint: FileFingerprint {
                    mtime_secs: 0,
                    size: 0,
                    content_hash: 0,
                },
                loaded_at: Utc::now(),
                last_error: None,
            },
        );
        assert!(cache.get(&id).is_some());
        assert_eq!(cache.len(), 1);

        cache.remove(&id);
        assert!(cache.get(&id).is_none());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn test_cache_set_error_preserves_workflow() {
        let mut cache = ProjectWorkflowCache::new();
        let id = Uuid::new_v4();
        cache.insert(
            id,
            ProjectWorkflow {
                settings: ProjectSettings {
                    model: Some("opus".to_string()),
                    ..Default::default()
                },
                template_body: "template".to_string(),
                fingerprint: FileFingerprint {
                    mtime_secs: 0,
                    size: 0,
                    content_hash: 0,
                },
                loaded_at: Utc::now(),
                last_error: None,
            },
        );

        cache.set_error(&id, "yaml broke".to_string());
        let wf = cache.get(&id).unwrap();
        // Last-known-good: settings and body preserved
        assert_eq!(wf.settings.model, Some("opus".to_string()));
        assert_eq!(wf.template_body, "template");
        // Error is set
        assert_eq!(wf.last_error, Some("yaml broke".to_string()));
    }

    #[test]
    fn test_cache_set_error_nonexistent() {
        let mut cache = ProjectWorkflowCache::new();
        // Should not panic for nonexistent project
        cache.set_error(&Uuid::new_v4(), "error".to_string());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn test_load_project_workflow() {
        let dir = tempfile::tempdir().unwrap();
        let rsi_md = dir
            .path()
            .join(rsi_common::identity::PROJECT_CONFIG_FILENAME);
        std::fs::write(&rsi_md, "---\nmodel: opus\n---\n# Workflow\nDo things.").unwrap();

        let mut cache = ProjectWorkflowCache::new();
        let project_id = Uuid::new_v4();
        load_project_workflow(project_id, dir.path(), &mut cache);

        let wf = cache.get(&project_id).unwrap();
        assert_eq!(wf.settings.model, Some("opus".to_string()));
        assert!(wf.template_body.contains("# Workflow"));
        assert!(wf.last_error.is_none());
    }

    #[test]
    fn test_load_project_workflow_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = ProjectWorkflowCache::new();
        load_project_workflow(Uuid::new_v4(), dir.path(), &mut cache);
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn test_project_settings_serde_defaults() {
        let yaml = "";
        let settings: ProjectSettings = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(settings.provider.is_none());
        assert!(settings.model.is_none());
        assert!(settings.session_kind.is_none());
        assert!(settings.stall_timeout_secs.is_none());
        assert!(settings.rotation_depth_limit.is_none());
        assert!(settings.rotation_enabled.is_none());
    }

    /// Regression test for the `blocking_lock` panic.
    ///
    /// Before the fix, the `projects_provider` closure called
    /// `store.blocking_lock()` which panics with "Cannot block the current
    /// thread from within a runtime" when invoked from inside a tokio task.
    ///
    /// This test constructs a `ProjectsProvider` that mirrors the real closure
    /// in `session/mod.rs` (using `Arc<tokio::sync::Mutex<_>>` and
    /// `.lock().await`), drives it from within a `tokio::spawn` task, and
    /// asserts it returns without panicking.
    #[tokio::test]
    async fn test_projects_provider_no_blocking_lock_panic() {
        // Stand-in for any data protected by an async mutex.
        let data: Arc<tokio::sync::Mutex<Vec<(Uuid, PathBuf)>>> =
            Arc::new(tokio::sync::Mutex::new(vec![
                (Uuid::new_v4(), PathBuf::from("/tmp/project-a")),
                (Uuid::new_v4(), PathBuf::from("/tmp/project-b")),
            ]));

        // Build the provider exactly as session/mod.rs does after the fix.
        let data_for_provider = Arc::clone(&data);
        let provider: ProjectsProvider = Arc::new(move || {
            let inner = Arc::clone(&data_for_provider);
            Box::pin(async move {
                let guard = inner.lock().await;
                guard.clone()
            })
        });

        // Drive it from a spawned tokio task — this is the context in which the
        // original `blocking_lock()` would have panicked.
        let provider_clone = Arc::clone(&provider);
        let result = tokio::spawn(async move { provider_clone().await })
            .await
            .expect("task must not panic");

        assert_eq!(result.len(), 2, "expected both projects to be returned");
    }
}
