//! In-memory project lookup cache for fast working-directory → project resolution.
//!
//! `ProjectIndex` maintains a sorted list of `(PathBuf, Uuid)` pairs ordered by
//! path length (longest first) so that prefix matching returns the most specific
//! project for a given working directory.

use rsi_common::types::Project;
use std::path::{Path, PathBuf};
use tracing;
use uuid::Uuid;

/// Sorted index of project paths for longest-prefix-match lookups.
///
/// Wrap in `Arc<RwLock<ProjectIndex>>` for concurrent access from
/// the RPC handler and session launcher.
#[derive(Debug, Clone)]
pub struct ProjectIndex {
    /// Sorted by path length descending (longest first) for prefix matching.
    entries: Vec<(PathBuf, Uuid)>,
}

impl ProjectIndex {
    /// Build a new index from a list of projects.
    ///
    /// Projects without a path are excluded. Project paths are canonicalized at
    /// construction time so that symlink-based differences don't break prefix matching.
    /// Paths that don't exist on disk (or can't be canonicalized) are silently skipped.
    pub fn new(projects: Vec<Project>) -> Self {
        let mut entries: Vec<(PathBuf, Uuid)> = projects
            .into_iter()
            .filter_map(|p| {
                p.path.and_then(|path| {
                    match path.canonicalize() {
                        Ok(canon) => Some((canon, p.id)),
                        Err(_) => {
                            // Path doesn't exist or isn't accessible -- skip it.
                            // Projects with stale or invalid paths are excluded from
                            // index lookups. This is intentional: update the project
                            // record to restore it to the index.
                            tracing::trace!(
                                path = %path.display(),
                                "Skipping project with inaccessible path in index"
                            );
                            None
                        }
                    }
                })
            })
            .collect();

        // Sort by path length descending so longest prefix wins in linear scan.
        entries.sort_by(|a, b| b.0.as_os_str().len().cmp(&a.0.as_os_str().len()));

        Self { entries }
    }

    /// Find the project whose path is the longest prefix of `working_dir`.
    /// Returns `None` if no project path is a prefix.
    pub fn find_project_for_path(&self, working_dir: &Path) -> Option<Uuid> {
        for (path, id) in &self.entries {
            if working_dir.starts_with(path) {
                return Some(*id);
            }
        }
        None
    }

    /// Rebuild the index from a fresh project list.
    pub fn invalidate(&mut self, projects: Vec<Project>) {
        *self = Self::new(projects);
    }

    /// Number of indexed entries (projects with paths).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the index has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::fs;

