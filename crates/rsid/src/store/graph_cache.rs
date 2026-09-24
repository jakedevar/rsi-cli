//! Graph cache SQLite persistence.
//!
//! Stores generated workflow artifacts for fast retrieval.

use anyhow::Result;
use rsi_graph::cache::{CacheEntry, CacheKey};
use rusqlite::Connection;

/// Ensure the graph_cache table exists.
pub fn init_cache_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS graph_cache (
            key_string TEXT PRIMARY KEY,
            intent_hash TEXT NOT NULL,
            params_hash TEXT NOT NULL,
            topology_version TEXT NOT NULL,
            content_hash TEXT NOT NULL DEFAULT '',
            workflow_json TEXT NOT NULL,
            reasoning TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            hit_count INTEGER NOT NULL DEFAULT 0
        );",
    )?;
    Ok(())
}

/// Load a cache entry by key.
pub fn load_entry(conn: &Connection, key: &CacheKey) -> Result<Option<CacheEntry>> {
    let key_str = key.to_key_string();
    let mut stmt = conn.prepare(
        "SELECT intent_hash, params_hash, topology_version, content_hash, workflow_json, reasoning, created_at, hit_count
         FROM graph_cache WHERE key_string = ?1",
    )?;

    let mut rows = stmt.query_map([&key_str], |row| {
        Ok((
            row.get::<_, String>(0)?, // intent_hash
            row.get::<_, String>(1)?, // params_hash
            row.get::<_, String>(2)?, // topology_version
            row.get::<_, String>(3)?, // content_hash
            row.get::<_, String>(4)?, // workflow_json
            row.get::<_, String>(5)?, // reasoning
            row.get::<_, String>(6)?, // created_at
            row.get::<_, u64>(7)?,    // hit_count
        ))
    })?;

    if let Some(row) = rows.next() {
        let (
            intent_hash,
            params_hash,
            topology_version,
            content_hash,
            workflow_json,
            reasoning,
            created_at,
            hit_count,
        ) = row?;
        let workflow: rsi_graph::format::WorkflowDefinition = serde_json::from_str(&workflow_json)?;
        Ok(Some(CacheEntry {
            key: CacheKey {
                intent_hash,
                params_hash,
                topology_version,
                content_hash,
            },
            workflow,
            reasoning,
            created_at,
            hit_count,
        }))
    } else {
        Ok(None)
    }
}

/// Save a cache entry.
pub fn save_entry(conn: &Connection, entry: &CacheEntry) -> Result<()> {
    let key_str = entry.key.to_key_string();
    let workflow_json = serde_json::to_string(&entry.workflow)?;
    conn.execute(
        "INSERT OR REPLACE INTO graph_cache (key_string, intent_hash, params_hash, topology_version, content_hash, workflow_json, reasoning, created_at, hit_count)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            key_str,
            entry.key.intent_hash,
            entry.key.params_hash,
            entry.key.topology_version,
            entry.key.content_hash,
            workflow_json,
            entry.reasoning,
            entry.created_at,
            entry.hit_count,
        ],
    )?;
    Ok(())
}

/// Clear all cache entries.
pub fn clear_cache(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM graph_cache", [])?;
    Ok(())
}

