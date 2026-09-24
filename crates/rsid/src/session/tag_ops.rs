//! Multi-tag CRUD on SessionManager. Mirrors hierarchy_ops.rs structurally.
//!
//! All DB operations run inside `spawn_blocking` closures using
//! `store.blocking_lock()`. No separate store module — SQL lives here directly,
//! matching the `handle_list_session_children` pattern in rpc.rs.

use super::SessionManager;
use crate::error::{DaemonError, Result};
use rsi_common::{TagWithCount, normalize_tag};
use uuid::Uuid;

/// Refresh `sessions.tag` to the lexicographically-first tag for this session,
/// or `""` if the session has no tags. Must be called inside the same DB
/// transaction (or immediately after the mutation) to keep the two tables in sync.
fn refresh_legacy_tag_column(
    conn: &rusqlite::Connection,
    session_id: Uuid,
) -> rusqlite::Result<()> {
    let id_str = session_id.to_string();

    // Get the lexicographically first tag (ORDER BY tag ASC LIMIT 1).
    let first_tag: String = conn
        .query_row(
            "SELECT tag FROM session_tags WHERE session_id = ?1 ORDER BY tag ASC LIMIT 1",
            rusqlite::params![&id_str],
            |row| row.get(0),
        )
        .unwrap_or_default(); // empty string if no rows

    conn.execute(
        "UPDATE sessions SET tag = ?1 WHERE id = ?2",
        rusqlite::params![&first_tag, &id_str],
    )?;
    Ok(())
}

