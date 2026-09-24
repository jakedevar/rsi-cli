//! Command-frontmatter registry for RSI-010 capability classes.
//!
//! Reads `.claude/commands/*.md` at daemon startup using the shared parser from
//! `rsi_common::command_meta`, and exposes an O(1) lookup by command name (file
//! stem) for stamping `Session.capability_class` on launch.
//!
//! Parse failures on individual files are logged and skipped per-file so a
//! single malformed file never takes down daemon startup.

use rsi_common::command_meta::{CommandMeta, parse_command_file};
use rsi_common::types::CapabilityClass;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// In-memory registry of command-frontmatter parse results, keyed by command
/// name (file stem). Construct once at daemon startup and hold behind an `Arc`.
#[derive(Debug, Clone, Default)]
pub struct CommandRegistry {
    by_name: HashMap<String, CommandMeta>,
}

impl CommandRegistry {
    /// Empty registry — used by unit tests and when no commands dir exists.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Walk `.claude/commands/*.md` under `commands_root`, skip `_shared/`,
    /// and build the registry. Per-file parse errors are logged via `tracing`
    /// and skipped — the registry never fails to construct.
    pub fn load(commands_root: &Path) -> Self {
        let mut by_name = HashMap::new();

        let entries = match std::fs::read_dir(commands_root) {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!(
                    path = %commands_root.display(),
                    error = %e,
                    "Commands directory unreadable; CommandRegistry empty"
                );
                return Self { by_name };
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();
            // Skip directories (e.g. `_shared/`) and non-md files.
            if path.is_dir() {
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let name = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            match parse_command_file(&path) {
                Ok(fm) => {
                    by_name.insert(name, fm);
                }
                Err(e) => {
                    // Log and skip — one bad file MUST NOT prevent daemon startup.
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "CommandRegistry: skipping malformed command file"
                    );
                }
            }
        }

        Self { by_name }
    }

    /// Number of commands in the registry (for telemetry / tests).
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// Fetch a command's full frontmatter by name (file stem).
    pub fn get(&self, name: &str) -> Option<&CommandMeta> {
        self.by_name.get(name)
    }

    /// Resolve a query's leading `/<command>` (ASCII alphanumeric + `_`) into
    /// a declared `CapabilityClass`, if any.
    ///
    /// Returns `None` for free-text queries, unknown commands, or commands
    /// whose frontmatter omits `capability_class`.
    pub fn class_for_query(&self, query: &str) -> Option<CapabilityClass> {
        let name = Self::parse_command_name(query)?;
        self.by_name.get(&name).and_then(|fm| fm.capability_class)
    }

    /// Parse `"/<name> …"` → `Some("<name>")`. Accepts only leading-slash
    /// commands made of ASCII alphanumerics and underscores.
    fn parse_command_name(query: &str) -> Option<String> {
        let rest = query.trim_start().strip_prefix('/')?;
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        if end == 0 {
            return None;
        }
        Some(rest[..end].to_string())
    }
}

