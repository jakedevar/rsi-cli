use crate::error::Result;
use crate::memory::embedding::EmbeddingProviderResult;
use crate::memory::types::{MemoryConfig, MemoryIndexMeta};
use std::path::{Path, PathBuf};
use tracing::info;
use uuid::Uuid;

/// Reasons a full reindex is required instead of incremental sync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReindexTrigger {
    /// No metadata found in the database (first run or corrupted).
    NoMeta,
    /// The embedding model has changed.
    ModelChanged { stored: String, current: String },
    /// The embedding provider has changed.
    ProviderChanged { stored: String, current: String },
    /// Chunk token settings have changed.
    ChunkSettingsChanged {
        stored_tokens: u32,
        stored_overlap: u32,
        current_tokens: u32,
        current_overlap: u32,
    },
    /// A schema migration backfilled `project_id` columns onto existing rows;
    /// existing chunks lack project scope until reindexed. Set by store
    /// migration via the `reindex_required` meta key.
    SchemaUpgrade,
}

/// Check whether a full reindex is needed by comparing stored metadata
/// against the current configuration and embedding provider.
///
/// Returns `None` if incremental sync is sufficient, or `Some(trigger)`
/// describing why a full reindex is required.
pub fn check_reindex_trigger(
    meta: &Option<MemoryIndexMeta>,
    provider: &EmbeddingProviderResult,
    config: &MemoryConfig,
) -> Option<ReindexTrigger> {
    let meta = match meta {
        Some(m) => m,
        None => return Some(ReindexTrigger::NoMeta),
    };

    let current_model = provider.model_name().to_string();
    if meta.model != current_model {
        return Some(ReindexTrigger::ModelChanged {
            stored: meta.model.clone(),
            current: current_model,
        });
    }

    let current_provider = provider.provider_id().to_string();
    if meta.provider != current_provider {
        return Some(ReindexTrigger::ProviderChanged {
            stored: meta.provider.clone(),
            current: current_provider,
        });
    }

    if meta.chunk_tokens != config.chunk_tokens || meta.chunk_overlap != config.chunk_overlap {
        return Some(ReindexTrigger::ChunkSettingsChanged {
            stored_tokens: meta.chunk_tokens,
            stored_overlap: meta.chunk_overlap,
            current_tokens: config.chunk_tokens,
            current_overlap: config.chunk_overlap,
        });
    }

    None
}

/// Atomically swap index files by renaming temp to target.
/// Handles WAL (-wal) and SHM (-shm) sidecar files.
///
/// Uses a three-step approach: current -> backup, temp -> current, remove backup.
/// If the temp -> current rename fails, the backup is restored.
pub async fn swap_index_files(target: &Path, temp: &Path) -> Result<()> {
    let backup_path = PathBuf::from(format!("{}.backup-{}", target.display(), Uuid::new_v4()));

    // Move current -> backup (may fail if no existing DB, which is fine)
    if target.exists() {
        move_index_files(target, &backup_path).await?;
    }

    // Move temp -> target
    match move_index_files(temp, target).await {
        Ok(()) => {}
        Err(e) => {
            // Restore from backup
            if backup_path.exists() {
                let _ = move_index_files(&backup_path, target).await;
            }
            return Err(e);
        }
    }

    // Clean up backup
    remove_index_files(&backup_path).await;

    Ok(())
}

/// Move a SQLite database and its sidecar files (WAL, SHM).
async fn move_index_files(source: &Path, target: &Path) -> Result<()> {
    let suffixes = ["", "-wal", "-shm"];
    for suffix in &suffixes {
        let src = PathBuf::from(format!("{}{}", source.display(), suffix));
        let dst = PathBuf::from(format!("{}{}", target.display(), suffix));
        match tokio::fs::rename(&src, &dst).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // WAL/SHM files may not exist -- that's fine
            }
            Err(e) => {
                return Err(crate::error::DaemonError::InvalidParam(format!(
                    "failed to rename {} -> {}: {e}",
                    src.display(),
                    dst.display()
                )));
            }
        }
    }
    Ok(())
}