impl SessionManager {
    /// Replace the full tag set for a session (atomic delete-then-insert).
    ///
    /// Validation:
    /// - `tags` must be non-empty (returns `InvalidParam("tags_required")`).
    /// - Each tag is normalized via `normalize_tag`; first malformed tag triggers
    ///   rejection without mutating any DB row.
    /// - Duplicates are deduplicated after normalization.
    ///
    /// Side-effect: `sessions.tag` is refreshed to the first (sorted) tag.
    pub async fn update_session_tags(&self, session_id: Uuid, tags: Vec<String>) -> Result<()> {
        if tags.is_empty() {
            return Err(DaemonError::InvalidParam("tags_required".to_string()));
        }

        // Normalize all tags upfront — fail-fast on first error.
        let mut normalized: Vec<String> = tags
            .iter()
            .map(|t| {
                normalize_tag(t)
                    .map_err(|_| DaemonError::InvalidParam(format!("tag_malformed: {}", t)))
            })
            .collect::<Result<Vec<_>>>()?;

        // Deduplicate (sort + dedup).
        normalized.sort();
        normalized.dedup();

        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            let conn = &store.conn;
            let id_str = session_id.to_string();

            // Validate session exists.
            let exists: bool = conn
                .query_row(
                    "SELECT 1 FROM sessions WHERE id = ?1",
                    rusqlite::params![&id_str],
                    |_| Ok(true),
                )
                .unwrap_or(false);
            if !exists {
                return Err(DaemonError::SessionNotFound(session_id));
            }

            // Atomic replace: delete all existing tags, insert new set.
            let tx = conn
                .unchecked_transaction()
                .map_err(DaemonError::Database)?;

            tx.execute(
                "DELETE FROM session_tags WHERE session_id = ?1",
                rusqlite::params![&id_str],
            )
            .map_err(DaemonError::Database)?;

            for tag in &normalized {
                tx.execute(
                    "INSERT INTO session_tags (session_id, tag) VALUES (?1, ?2)",
                    rusqlite::params![&id_str, tag],
                )
                .map_err(DaemonError::Database)?;
            }

            refresh_legacy_tag_column(&tx, session_id).map_err(DaemonError::Database)?;

            tx.commit().map_err(DaemonError::Database)?;
            Ok(())
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))?
    }

    /// Add a single tag to the session (idempotent — already-present tag is a no-op).
    pub async fn add_session_tag(&self, session_id: Uuid, tag: String) -> Result<()> {
        let normalized = normalize_tag(&tag)
            .map_err(|_| DaemonError::InvalidParam(format!("tag_malformed: {}", tag)))?;

        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            let conn = &store.conn;
            let id_str = session_id.to_string();

            // Validate session exists.
            let exists: bool = conn
                .query_row(
                    "SELECT 1 FROM sessions WHERE id = ?1",
                    rusqlite::params![&id_str],
                    |_| Ok(true),
                )
                .unwrap_or(false);
            if !exists {
                return Err(DaemonError::SessionNotFound(session_id));
            }

            conn.execute(
                "INSERT OR IGNORE INTO session_tags (session_id, tag) VALUES (?1, ?2)",
                rusqlite::params![&id_str, &normalized],
            )
            .map_err(DaemonError::Database)?;

            refresh_legacy_tag_column(conn, session_id).map_err(DaemonError::Database)?;

            Ok(())
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))?
    }

    /// Remove a single tag from the session (idempotent — absent tag is a no-op).
    pub async fn remove_session_tag(&self, session_id: Uuid, tag: String) -> Result<()> {
        let normalized = normalize_tag(&tag)
            .map_err(|_| DaemonError::InvalidParam(format!("tag_malformed: {}", tag)))?;

        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            let conn = &store.conn;
            let id_str = session_id.to_string();

            // Validate session exists.
            let exists: bool = conn
                .query_row(
                    "SELECT 1 FROM sessions WHERE id = ?1",
                    rusqlite::params![&id_str],
                    |_| Ok(true),
                )
                .unwrap_or(false);
            if !exists {
                return Err(DaemonError::SessionNotFound(session_id));
            }

            conn.execute(
                "DELETE FROM session_tags WHERE session_id = ?1 AND tag = ?2",
                rusqlite::params![&id_str, &normalized],
            )
            .map_err(DaemonError::Database)?;

            refresh_legacy_tag_column(conn, session_id).map_err(DaemonError::Database)?;

            Ok(())
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))?
    }

    /// List tags with session-counts, ordered by count descending then alphabetically.
    /// Optional `prefix` filters by tag prefix. Optional `project_id` restricts
    /// to sessions in a project. Capped at 100 rows.
    pub async fn list_tags(
        &self,
        prefix: Option<String>,
        project_id: Option<Uuid>,
    ) -> Result<Vec<TagWithCount>> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            let conn = &store.conn;

            // Build query conditionally.
            let has_project = project_id.is_some();
            let has_prefix = prefix.is_some();

            // Base query (no join): returns all tags with counts.
            // When project_id filter is active, JOIN sessions and use st. prefix.
            let mut sql = if has_project {
                String::from(
                    "SELECT st.tag, COUNT(*) AS n FROM session_tags st \
                     INNER JOIN sessions s ON s.id = st.session_id",
                )
            } else {
                String::from("SELECT tag, COUNT(*) AS n FROM session_tags")
            };

            let mut where_clauses: Vec<&str> = Vec::new();

            if has_prefix && has_project {
                where_clauses.push("st.tag LIKE ?1 || '%'");
            } else if has_prefix {
                where_clauses.push("tag LIKE ?1 || '%'");
            }
            if has_project {
                where_clauses.push(if has_prefix {
                    "s.project_id = ?2"
                } else {
                    "s.project_id = ?1"
                });
            }

            if !where_clauses.is_empty() {
                sql.push_str(" WHERE ");
                sql.push_str(&where_clauses.join(" AND "));
            }

            let tag_col = if has_project { "st.tag" } else { "tag" };
            sql.push_str(&format!(
                " GROUP BY {tag_col} ORDER BY n DESC, {tag_col} ASC LIMIT 100"
            ));

            // Build params dynamically.
            let prefix_str = prefix.unwrap_or_default();
            let project_str = project_id.map(|id| id.to_string()).unwrap_or_default();

            let mut stmt = conn.prepare(&sql).map_err(DaemonError::Database)?;

            let rows: Vec<TagWithCount> = match (has_prefix, has_project) {
                (true, true) => stmt
                    .query_map(rusqlite::params![&prefix_str, &project_str], |row| {
                        Ok(TagWithCount {
                            tag: row.get(0)?,
                            count: row.get::<_, i64>(1)? as u32,
                        })
                    })
                    .map_err(DaemonError::Database)?
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(DaemonError::Database)?,
                (true, false) => stmt
                    .query_map(rusqlite::params![&prefix_str], |row| {
                        Ok(TagWithCount {
                            tag: row.get(0)?,
                            count: row.get::<_, i64>(1)? as u32,
                        })
                    })
                    .map_err(DaemonError::Database)?
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(DaemonError::Database)?,
                (false, true) => stmt
                    .query_map(rusqlite::params![&project_str], |row| {
                        Ok(TagWithCount {
                            tag: row.get(0)?,
                            count: row.get::<_, i64>(1)? as u32,
                        })
                    })
                    .map_err(DaemonError::Database)?
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(DaemonError::Database)?,
                (false, false) => stmt
                    .query_map([], |row| {
                        Ok(TagWithCount {
                            tag: row.get(0)?,
                            count: row.get::<_, i64>(1)? as u32,
                        })
                    })
                    .map_err(DaemonError::Database)?
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(DaemonError::Database)?,
            };

            Ok(rows)
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::EventBus;
    use crate::config::{Config, RuntimeConfig};
    use crate::session::{CompletedSession, SessionManager};
    use crate::store::Store;
    use rsi_common::types::{
        ContextUsageConfidence, Session, SessionKind, SessionProvider, SessionStatus,
    };
    use tempfile::TempDir;

    fn manager() -> (SessionManager, TempDir) {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("rsi.db");
        let store = Store::open(&db_path).expect("open store");
        let config = Config::from_env();
        let runtime_config = RuntimeConfig::from_config(&config);
        let manager = SessionManager::new(
            std::sync::Arc::new(EventBus::new(16)),
            store,
            false,
            dir.path().join("daemon.sock"),
            None,
            Vec::new(),
            runtime_config,
            dir.path().join("sandboxes"),
        )
        .expect("manager");
        (manager, dir)
    }

    fn bare_session(id: Uuid) -> Session {
        let now = chrono::Utc::now();
        Session {
            context_fill_pct: None,
            id,
            status: SessionStatus::Completed,
            session_kind: SessionKind::Standard,
            provider: SessionProvider::default(),
            context_usage_confidence: ContextUsageConfidence::default(),
            rotation_depth: 0,
            retry_attempt: None,
            max_retries: None,
            created_at: now,
            updated_at: now,
            pinned_at: None,
            testing_needed_at: None,
            rotation_disabled_at: None,
            query: "test".to_string(),
            title: None,
            agent_role: None,
            epic_spawn_ordinal: None,
            description: None,
            short_summary: None,
            working_dir: std::path::PathBuf::from("/tmp"),
            git_branch: None,
            model: None,
            claude_session_id: None,
            project_id: None,
            continued_from: None,
            tag: String::new(),
            tags: Vec::new(),
            parent_id: None,
            lead_session_id: None,
            is_eval: false,
            handoff_filepath: None,
            active_task: None,
            group_id: None,
            scheduled_job_id: None,
            stop_reason: None,
            cost_usd: None,
            duration_ms: None,
            num_turns: None,
            input_tokens: None,
            output_tokens: None,
            context_window: None,
            resolved_context_budget: None,
            total_input_tokens: None,
            total_output_tokens: None,
            total_cache_creation_tokens: None,
            total_cache_read_tokens: None,
            daemon_input_tokens: None,
            daemon_output_tokens: None,
            pipeline_artifact: None,
            workflow_id: None,
            workflow_id_override: None,
            pending_question: None,
            pending_archive: false,
            effort: None,
            issue_identifier: None,
            issue_url: None,
            issue_tracker_id: None,
            rating: None,
            harness_version_hash: None,
            test_passed: None,
            clippy_passed: None,
            turn_count: None,
            retry_count: None,
            approval_wait_ms: None,
            work_time_ms: None,
            approval_started_at: None,
            sandbox_kind: None,
            sandbox_root: None,
            sandbox_branch: None,
            sandbox_cleanup_state: None,
            capability_class: None,
            topology_node_id: None,
            topology_iteration: 0,
            provider_cli_version: None,
            provider_capabilities: Vec::new(),
            thinking_tokens: None,
            service_tier: None,
            cache_creation_1h_tokens: None,
            cache_creation_5m_tokens: None,
            permission_denial_count: None,
            subagent_stats_json: None,
            queued_turn_count: None,
            terminal_reason: None,
        }
    }

    async fn insert_session(manager: &SessionManager, session: Session) {
        let store = manager.store.clone();
        let s = session.clone();
        tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.insert_session(&s)
        })
        .await
        .expect("join")
        .expect("insert");
        manager.completed.write().await.insert(
            session.id,
            CompletedSession {
                session,
                events: Vec::new(),
                turn_metrics: Vec::new(),
                retry_cancel: None,
                retry_fired_at: None,
                superseded_by_retry: None,
                events_hydrated: true,
            },
        );
    }

    fn get_tags_for(manager: &SessionManager, session_id: Uuid) -> Vec<String> {
        let store = manager.store.clone();
        let id_str = session_id.to_string();
        let (tags, _) = tokio::task::block_in_place(|| {
            let store = store.blocking_lock();
            let mut stmt = store
                .conn
                .prepare("SELECT tag FROM session_tags WHERE session_id = ?1 ORDER BY tag ASC")
                .unwrap();
            let tags: Vec<String> = stmt
                .query_map(rusqlite::params![&id_str], |r| r.get(0))
                .unwrap()
                .collect::<std::result::Result<_, _>>()
                .unwrap();
            let legacy: String = store
                .conn
                .query_row(
                    "SELECT tag FROM sessions WHERE id = ?1",
                    rusqlite::params![&id_str],
                    |r| r.get(0),
                )
                .unwrap_or_default();
            (tags, legacy)
        });
        tags
    }

    fn get_legacy_tag(manager: &SessionManager, session_id: Uuid) -> String {
        let store = manager.store.clone();
        let id_str = session_id.to_string();
        tokio::task::block_in_place(|| {
            let store = store.blocking_lock();
            store
                .conn
                .query_row(
                    "SELECT tag FROM sessions WHERE id = ?1",
                    rusqlite::params![&id_str],
                    |r| r.get(0),
                )
                .unwrap_or_default()
        })
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_update_session_tags_empty_rejected() {
        let (manager, _dir) = manager();
        let id = Uuid::new_v4();
        insert_session(&manager, bare_session(id)).await;

        let err = manager.update_session_tags(id, vec![]).await.unwrap_err();
        match err {
            DaemonError::InvalidParam(msg) => assert!(msg.contains("tags_required")),
            other => panic!("expected InvalidParam, got {:?}", other),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_update_session_tags_replaces_full_set() {
        let (manager, _dir) = manager();
        let id = Uuid::new_v4();
        insert_session(&manager, bare_session(id)).await;

        manager
            .update_session_tags(id, vec!["alpha".to_string(), "beta".to_string()])
            .await
            .unwrap();
        manager
            .update_session_tags(id, vec!["gamma".to_string()])
            .await
            .unwrap();

        let tags = get_tags_for(&manager, id);
        assert_eq!(tags, vec!["gamma"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_update_session_tags_normalizes_each_input() {
        let (manager, _dir) = manager();
        let id = Uuid::new_v4();
        insert_session(&manager, bare_session(id)).await;

        manager
            .update_session_tags(id, vec!["FOO".to_string(), "Bar Baz".to_string()])
            .await
            .unwrap();

        let tags = get_tags_for(&manager, id);
        assert_eq!(tags, vec!["bar-baz", "foo"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_update_session_tags_dedupes() {
        let (manager, _dir) = manager();
        let id = Uuid::new_v4();
        insert_session(&manager, bare_session(id)).await;

        manager
            .update_session_tags(
                id,
                vec!["foo".to_string(), "Foo".to_string(), "FOO".to_string()],
            )
            .await
            .unwrap();

        let tags = get_tags_for(&manager, id);
        assert_eq!(tags, vec!["foo"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_update_session_tags_malformed_atomic_reject() {
        let (manager, _dir) = manager();
        let id = Uuid::new_v4();
        insert_session(&manager, bare_session(id)).await;

        // Pre-populate with "good".
        manager
            .update_session_tags(id, vec!["good".to_string()])
            .await
            .unwrap();

        // Attempt to replace with ["good", "BAD!"] — should fail atomically.
        let result = manager
            .update_session_tags(id, vec!["good".to_string(), "BAD!".to_string()])
            .await;
        assert!(result.is_err());

        // Original tags must be unchanged.
        let tags = get_tags_for(&manager, id);
        assert_eq!(tags, vec!["good"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_update_session_tags_syncs_legacy_tag_column() {
        let (manager, _dir) = manager();
        let id = Uuid::new_v4();
        insert_session(&manager, bare_session(id)).await;

        manager
            .update_session_tags(id, vec!["zebra".to_string(), "apple".to_string()])
            .await
            .unwrap();

        let legacy = get_legacy_tag(&manager, id);
        assert_eq!(legacy, "apple"); // "apple" < "zebra" alphabetically
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_add_session_tag_idempotent() {
        let (manager, _dir) = manager();
        let id = Uuid::new_v4();
        insert_session(&manager, bare_session(id)).await;

        manager
            .add_session_tag(id, "foo".to_string())
            .await
            .unwrap();
        manager
            .add_session_tag(id, "foo".to_string())
            .await
            .unwrap(); // second add — should be fine

        let tags = get_tags_for(&manager, id);
        assert_eq!(tags, vec!["foo"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_remove_session_tag_idempotent_when_absent() {
        let (manager, _dir) = manager();
        let id = Uuid::new_v4();
        insert_session(&manager, bare_session(id)).await;

        // Remove a tag that was never added — must be Ok.
        manager
            .remove_session_tag(id, "nope".to_string())
            .await
            .unwrap();

        let tags = get_tags_for(&manager, id);
        assert!(tags.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_remove_session_tag_recomputes_legacy_column() {
        let (manager, _dir) = manager();
        let id = Uuid::new_v4();
        insert_session(&manager, bare_session(id)).await;

        manager
            .update_session_tags(id, vec!["apple".to_string(), "zebra".to_string()])
            .await
            .unwrap();

        manager
            .remove_session_tag(id, "apple".to_string())
            .await
            .unwrap();

        let legacy = get_legacy_tag(&manager, id);
        assert_eq!(legacy, "zebra");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_remove_last_session_tag_clears_legacy_column() {
        let (manager, _dir) = manager();
        let id = Uuid::new_v4();
        insert_session(&manager, bare_session(id)).await;

        manager
            .update_session_tags(id, vec!["only".to_string()])
            .await
            .unwrap();
        manager
            .remove_session_tag(id, "only".to_string())
            .await
            .unwrap();

        let legacy = get_legacy_tag(&manager, id);
        assert_eq!(legacy, "");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_list_tags_orders_by_count_desc() {
        let (manager, _dir) = manager();

        // Session A: "a" tag x3, "b" tag x1, "c" tag x5
        for _ in 0..3 {
            let id = Uuid::new_v4();
            insert_session(&manager, bare_session(id)).await;
            manager
                .update_session_tags(id, vec!["a".to_string()])
                .await
                .unwrap();
        }
        {
            let id = Uuid::new_v4();
            insert_session(&manager, bare_session(id)).await;
            manager
                .update_session_tags(id, vec!["b".to_string()])
                .await
                .unwrap();
        }
        for _ in 0..5 {
            let id = Uuid::new_v4();
            insert_session(&manager, bare_session(id)).await;
            manager
                .update_session_tags(id, vec!["c".to_string()])
                .await
                .unwrap();
        }

        let tags = manager.list_tags(None, None).await.unwrap();
        let names: Vec<&str> = tags.iter().map(|t| t.tag.as_str()).collect();
        assert_eq!(names, vec!["c", "a", "b"]);
        assert_eq!(tags[0].count, 5);
        assert_eq!(tags[1].count, 3);
        assert_eq!(tags[2].count, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_list_tags_filters_by_prefix() {
        let (manager, _dir) = manager();

        for tag in &["alpha", "alpine", "beta"] {
            let id = Uuid::new_v4();
            insert_session(&manager, bare_session(id)).await;
            manager
                .update_session_tags(id, vec![tag.to_string()])
                .await
                .unwrap();
        }

        let tags = manager
            .list_tags(Some("alp".to_string()), None)
            .await
            .unwrap();
        let names: Vec<&str> = tags.iter().map(|t| t.tag.as_str()).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"alpine"));
        assert!(!names.contains(&"beta"));
        assert_eq!(tags.len(), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_list_tags_filters_by_project() {
        let (manager, _dir) = manager();

        let project_a = Uuid::new_v4();
        let project_b = Uuid::new_v4();

        // Insert two sessions in project_a with "tag-a".
        for _ in 0..2 {
            let id = Uuid::new_v4();
            let mut s = bare_session(id);
            s.project_id = Some(project_a);
            insert_session(&manager, s).await;
            manager
                .update_session_tags(id, vec!["tag-a".to_string()])
                .await
                .unwrap();
        }

        // Insert one session in project_b with "tag-b".
        {
            let id = Uuid::new_v4();
            let mut s = bare_session(id);
            s.project_id = Some(project_b);
            insert_session(&manager, s).await;
            manager
                .update_session_tags(id, vec!["tag-b".to_string()])
                .await
                .unwrap();
        }

        let tags = manager.list_tags(None, Some(project_a)).await.unwrap();
        let names: Vec<&str> = tags.iter().map(|t| t.tag.as_str()).collect();
        assert!(names.contains(&"tag-a"));
        assert!(!names.contains(&"tag-b"));
    }
}
