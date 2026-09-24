use crate::error::Result;
use crate::memory::math::cosine_similarity;
use crate::memory::store::MemoryStore;
use crate::memory::types::str_to_memory_source;

use super::ScoredChunk;

/// Convert an f32 slice to a byte buffer for sqlite-vec binding.
/// sqlite-vec expects embeddings as raw little-endian f32 arrays.
fn query_vec_to_blob(vec: &[f32]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(vec.len() * 4);
    for &val in vec {
        buf.extend_from_slice(&val.to_le_bytes());
    }
    buf
}

/// Parse a JSON-encoded f32 array string into a Vec<f32>.
/// Returns None if the string is empty or malformed.
fn parse_embedding_json(json_str: &str) -> Option<Vec<f32>> {
    if json_str.is_empty() {
        return None;
    }
    serde_json::from_str::<Vec<f32>>(json_str).ok()
}

/// Execute a vector similarity search.
///
/// Two code paths:
/// 1. **sqlite-vec fast path**: Uses `chunks_vec` virtual table with cosine distance.
/// 2. **Cosine fallback**: Loads all chunks, computes cosine similarity in Rust.
///
/// When `project_id` is `Some`, both paths restrict matches to chunks whose
/// `chunks.project_id` matches. When `None`, all chunks are eligible
/// (unscoped / manual search behavior).
pub fn search_vector(
    store: &MemoryStore,
    query_vec: &[f32],
    limit: usize,
    model: &str,
    project_id: Option<&str>,
) -> Result<Vec<ScoredChunk>> {
    if query_vec.is_empty() || limit == 0 {
        return Ok(vec![]);
    }

    if store.vector_available() {
        search_vector_sqlite_vec(store, query_vec, limit, model, project_id)
    } else {
        search_vector_cosine_fallback(store, query_vec, limit, model, project_id)
    }
}