/// Remove a SQLite database and its sidecar files.
pub async fn remove_index_files(base: &Path) {
    let suffixes = ["", "-wal", "-shm"];
    for suffix in &suffixes {
        let path = PathBuf::from(format!("{}{}", base.display(), suffix));
        let _ = tokio::fs::remove_file(&path).await;
    }
}

/// Remove stale temporary reindex files left behind by interrupted reindexes.
///
/// Scans the directory containing `db_path` for files matching
/// `{db_name}.tmp-*` and `{db_name}.backup-*` and removes them
/// along with their WAL/SHM sidecars.
pub async fn cleanup_stale_temp_files(db_path: &Path) -> Result<()> {
    let parent = db_path
        .parent()
        .ok_or_else(|| crate::error::DaemonError::InvalidParam("db_path has no parent".into()))?;

    let db_name = db_path
        .file_name()
        .ok_or_else(|| crate::error::DaemonError::InvalidParam("db_path has no filename".into()))?
        .to_string_lossy();

    let temp_prefix = format!("{}.tmp-", db_name);
    let backup_prefix = format!("{}.backup-", db_name);

    let mut entries = match tokio::fs::read_dir(parent).await {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(crate::error::DaemonError::InvalidParam(format!(
                "failed to read directory {}: {e}",
                parent.display()
            )));
        }
    };

    let mut cleaned = 0u32;
    while let Some(entry) = entries.next_entry().await.map_err(|e| {
        crate::error::DaemonError::InvalidParam(format!("failed to read dir entry: {e}"))
    })? {
        let name = entry.file_name().to_string_lossy().to_string();
        // Skip WAL/SHM sidecar files (they'll be cleaned with their parent)
        if name.ends_with("-wal") || name.ends_with("-shm") {
            continue;
        }
        if name.starts_with(&temp_prefix) || name.starts_with(&backup_prefix) {
            let stale_path = entry.path();
            info!("memory: cleaning stale file: {}", stale_path.display());
            remove_index_files(&stale_path).await;
            cleaned += 1;
        }
    }

    if cleaned > 0 {
        info!("memory: cleaned {cleaned} stale temp/backup files");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::types::MemorySource;
    use tempfile::TempDir;

    fn make_meta(model: &str, provider: &str, tokens: u32, overlap: u32) -> MemoryIndexMeta {
        MemoryIndexMeta {
            model: model.to_string(),
            provider: provider.to_string(),
            provider_key: None,
            sources: vec![MemorySource::Memory],
            chunk_tokens: tokens,
            chunk_overlap: overlap,
            vector_dims: None,
        }
    }

    fn make_fts_only_provider() -> EmbeddingProviderResult {
        EmbeddingProviderResult {
            provider: None,
            requested: "none".to_string(),
            provider_label: "none".to_string(),
            backend: "none".to_string(),
            base_url: None,
            fallback_reason: None,
            unavailable_reason: None,
        }
    }

    fn make_config(tokens: u32, overlap: u32) -> MemoryConfig {
        MemoryConfig {
            chunk_tokens: tokens,
            chunk_overlap: overlap,
            ..Default::default()
        }
    }

    #[test]
    fn test_check_reindex_no_meta() {
        let provider = make_fts_only_provider();
        let config = make_config(400, 80);
        let result = check_reindex_trigger(&None, &provider, &config);
        assert_eq!(result, Some(ReindexTrigger::NoMeta));
    }

    #[test]
    fn test_check_reindex_model_changed() {
        let meta = make_meta("old-model", "none", 400, 80);
        let provider = make_fts_only_provider();
        let config = make_config(400, 80);
        let result = check_reindex_trigger(&Some(meta), &provider, &config);
        assert!(matches!(result, Some(ReindexTrigger::ModelChanged { .. })));
    }

    #[test]
    fn test_check_reindex_provider_changed() {
        let meta = make_meta("none", "ollama", 400, 80);
        let provider = make_fts_only_provider();
        let config = make_config(400, 80);
        let result = check_reindex_trigger(&Some(meta), &provider, &config);
        assert!(matches!(
            result,
            Some(ReindexTrigger::ProviderChanged { .. })
        ));
    }

    #[test]
    fn test_check_reindex_chunk_settings_changed() {
        let meta = make_meta("none", "none", 400, 80);
        let provider = make_fts_only_provider();
        let config = make_config(800, 160);
        let result = check_reindex_trigger(&Some(meta), &provider, &config);
        assert!(matches!(
            result,
            Some(ReindexTrigger::ChunkSettingsChanged { .. })
        ));
    }

    #[test]
    fn test_check_reindex_no_trigger() {
        let meta = make_meta("none", "none", 400, 80);
        let provider = make_fts_only_provider();
        let config = make_config(400, 80);
        let result = check_reindex_trigger(&Some(meta), &provider, &config);
        assert_eq!(result, None);
    }

    #[test]
    fn test_check_reindex_fts_only_to_fts_only() {
        let meta = make_meta("none", "none", 400, 80);
        let provider = make_fts_only_provider();
        let config = make_config(400, 80);
        let result = check_reindex_trigger(&Some(meta), &provider, &config);
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn test_cleanup_stale_temp_files() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("memory.sqlite");
        std::fs::write(&db_path, "real db").unwrap();

        // Create stale temp files
        let temp1 = dir.path().join("memory.sqlite.tmp-abc123");
        let temp1_wal = dir.path().join("memory.sqlite.tmp-abc123-wal");
        std::fs::write(&temp1, "temp1").unwrap();
        std::fs::write(&temp1_wal, "wal1").unwrap();

        cleanup_stale_temp_files(&db_path).await.unwrap();

        assert!(db_path.exists(), "real db should be preserved");
        assert!(!temp1.exists(), "stale temp should be removed");
        assert!(!temp1_wal.exists(), "stale wal should be removed");
    }

    #[tokio::test]
    async fn test_cleanup_stale_temp_no_false_positives() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("memory.sqlite");
        std::fs::write(&db_path, "real db").unwrap();

        // Create an unrelated file
        let unrelated = dir.path().join("other.tmp-xyz");
        std::fs::write(&unrelated, "unrelated").unwrap();

        cleanup_stale_temp_files(&db_path).await.unwrap();

        assert!(unrelated.exists(), "unrelated file should be preserved");
    }

    #[tokio::test]
    async fn test_cleanup_stale_backup_files() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("memory.sqlite");
        std::fs::write(&db_path, "real db").unwrap();

        let backup = dir.path().join("memory.sqlite.backup-abc123");
        std::fs::write(&backup, "backup").unwrap();

        cleanup_stale_temp_files(&db_path).await.unwrap();

        assert!(!backup.exists(), "stale backup should be removed");
    }

    #[tokio::test]
    async fn test_swap_index_files_basic() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("memory.sqlite");
        let temp = dir.path().join("memory.sqlite.tmp-test");

        std::fs::write(&target, "original").unwrap();
        std::fs::write(&temp, "new content").unwrap();

        swap_index_files(&target, &temp).await.unwrap();

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new content");
        assert!(!temp.exists(), "temp should be removed after swap");
    }

    #[tokio::test]
    async fn test_swap_index_files_handles_wal_shm() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("memory.sqlite");
        let target_wal = dir.path().join("memory.sqlite-wal");
        let temp = dir.path().join("memory.sqlite.tmp-test");
        let temp_wal = dir.path().join("memory.sqlite.tmp-test-wal");

        std::fs::write(&target, "original").unwrap();
        std::fs::write(&target_wal, "original-wal").unwrap();
        std::fs::write(&temp, "new").unwrap();
        std::fs::write(&temp_wal, "new-wal").unwrap();

        swap_index_files(&target, &temp).await.unwrap();

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(&target_wal).unwrap(), "new-wal");
    }
}