/// Resolve the commands root directory next to a project root. Mirrors the
/// `.claude/commands/` convention. Used by `SessionManager::new`.
pub fn default_commands_root() -> PathBuf {
    // Prefer cwd's `.claude/commands` (the repo layout).
    let cwd_candidate = std::env::current_dir()
        .map(|c| c.join(".claude").join("commands"))
        .ok();
    if let Some(p) = cwd_candidate
        && p.is_dir()
    {
        return p;
    }
    // Fall back to ~/.claude/commands if it exists.
    if let Some(home) = dirs::home_dir() {
        let home_candidate = home.join(".claude").join("commands");
        if home_candidate.is_dir() {
            return home_candidate;
        }
    }
    // Last resort — return the cwd candidate; `load()` handles a missing dir.
    std::env::current_dir()
        .map(|c| c.join(".claude").join("commands"))
        .unwrap_or_else(|_| PathBuf::from(".claude/commands"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn tracked_top_level_command_names(
        repo_root: &Path,
        commands_root: &Path,
    ) -> Option<Vec<String>> {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo_root)
            .args(["ls-files", "--", ".claude/commands"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let names = stdout
            .lines()
            .filter_map(|rel_path| {
                let path = repo_root.join(rel_path);
                if path.parent() != Some(commands_root) {
                    return None;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("md") {
                    return None;
                }
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .map(str::to_string)
            })
            .collect();
        Some(names)
    }

    #[test]
    fn class_for_query_matches() {
        let mut reg = CommandRegistry::default();
        reg.by_name.insert(
            "master_implement".into(),
            CommandMeta {
                capability_class: Some(CapabilityClass::Architect),
                ..Default::default()
            },
        );
        reg.by_name.insert(
            "commit".into(),
            CommandMeta {
                capability_class: Some(CapabilityClass::LookupFast),
                ..Default::default()
            },
        );

        assert_eq!(
            reg.class_for_query("/master_implement foo bar"),
            Some(CapabilityClass::Architect)
        );
        assert_eq!(
            reg.class_for_query("/commit"),
            Some(CapabilityClass::LookupFast)
        );
        assert_eq!(
            reg.class_for_query("/commit\nwith trailing newline"),
            Some(CapabilityClass::LookupFast)
        );
    }

    #[test]
    fn class_for_query_none_for_free_text() {
        let mut reg = CommandRegistry::default();
        reg.by_name.insert(
            "commit".into(),
            CommandMeta {
                capability_class: Some(CapabilityClass::LookupFast),
                ..Default::default()
            },
        );

        assert_eq!(reg.class_for_query("just a free-text query"), None);
        assert_eq!(reg.class_for_query("/unknown_cmd"), None);
        assert_eq!(reg.class_for_query(""), None);
        assert_eq!(reg.class_for_query("/"), None);
    }

    #[test]
    fn registry_loads_files_and_skips_malformed() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        fs::write(
            root.join("good.md"),
            "---\ncapability_class: architect\n---\nBody\n",
        )
        .unwrap();
        fs::write(
            root.join("no_fm.md"),
            "No frontmatter at all — just body text.\n",
        )
        .unwrap();
        fs::write(
            root.join("bad.md"),
            "---\n: not: valid: yaml: [\n---\nBroken.\n",
        )
        .unwrap();
        // Unrelated file — should be ignored.
        fs::write(root.join("README.txt"), "not a command").unwrap();
        // _shared subdir — should be skipped by the is_dir filter.
        fs::create_dir(root.join("_shared")).unwrap();
        fs::write(
            root.join("_shared").join("worker_preamble.md"),
            "---\nversion: 3\n---\nshared",
        )
        .unwrap();

        let reg = CommandRegistry::load(root);

        // "good.md" parsed successfully with class.
        let good = reg.get("good").expect("good.md should be registered");
        assert_eq!(good.capability_class, Some(CapabilityClass::Architect));

        // "no_fm.md" → default frontmatter.
        let no_fm = reg.get("no_fm").expect("no_fm.md should be registered");
        assert!(no_fm.capability_class.is_none());

        // "bad.md" was skipped; key absent.
        assert!(reg.get("bad").is_none());

        // Non-md files skipped.
        assert!(reg.get("README").is_none());

        // _shared/worker_preamble.md is in a sub-directory and must NOT appear.
        assert!(reg.get("worker_preamble").is_none());
    }

    #[test]
    fn registry_load_missing_dir_is_empty() {
        let dir = tempdir().unwrap();
        let nonexistent = dir.path().join("does_not_exist");
        let reg = CommandRegistry::load(&nonexistent);
        assert!(reg.is_empty());
    }

    /// Regression test (RSI-010 plan §Automated verification): every
    /// `.claude/commands/*.md` file in the repo carries a `capability_class`.
    /// Locates the repo root by walking up from `CARGO_MANIFEST_DIR` until a
    /// `.claude/commands` sibling directory is found. Silently skips when the
    /// commands directory is absent (e.g. cross-checkout runs outside the
    /// repo), so the test never becomes a drag on unrelated harnesses.
    #[test]
    fn registry_loads_all_commands_with_class() {
        let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        // Walk up: crates/rsid -> crates -> repo root
        let repo_root = manifest_dir
            .ancestors()
            .find(|p| p.join(".claude").join("commands").is_dir())
            .map(Path::to_path_buf);
        let Some(repo_root) = repo_root else {
            eprintln!("no .claude/commands found; skipping coverage assertion");
            return;
        };
        let commands_root = repo_root.join(".claude").join("commands");
        let Some(command_names) = tracked_top_level_command_names(&repo_root, &commands_root)
        else {
            eprintln!("git tracked command list unavailable; skipping coverage assertion");
            return;
        };
        assert!(
            !command_names.is_empty(),
            "git found zero tracked command files under {}",
            commands_root.display()
        );

        let reg = CommandRegistry::load(&commands_root);
        assert!(
            !reg.is_empty(),
            "CommandRegistry loaded zero entries from {}",
            commands_root.display()
        );

        let mut missing = Vec::new();
        for name in command_names {
            match reg.get(&name) {
                Some(fm) if fm.capability_class.is_some() => {}
                Some(_) => missing.push(format!("{}: declared but no class", name)),
                None => missing.push(format!("{}: not registered", name)),
            }
        }
        assert!(
            missing.is_empty(),
            "commands missing capability_class: {}",
            missing.join(", ")
        );
    }
}