/// sqlite-vec fast path: use ANN via vec_distance_cosine.
fn search_vector_sqlite_vec(
    store: &MemoryStore,
    query_vec: &[f32],
    limit: usize,
    model: &str,
    project_id: Option<&str>,
) -> Result<Vec<ScoredChunk>> {
    let blob = query_vec_to_blob(query_vec);
    let conn = store.conn();

    // The vector path already joins `chunks_vec` back to `chunks` for
    // metadata; the project filter rides on that join (denormalized
    // `chunks.project_id`).
    let (sql, has_project) = if project_id.is_some() {
        (
            "SELECT c.id, c.path, c.start_line, c.end_line, c.text, \
                    c.source, \
                    vec_distance_cosine(v.embedding, ?1) AS dist \
               FROM chunks_vec v \
               JOIN chunks c ON c.id = v.id \
              WHERE c.model = ?2 AND c.project_id = ?3 \
              ORDER BY dist ASC \
              LIMIT ?4",
            true,
        )
    } else {
        (
            "SELECT c.id, c.path, c.start_line, c.end_line, c.text, \
                    c.source, \
                    vec_distance_cosine(v.embedding, ?1) AS dist \
               FROM chunks_vec v \
               JOIN chunks c ON c.id = v.id \
              WHERE c.model = ?2 \
              ORDER BY dist ASC \
              LIMIT ?3",
            false,
        )
    };

    let mut stmt = conn.prepare(sql)?;

    let map_row = |row: &rusqlite::Row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, u32>(2)?,
            row.get::<_, u32>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, f64>(6)?,
        ))
    };

    let rows: Vec<(String, String, u32, u32, String, String, f64)> = if has_project {
        stmt.query_map(
            rusqlite::params![blob, model, project_id.unwrap(), limit as u32],
            map_row,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?
    } else {
        stmt.query_map(rusqlite::params![blob, model, limit as u32], map_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };

    let mut results = Vec::new();
    for (id, path, start_line, end_line, text, source_str, dist) in rows {
        let score = 1.0 - dist;
        let source = str_to_memory_source(&source_str)?;
        results.push(ScoredChunk {
            id,
            path,
            source,
            start_line,
            end_line,
            text,
            score,
            vector_score: score,
            text_score: 0.0,
        });
    }

    Ok(results)
}

/// Cosine fallback: load all chunks with embeddings, compute similarity in Rust.
fn search_vector_cosine_fallback(
    store: &MemoryStore,
    query_vec: &[f32],
    limit: usize,
    model: &str,
    project_id: Option<&str>,
) -> Result<Vec<ScoredChunk>> {
    tracing::debug!("Vector search using cosine fallback (sqlite-vec unavailable)");

    let conn = store.conn();
    let (sql, has_project) = if project_id.is_some() {
        (
            "SELECT id, path, source, start_line, end_line, text, embedding \
               FROM chunks \
              WHERE model = ?1 AND embedding != '' AND project_id = ?2",
            true,
        )
    } else {
        (
            "SELECT id, path, source, start_line, end_line, text, embedding \
               FROM chunks \
              WHERE model = ?1 AND embedding != ''",
            false,
        )
    };
    let mut stmt = conn.prepare(sql)?;

    let map_row = |row: &rusqlite::Row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, u32>(3)?,
            row.get::<_, u32>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
        ))
    };

    let rows: Vec<(String, String, String, u32, u32, String, String)> = if has_project {
        stmt.query_map(rusqlite::params![model, project_id.unwrap()], map_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?
    } else {
        stmt.query_map(rusqlite::params![model], map_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };

    let mut scored: Vec<ScoredChunk> = rows
        .into_iter()
        .filter_map(
            |(id, path, source_str, start_line, end_line, text, embedding_json)| {
                let embedding = parse_embedding_json(&embedding_json)?;
                let sim = cosine_similarity(query_vec, &embedding);
                let source = str_to_memory_source(&source_str).ok()?;
                if sim.is_finite() {
                    Some(ScoredChunk {
                        id,
                        path,
                        source,
                        start_line,
                        end_line,
                        text,
                        score: sim as f64,
                        vector_score: sim as f64,
                        text_score: 0.0,
                    })
                } else {
                    None
                }
            },
        )
        .collect();

    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    scored.truncate(limit);

    Ok(scored)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_vector_store(
        chunks: &[(&str, &str, &str, u32, u32, &str, &str, &str)],
    ) -> MemoryStore {
        let store = MemoryStore::open_in_memory().unwrap();
        for (id, path, source, start, end, text, model, embedding) in chunks {
            store
                .conn()
                .execute(
                    "INSERT INTO chunks (id, path, source, start_line, end_line, hash, model, text, embedding, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, 'test-hash', ?6, ?7, ?8, 0)",
                    rusqlite::params![id, path, source, start, end, model, text, embedding],
                )
                .unwrap();
        }
        store
    }

    #[test]
    fn test_search_vector_empty_query_vec() {
        let store = setup_vector_store(&[]);
        let results = search_vector(&store, &[], 10, "nomic", None).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_vector_zero_limit() {
        let store = setup_vector_store(&[]);
        let results = search_vector(&store, &[1.0, 0.0], 0, "nomic", None).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_vector_cosine_fallback_basic() {
        let store = setup_vector_store(&[
            (
                "c1",
                "memory/test.md",
                "memory",
                1,
                10,
                "rust programming",
                "nomic",
                "[1.0, 0.0, 0.0]",
            ),
            (
                "c2",
                "memory/test.md",
                "memory",
                11,
                20,
                "python programming",
                "nomic",
                "[0.0, 1.0, 0.0]",
            ),
        ]);

        let query = [1.0f32, 0.0, 0.0];
        let results = search_vector(&store, &query, 10, "nomic", None).unwrap();
        assert_eq!(results.len(), 2);
        // c1 should be most similar (identical direction)
        assert!(results[0].score > results[1].score);
        assert!(results[0].text.contains("rust"));
    }

    #[test]
    fn test_search_vector_cosine_fallback_ordering() {
        let store = setup_vector_store(&[
            (
                "c1",
                "a.md",
                "memory",
                1,
                5,
                "exact match",
                "nomic",
                "[1.0, 0.0]",
            ),
            (
                "c2",
                "b.md",
                "memory",
                1,
                5,
                "partial match",
                "nomic",
                "[0.707, 0.707]",
            ),
            (
                "c3",
                "c.md",
                "memory",
                1,
                5,
                "no match",
                "nomic",
                "[0.0, 1.0]",
            ),
        ]);

        let query = [1.0f32, 0.0];
        let results = search_vector(&store, &query, 10, "nomic", None).unwrap();
        assert_eq!(results.len(), 3);
        assert!(results[0].score > results[1].score);
        assert!(results[1].score > results[2].score);
    }

    #[test]
    fn test_search_vector_cosine_fallback_limit() {
        let store = setup_vector_store(&[
            ("c1", "a.md", "memory", 1, 5, "text1", "nomic", "[1.0, 0.0]"),
            ("c2", "b.md", "memory", 1, 5, "text2", "nomic", "[0.5, 0.5]"),
            ("c3", "c.md", "memory", 1, 5, "text3", "nomic", "[0.0, 1.0]"),
        ]);

        let query = [1.0f32, 0.0];
        let results = search_vector(&store, &query, 2, "nomic", None).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_search_vector_cosine_fallback_empty_embeddings() {
        let store = setup_vector_store(&[
            (
                "c1",
                "a.md",
                "memory",
                1,
                5,
                "has embedding",
                "nomic",
                "[1.0, 0.0]",
            ),
            ("c2", "b.md", "memory", 1, 5, "no embedding", "nomic", ""),
        ]);

        let query = [1.0f32, 0.0];
        let results = search_vector(&store, &query, 10, "nomic", None).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].text.contains("has embedding"));
    }

    #[test]
    fn test_search_vector_cosine_fallback_no_chunks() {
        let store = setup_vector_store(&[]);
        let query = [1.0f32, 0.0];
        let results = search_vector(&store, &query, 10, "nomic", None).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_parse_embedding_json_valid() {
        assert_eq!(
            parse_embedding_json("[1.0, 2.0, 3.0]"),
            Some(vec![1.0, 2.0, 3.0])
        );
    }

    #[test]
    fn test_parse_embedding_json_empty() {
        assert_eq!(parse_embedding_json(""), None);
    }

    #[test]
    fn test_parse_embedding_json_malformed() {
        assert_eq!(parse_embedding_json("not json"), None);
    }

    #[test]
    fn test_query_vec_to_blob() {
        let vec = [1.0f32, 2.0f32];
        let blob = query_vec_to_blob(&vec);
        assert_eq!(blob.len(), 8); // 2 floats * 4 bytes
        // Check first float is 1.0 in little-endian
        assert_eq!(&blob[0..4], &1.0f32.to_le_bytes());
        assert_eq!(&blob[4..8], &2.0f32.to_le_bytes());
    }

    #[test]
    fn test_search_vector_model_filter() {
        let store = setup_vector_store(&[
            (
                "c1",
                "a.md",
                "memory",
                1,
                5,
                "nomic chunk",
                "nomic",
                "[1.0, 0.0]",
            ),
            (
                "c2",
                "b.md",
                "memory",
                1,
                5,
                "openai chunk",
                "openai",
                "[1.0, 0.0]",
            ),
        ]);

        let query = [1.0f32, 0.0];
        let results = search_vector(&store, &query, 10, "nomic", None).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].text.contains("nomic"));
    }

    /// Seed chunks with explicit `project_id` so the cosine fallback path can
    /// honor `WHERE project_id = ?`. Bypasses the public insert helpers
    /// because tests want raw control of every column.
    fn setup_vector_store_with_project(
        chunks: &[(&str, &str, &str, u32, u32, &str, &str, &str, Option<&str>)],
    ) -> MemoryStore {
        let store = MemoryStore::open_in_memory().unwrap();
        for (id, path, source, start, end, text, model, embedding, project_id) in chunks {
            store
                .conn()
                .execute(
                    "INSERT INTO chunks (id, path, source, start_line, end_line, hash, model, text, embedding, updated_at, project_id) \
                     VALUES (?1, ?2, ?3, ?4, ?5, 'test-hash', ?6, ?7, ?8, 0, ?9)",
                    rusqlite::params![id, path, source, start, end, model, text, embedding, project_id],
                )
                .unwrap();
        }
        store
    }

    #[test]
    fn test_search_vector_cosine_fallback_project_filter() {
        let store = setup_vector_store_with_project(&[
            (
                "c_a",
                "sessions/a.md",
                "sessions",
                1,
                5,
                "alpha-only",
                "nomic",
                "[1.0, 0.0]",
                Some("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"),
            ),
            (
                "c_b",
                "sessions/b.md",
                "sessions",
                1,
                5,
                "beta-only",
                "nomic",
                "[1.0, 0.0]",
                Some("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb"),
            ),
            (
                "c_g",
                "memory/global.md",
                "memory",
                1,
                5,
                "global-note",
                "nomic",
                "[1.0, 0.0]",
                None,
            ),
        ]);

        let query = [1.0f32, 0.0];
        // Project A scope returns only the A row.
        let res_a = search_vector(
            &store,
            &query,
            10,
            "nomic",
            Some("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"),
        )
        .unwrap();
        assert_eq!(res_a.len(), 1);
        assert!(res_a[0].text.contains("alpha-only"));

        // Project B scope returns only the B row.
        let res_b = search_vector(
            &store,
            &query,
            10,
            "nomic",
            Some("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb"),
        )
        .unwrap();
        assert_eq!(res_b.len(), 1);
        assert!(res_b[0].text.contains("beta-only"));

        // Unscoped returns all three rows.
        let res_all = search_vector(&store, &query, 10, "nomic", None).unwrap();
        assert_eq!(res_all.len(), 3);
    }
}