    fn make_project(name: &str, path: Option<PathBuf>) -> Project {
        Project {
            id: Uuid::new_v4(),
            name: name.to_string(),
            path,
            description: None,
            color: Project::DEFAULT_COLOR.to_string(),
            context_files: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    /// Create a real temporary directory for a project path, returning the
    /// canonical path and a cleanup guard (the TempDir).
    fn make_real_dir(suffix: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("flywheel_project_cache_{suffix}"));
        fs::create_dir_all(&path).expect("create temp dir");
        path.canonicalize().expect("canonicalize temp dir")
    }

    fn cleanup(path: &Path) {
        let _ = fs::remove_dir_all(path);
    }

    #[test]
    fn test_exact_match() {
        let dir = make_real_dir("exact_match");
        let p = make_project("flywheel", Some(dir.clone()));
        let id = p.id;
        let index = ProjectIndex::new(vec![p]);

        assert_eq!(index.find_project_for_path(&dir), Some(id));
        cleanup(&dir);
    }

    #[test]
    fn test_prefix_match() {
        let dir = make_real_dir("prefix_match");
        let p = make_project("flywheel", Some(dir.clone()));
        let id = p.id;
        let index = ProjectIndex::new(vec![p]);

        assert_eq!(
            index.find_project_for_path(&dir.join("crates/flywheeld")),
            Some(id)
        );
        cleanup(&dir);
    }

    #[test]
    fn test_no_match() {
        let dir = make_real_dir("no_match");
        let p = make_project("flywheel", Some(dir.clone()));
        let index = ProjectIndex::new(vec![p]);

        assert_eq!(
            index.find_project_for_path(&dir.parent().unwrap().join("other-project")),
            None
        );
        cleanup(&dir);
    }

    #[test]
    fn test_path_boundary_no_false_prefix_match() {
        // dir must NOT match dir + "bar" suffix
        let dir = make_real_dir("path_boundary_foo");
        let p = make_project("foo", Some(dir.clone()));
        let index = ProjectIndex::new(vec![p]);

        let foobar = dir.parent().unwrap().join(format!(
            "{}_bar",
            dir.file_name().unwrap().to_string_lossy()
        ));
        assert_eq!(
            index.find_project_for_path(&foobar),
            None,
            "{} should not match project at {}",
            foobar.display(),
            dir.display()
        );
        cleanup(&dir);
    }

    #[test]
    fn test_longest_prefix_wins() {
        let parent_dir = make_real_dir("longest_prefix_parent");
        let child_dir = make_real_dir("longest_prefix_parent/child");
        // child_dir is inside parent_dir
        let parent = make_project("parent", Some(parent_dir.clone()));
        let child = make_project("child", Some(child_dir.clone()));
        let parent_id = parent.id;
        let child_id = child.id;
        let index = ProjectIndex::new(vec![parent, child]);

        // Deep path inside child should match child (longer prefix)
        assert_eq!(
            index.find_project_for_path(&child_dir.join("src")),
            Some(child_id)
        );

        // Path inside parent but outside child should match parent
        let sibling = parent_dir.join("other");
        assert_eq!(index.find_project_for_path(&sibling), Some(parent_id));
        cleanup(&child_dir);
        cleanup(&parent_dir);
    }

    #[test]
    fn test_projects_without_path_excluded() {
        let dir = make_real_dir("no_path_excluded");
        let with_path = make_project("with-path", Some(dir.clone()));
        let without_path = make_project("no-path", None);
        let id = with_path.id;
        let index = ProjectIndex::new(vec![with_path, without_path]);

        assert_eq!(index.len(), 1);
        assert_eq!(index.find_project_for_path(&dir), Some(id));
        cleanup(&dir);
    }

    #[test]
    fn test_invalidate_rebuilds() {
        let dir1 = make_real_dir("invalidate_old");
        let dir2 = make_real_dir("invalidate_new");
        let p1 = make_project("old", Some(dir1.clone()));
        let mut index = ProjectIndex::new(vec![p1]);

        let p2 = make_project("new", Some(dir2.clone()));
        let new_id = p2.id;
        index.invalidate(vec![p2]);

        assert_eq!(index.len(), 1);
        assert_eq!(index.find_project_for_path(&dir1), None);
        assert_eq!(index.find_project_for_path(&dir2), Some(new_id));
        cleanup(&dir1);
        cleanup(&dir2);
    }

    #[test]
    fn test_empty_index() {
        let index = ProjectIndex::new(vec![]);
        assert!(index.is_empty());
        assert_eq!(index.len(), 0);
        assert_eq!(index.find_project_for_path(Path::new("/any/path")), None);
    }

    #[tokio::test]
    async fn test_concurrent_invalidation_and_lookup() {
        use std::sync::Arc;
        use tokio::sync::RwLock;

        let dir = make_real_dir("concurrent_alpha");
        let p1 = make_project("alpha", Some(dir.clone()));
        let p1_id = p1.id;
        let index = Arc::new(RwLock::new(ProjectIndex::new(vec![p1])));

        // Spawn reader tasks that continuously look up while we invalidate
        let mut handles = Vec::new();
        for _ in 0..10 {
            let idx = Arc::clone(&index);
            let lookup_dir = dir.join("src");
            handles.push(tokio::spawn(async move {
                for _ in 0..100 {
                    let guard = idx.read().await;
                    let _ = guard.find_project_for_path(&lookup_dir);
                    drop(guard);
                    tokio::task::yield_now().await;
                }
            }));
        }

        // Invalidate the index repeatedly while readers run
        for i in 0..50 {
            let new_project = make_project(&format!("proj-{}", i), Some(dir.clone()));
            index.write().await.invalidate(vec![new_project]);
            tokio::task::yield_now().await;
        }

        // All reader tasks should complete without panic
        for h in handles {
            h.await.unwrap();
        }

        // After all invalidations, index should have exactly 1 entry
        let guard = index.read().await;
        assert_eq!(guard.len(), 1);
        assert!(guard.find_project_for_path(&dir).is_some());
        let _ = p1_id;
        cleanup(&dir);
    }

    #[test]
    fn test_multiple_projects_ordering() {
        let a = make_real_dir("ordering_a");
        let ab = make_real_dir("ordering_a/b");
        let abc = make_real_dir("ordering_a/b/c");

        let short = make_project("short", Some(a.clone()));
        let medium = make_project("medium", Some(ab.clone()));
        let long = make_project("long", Some(abc.clone()));
        let long_id = long.id;
        let medium_id = medium.id;
        let short_id = short.id;

        // Insert in non-sorted order to verify sorting works
        let index = ProjectIndex::new(vec![medium, short, long]);

        assert_eq!(index.find_project_for_path(&abc.join("d")), Some(long_id));
        assert_eq!(index.find_project_for_path(&ab.join("x")), Some(medium_id));
        assert_eq!(index.find_project_for_path(&a.join("x")), Some(short_id));
        cleanup(&abc);
        cleanup(&ab);
        cleanup(&a);
    }

    #[test]
    fn test_canonicalization_in_index() {
        // Create a real temp dir so canonicalize works
        let tmp = make_real_dir("canon_in_index");
        let mut p = make_project("real", Some(tmp.clone()));
        p.path = Some(tmp.clone());
        let id = p.id;
        let index = ProjectIndex::new(vec![p]);

        // Lookup with the canonical form should match
        assert_eq!(index.find_project_for_path(&tmp.join("subdir")), Some(id));
        cleanup(&tmp);
    }

    #[test]
    fn test_nonexistent_project_path_excluded_from_index() {
        let p = make_project(
            "ghost",
            Some(PathBuf::from("/nonexistent/path/that/does/not/exist")),
        );
        let index = ProjectIndex::new(vec![p]);
        assert!(index.is_empty());
    }
}