/// Count cache entries.
pub fn count_entries(conn: &Connection) -> Result<usize> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM graph_cache", [], |row| row.get(0))?;
    Ok(count as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_graph::cache::TOPOLOGY_VERSION;
    use rsi_graph::format::WorkflowDefinition;
    use rusqlite::Connection;

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        init_cache_table(&conn).unwrap();
        conn
    }

    fn make_entry(intent: &str) -> CacheEntry {
        let key = CacheKey::new(intent, intent, None, TOPOLOGY_VERSION);
        CacheEntry {
            key,
            workflow: WorkflowDefinition::new("test-workflow"),
            reasoning: "test reasoning".to_string(),
            created_at: "2026-03-21T00:00:00Z".to_string(),
            hit_count: 0,
        }
    }

    #[test]
    fn init_cache_table_creates_table() {
        let conn = Connection::open_in_memory().unwrap();
        init_cache_table(&conn).unwrap();

        // Verify table exists by querying it
        let count = count_entries(&conn).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn save_and_load_entry_roundtrip() {
        let conn = setup_db();
        let entry = make_entry("build a pipeline");

        save_entry(&conn, &entry).unwrap();

        let loaded = load_entry(&conn, &entry.key).unwrap().unwrap();
        assert_eq!(loaded.key.intent_hash, entry.key.intent_hash);
        assert_eq!(loaded.key.params_hash, entry.key.params_hash);
        assert_eq!(loaded.key.topology_version, entry.key.topology_version);
        // content_hash round-trips through save/load.
        assert_eq!(loaded.key.content_hash, entry.key.content_hash);
        assert!(!loaded.key.content_hash.is_empty());
        assert_eq!(loaded.reasoning, entry.reasoning);
        assert_eq!(loaded.hit_count, entry.hit_count);
        assert_eq!(loaded.workflow.name, "test-workflow");
    }

    #[test]
    fn changed_content_yields_distinct_key_and_cache_miss() {
        let conn = setup_db();
        // Same intent, params, and topology, but different RESOLVED content.
        let fresh_key = CacheKey::new("intent", "resolved content A", None, TOPOLOGY_VERSION);
        let stale_key = CacheKey::new("intent", "resolved content B", None, TOPOLOGY_VERSION);
        assert_ne!(fresh_key.to_key_string(), stale_key.to_key_string());

        let entry = CacheEntry {
            key: fresh_key.clone(),
            workflow: WorkflowDefinition::new("test-workflow"),
            reasoning: "r".to_string(),
            created_at: "2026-03-21T00:00:00Z".to_string(),
            hit_count: 0,
        };
        save_entry(&conn, &entry).unwrap();

        // Unchanged content hits.
        assert!(load_entry(&conn, &fresh_key).unwrap().is_some());
        // Changed content misses (content-staleness invalidation).
        assert!(load_entry(&conn, &stale_key).unwrap().is_none());
    }

    #[test]
    fn legacy_shaped_rows_load_with_defaulted_content_hash() {
        // Simulate a pre-V69 DB: create the table WITHOUT content_hash, insert a
        // legacy row, then apply the strict-superset ALTER (mirrors the V69
        // migration) and confirm the legacy row loads with a blank content_hash
        // rather than being dropped.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE graph_cache (
                key_string TEXT PRIMARY KEY,
                intent_hash TEXT NOT NULL,
                params_hash TEXT NOT NULL,
                topology_version TEXT NOT NULL,
                workflow_json TEXT NOT NULL,
                reasoning TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                hit_count INTEGER NOT NULL DEFAULT 0
            );",
        )
        .unwrap();

        // A legacy key_string has no trailing :content_hash segment; we insert
        // one directly so we can read it back after the migration.
        let workflow_json = serde_json::to_string(&WorkflowDefinition::new("legacy")).unwrap();
        let legacy_key = "ihash:phash:1.0.0".to_string();
        conn.execute(
            "INSERT INTO graph_cache (key_string, intent_hash, params_hash, topology_version, workflow_json, reasoning, created_at, hit_count)
             VALUES (?1, 'ihash', 'phash', '1.0.0', ?2, 'r', '2026-03-21T00:00:00Z', 0)",
            rusqlite::params![legacy_key, workflow_json],
        )
        .unwrap();

        // Apply the strict-superset column add (V69 shape).
        conn.execute_batch(
            "ALTER TABLE graph_cache ADD COLUMN content_hash TEXT NOT NULL DEFAULT '';",
        )
        .unwrap();

        // The legacy row loads with a defaulted (blank) content_hash.
        let key = CacheKey {
            intent_hash: "ihash".to_string(),
            params_hash: "phash".to_string(),
            topology_version: "1.0.0".to_string(),
            content_hash: String::new(),
        };
        // Reconstruct the legacy key_string (no content segment) for lookup.
        let loaded = {
            let mut stmt = conn
                .prepare("SELECT content_hash FROM graph_cache WHERE key_string = ?1")
                .unwrap();
            stmt.query_row([&legacy_key], |row| row.get::<_, String>(0))
                .unwrap()
        };
        assert_eq!(loaded, "");
        // A freshly-computed key (with content) is disjoint from the legacy row.
        assert_ne!(key.to_key_string(), legacy_key);
    }

    #[test]
    fn clear_cache_removes_all_entries() {
        let conn = setup_db();
        save_entry(&conn, &make_entry("a")).unwrap();
        save_entry(&conn, &make_entry("b")).unwrap();
        assert_eq!(count_entries(&conn).unwrap(), 2);

        clear_cache(&conn).unwrap();
        assert_eq!(count_entries(&conn).unwrap(), 0);
    }
}
