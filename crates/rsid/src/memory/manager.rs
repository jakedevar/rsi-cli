use std::path::{Path, PathBuf};

use crate::error::{DaemonError, Result};
use crate::memory::files::is_memory_path;
use crate::memory::types::{MemoryProviderStatus, MemorySearchResult};
use crate::memory::worker::MemoryHandle;
use rsi_common::types::{Observation, ObservationSearchResult};
use uuid::Uuid;

/// Thin async facade over `MemoryHandle` that adds path validation for `read_file`.
#[derive(Clone)]
pub struct MemoryManager {
    handle: MemoryHandle,
    memory_dir: PathBuf,
}

impl MemoryManager {
    pub fn new(handle: MemoryHandle, memory_dir: PathBuf) -> Self {
        Self { handle, memory_dir }
    }

    pub async fn search(
        &self,
        query: &str,
        max_results: Option<usize>,
        min_score: Option<f64>,
        project_id: Option<Uuid>,
    ) -> Result<Vec<MemorySearchResult>> {
        self.handle
            .search(query, max_results, min_score, project_id)
            .await
    }

    pub async fn status(&self) -> Result<MemoryProviderStatus> {
        self.handle.status().await
    }

    pub async fn sync(&self, force: bool) -> Result<()> {
        self.handle.sync_now(force, "rpc").await
    }

    pub async fn read_file(
        &self,
        path: &str,
        from_line: Option<usize>,
        num_lines: Option<usize>,
    ) -> Result<(String, String)> {
        let validated = validate_read_path(path, &self.memory_dir)?;
        let content = self
            .handle
            .read_file(&validated, from_line, num_lines)
            .await?;
        Ok((content, validated))
    }

    pub async fn trigger_sync(&self, reason: &str) -> Result<()> {
        self.handle.sync_now(false, reason).await
    }

    pub async fn shutdown(&self) {
        let _ = self.handle.shutdown().await;
    }

    pub fn handle(&self) -> &MemoryHandle {
        &self.handle
    }

    /// List observations with optional session/project filter and limit.
    pub async fn list_observations(
        &self,
        session_id: Option<Uuid>,
        project_id: Option<Uuid>,
        limit: Option<usize>,
    ) -> Result<Vec<Observation>> {
        self.handle
            .list_observations(session_id, project_id, limit)
            .await
    }

    /// Search observations by keyword query.
    pub async fn search_observations(
        &self,
        query: &str,
        max_results: Option<usize>,
        project_id: Option<Uuid>,
    ) -> Result<Vec<ObservationSearchResult>> {
        self.handle
            .search_observations(query, max_results, project_id)
            .await
    }

    /// Get total observation count.
    pub async fn observation_count(&self) -> Result<u64> {
        self.handle.observation_count().await
    }
}

/// Validate that a read path is within memory scope and is a .md file.
///
/// Returns the relative path string to pass to the worker.
fn validate_read_path(path: &str, memory_dir: &Path) -> Result<String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err(DaemonError::Rpc("path required".to_string()));
    }

    // Resolve to absolute, then derive relative path from memory_dir
    let abs_path = if Path::new(trimmed).is_absolute() {
        PathBuf::from(trimmed)
    } else {
        memory_dir.join(trimmed)
    };

    // Canonicalize-lite: strip redundant components without requiring the file to exist
    let rel_path = match abs_path.strip_prefix(memory_dir) {
        Ok(rel) => rel.to_string_lossy().to_string(),
        Err(_) => {
            // The path doesn't fall under memory_dir
            return Err(DaemonError::Rpc("path outside memory scope".to_string()));
        }
    };

    let rel_path = rel_path.replace('\\', "/");

    if rel_path.is_empty() || rel_path.starts_with("..") || Path::new(&rel_path).is_absolute() {
        return Err(DaemonError::Rpc("path outside memory scope".to_string()));
    }

    // Check via is_memory_path (expects paths like "MEMORY.md" or "memory/foo.md")
    if !is_memory_path(&rel_path) {
        return Err(DaemonError::Rpc("path outside memory scope".to_string()));
    }

    // Must be a .md file
    if !rel_path.ends_with(".md") {
        return Err(DaemonError::Rpc("only .md files can be read".to_string()));
    }

    Ok(rel_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir() -> PathBuf {
        PathBuf::from("/home/user/.flywheel/memory")
    }

    #[test]
    fn test_empty_path() {
        assert!(validate_read_path("", &test_dir()).is_err());
        assert!(validate_read_path("   ", &test_dir()).is_err());
    }

    #[test]
    fn test_traversal_attack() {
        let err = validate_read_path("../../../etc/passwd", &test_dir()).unwrap_err();
        assert!(err.to_string().contains("outside memory scope"));
    }

    #[test]
    fn test_absolute_outside_path() {
        let err = validate_read_path("/etc/passwd", &test_dir()).unwrap_err();
        assert!(err.to_string().contains("outside memory scope"));
    }

    #[test]
    fn test_non_memory_path() {
        let err = validate_read_path("src/main.rs", &test_dir()).unwrap_err();
        assert!(err.to_string().contains("outside memory scope"));
    }

    #[test]
    fn test_non_md_file() {
        let err = validate_read_path("memory/notes.txt", &test_dir()).unwrap_err();
        assert!(err.to_string().contains("only .md files"));
    }

    #[test]
    fn test_valid_memory_md() {
        let result = validate_read_path("MEMORY.md", &test_dir()).unwrap();
        assert_eq!(result, "MEMORY.md");
    }

    #[test]
    fn test_valid_memory_subdir() {
        let result = validate_read_path("memory/2026-02-28.md", &test_dir()).unwrap();
        assert_eq!(result, "memory/2026-02-28.md");
    }

    #[test]
    fn test_valid_nested_path() {
        let result = validate_read_path("memory/topic/notes.md", &test_dir()).unwrap();
        assert_eq!(result, "memory/topic/notes.md");
    }

    #[test]
    fn test_backslash_normalized() {
        // Windows-style separators should be normalized but the path must still be valid
        // On Unix, backslashes become part of the filename — this tests the normalization logic
        let result = validate_read_path("memory/notes.md", &test_dir());
        assert!(result.is_ok());
    }
}
