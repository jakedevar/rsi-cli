use crate::error::Result;
use crate::memory::store::MemoryStore;
use crate::memory::types::str_to_memory_source;

use super::ScoredChunk;

/// Build a safe FTS5 MATCH query from raw user input.
///
/// Tokenizes the input into alphanumeric words, strips all `"` characters
/// from each token, wraps each token in double quotes, and joins with ` AND `.
///
/// Returns `None` if no valid tokens remain after filtering.
pub fn build_fts_query(raw: &str) -> Option<String> {
    let tokens = tokenize_for_fts(raw);
    if tokens.is_empty() {
        return None;
    }
    let quoted: Vec<String> = tokens
        .iter()
        .map(|t| {
            let cleaned = t.replace('"', "");
            format!("\"{}\"", cleaned)
        })
        .collect();
    Some(quoted.join(" AND "))
}

/// Extract alphanumeric+underscore tokens from input for FTS query building.
/// Lowercases all tokens to match FTS5's default tokenizer behavior.
fn tokenize_for_fts(raw: &str) -> Vec<String> {
    let lowered = raw.to_lowercase();
    let mut tokens = Vec::new();
    let mut current = String::new();

    for ch in lowered.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            current.push(ch);
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }

    tokens
}

/// Convert an FTS5 BM25 rank value to a [0, 1] relevance score.
///
/// FTS5's `bm25()` function returns a negative rank where lower (more negative)
/// means more relevant. We negate it to get a positive value, clamp to >= 0,
/// then apply `1 / (1 + rank)` to produce a score in (0, 1].
pub fn bm25_rank_to_score(rank: f64) -> f64 {
    let normalized = if rank.is_finite() {
        rank.max(0.0)
    } else {
        999.0
    };
    1.0 / (1.0 + normalized)
}

/// Execute an FTS5 keyword search against the `chunks_fts` table.
///
/// Returns an empty vec if FTS5 is not available, the query produces no valid
/// tokens, or the limit is 0.
///
/// The `model` parameter filters chunks by embedding model. When `None`
/// (FTS-only mode, no provider), all models are searched.
///
/// The `project_id` parameter scopes results to a specific project. When
/// `Some(pid)`, only chunks indexed under that project match. When `None`,
/// chunks of every project (and global memory files) are eligible. The
/// filter is applied directly in FTS so query plans stay flat.
pub fn search_keyword(
    store: &MemoryStore,
    query: &str,
    limit: usize,
    model: Option<&str>,
    project_id: Option<&str>,
) -> Result<Vec<ScoredChunk>> {
    if !store.fts_available() || limit == 0 {
        return Ok(vec![]);
    }

    let fts_query = match build_fts_query(query) {
        Some(q) => q,
        None => return Ok(vec![]),
    };

    // Build SQL conditionally — both `model` and `project_id` are optional.
    // Bind positions are numbered as: ?1=fts_query, then model (if any),
    // then project_id (if any), then limit (last).
    let mut where_clause = String::from("chunks_fts MATCH ?1");
    let mut next_idx: usize = 2;
    let model_idx = if model.is_some() {
        where_clause.push_str(&format!(" AND model = ?{next_idx}"));
        let idx = next_idx;
        next_idx += 1;
        Some(idx)
    } else {
        None
    };
    let project_idx = if project_id.is_some() {
        where_clause.push_str(&format!(" AND project_id = ?{next_idx}"));
        let idx = next_idx;
        next_idx += 1;
        Some(idx)
    } else {
        None
    };
    let limit_idx = next_idx;

    let sql = format!(
        "SELECT id, path, source, start_line, end_line, text, \
                bm25(chunks_fts) AS rank \
           FROM chunks_fts \
          WHERE {where_clause} \
          ORDER BY rank ASC \
          LIMIT ?{limit_idx}"
    );

    let conn = store.conn();
    let mut stmt = conn.prepare(&sql)?;

    let map_row =
        |row: &rusqlite::Row| -> rusqlite::Result<(String, String, String, u32, u32, String, f64)> {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        };

    let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    param_values.push(Box::new(fts_query.clone()));
    if let (Some(_), Some(m)) = (model_idx, model) {
        param_values.push(Box::new(m.to_string()));
    }
    if let (Some(_), Some(pid)) = (project_idx, project_id) {
        param_values.push(Box::new(pid.to_string()));
    }
    param_values.push(Box::new(limit as u32));

    let params_ref: Vec<&dyn rusqlite::types::ToSql> =
        param_values.iter().map(|b| b.as_ref()).collect();

    let raw_rows: Vec<(String, String, String, u32, u32, String, f64)> = stmt
        .query_map(params_ref.as_slice(), map_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut results = Vec::new();
    for (id, path, source_str, start_line, end_line, text, rank) in raw_rows {
        // BM25 returns negative ranks; negate for bm25_rank_to_score
        let score = bm25_rank_to_score(-rank);
        let source = str_to_memory_source(&source_str)?;
        results.push(ScoredChunk {
            id,
            path,
            source,
            start_line,
            end_line,
            text,
            score,
            vector_score: 0.0,
            text_score: score,
        });
    }

    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Pure function tests (build_fts_query) ---

    #[test]
    fn test_build_fts_query_basic() {
        assert_eq!(
            build_fts_query("hello world"),
            Some(r#""hello" AND "world""#.to_string())
        );
    }

    #[test]
    fn test_build_fts_query_empty() {
        assert_eq!(build_fts_query(""), None);
    }

    #[test]
    fn test_build_fts_query_only_punctuation() {
        assert_eq!(build_fts_query("!@#$%"), None);
    }

    #[test]
    fn test_build_fts_query_quotes_stripped() {
        assert_eq!(
            build_fts_query(r#"hello "world""#),
            Some(r#""hello" AND "world""#.to_string())
        );
    }

    #[test]
    fn test_build_fts_query_fts_operators_neutralized() {
        assert_eq!(
            build_fts_query("rust NOT memory"),
            Some(r#""rust" AND "not" AND "memory""#.to_string())
        );
    }

    #[test]
    fn test_build_fts_query_column_filter_neutralized() {
        assert_eq!(
            build_fts_query("text:secret"),
            Some(r#""text" AND "secret""#.to_string())
        );
    }

    #[test]
    fn test_build_fts_query_near_operator_neutralized() {
        assert_eq!(
            build_fts_query("NEAR(a, b)"),
            Some(r#""near" AND "a" AND "b""#.to_string())
        );
    }

    #[test]
    fn test_build_fts_query_single_word() {
        assert_eq!(build_fts_query("rust"), Some(r#""rust""#.to_string()));
    }

    #[test]
    fn test_build_fts_query_lowercased() {
        assert_eq!(
            build_fts_query("Rust Memory"),
            Some(r#""rust" AND "memory""#.to_string())
        );
    }

    #[test]
    fn test_build_fts_query_unicode() {
        let result = build_fts_query("uber design").unwrap();
        assert!(result.contains("uber"));
        assert!(result.contains("design"));
    }

    // --- Pure function tests (bm25_rank_to_score) ---

    #[test]
    fn test_bm25_rank_to_score_zero() {
        assert!((bm25_rank_to_score(0.0) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_bm25_rank_to_score_one() {
        assert!((bm25_rank_to_score(1.0) - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_bm25_rank_to_score_nine() {
        assert!((bm25_rank_to_score(9.0) - 0.1).abs() < f64::EPSILON);
    }

    #[test]
    fn test_bm25_rank_to_score_negative() {
        // Negative rank clamped to 0 -> score 1.0
        assert!((bm25_rank_to_score(-5.0) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_bm25_rank_to_score_infinity() {
        let score = bm25_rank_to_score(f64::INFINITY);
        assert!((score - 1.0 / 1000.0).abs() < 0.001);
    }

    #[test]
    fn test_bm25_rank_to_score_nan() {
        let score = bm25_rank_to_score(f64::NAN);
        assert!((score - 1.0 / 1000.0).abs() < 0.001);
    }

    // --- Database-dependent tests (search_keyword) ---

    fn setup_fts_store(chunks: &[(&str, &str, &str, u32, u32, &str, &str)]) -> MemoryStore {
        let store = MemoryStore::open_in_memory().unwrap();
        if !store.fts_available() {
            return store;
        }
        for (id, path, source, start, end, text, model) in chunks {
            store
                .conn()
                .execute(
                    "INSERT INTO chunks_fts (id, path, source, start_line, end_line, text, model) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    rusqlite::params![id, path, source, start, end, text, model],
                )
                .unwrap();
        }
        store
    }

    /// Variant that also seeds `project_id` in FTS, for scope-filter tests.
    fn setup_fts_store_with_project(
        chunks: &[(&str, &str, &str, u32, u32, &str, &str, Option<&str>)],
    ) -> MemoryStore {
        let store = MemoryStore::open_in_memory().unwrap();
        if !store.fts_available() {
            return store;
        }
        for (id, path, source, start, end, text, model, project_id) in chunks {
            store
                .conn()
                .execute(
                    "INSERT INTO chunks_fts (id, path, source, start_line, end_line, text, model, project_id) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    rusqlite::params![id, path, source, start, end, text, model, project_id],
                )
                .unwrap();
        }
        store
    }

    #[test]
    fn test_search_keyword_empty_query() {
        let store = setup_fts_store(&[]);
        let results = search_keyword(&store, "", 10, None, None).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_keyword_fts_unavailable() {
        // We can't easily make fts unavailable in tests since open_in_memory
        // enables it. But if it's available, test with zero limit instead.
        let store = MemoryStore::open_in_memory().unwrap();
        let results = search_keyword(&store, "test", 0, None, None).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_keyword_zero_limit() {
        let store = setup_fts_store(&[]);
        let results = search_keyword(&store, "test", 0, None, None).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_keyword_with_results() {
        let store = setup_fts_store(&[
            (
                "c1",
                "memory/test.md",
                "memory",
                1,
                10,
                "rust programming language",
                "nomic",
            ),
            (
                "c2",
                "memory/test.md",
                "memory",
                11,
                20,
                "python programming language",
                "nomic",
            ),
            (
                "c3",
                "memory/other.md",
                "memory",
                1,
                5,
                "rust memory safety",
                "nomic",
            ),
        ]);

        if !store.fts_available() {
            return; // Skip on systems without FTS5
        }

        let results = search_keyword(&store, "rust", 10, None, None).unwrap();
        assert!(!results.is_empty());
        // All results should contain "rust"
        for r in &results {
            assert!(r.text.to_lowercase().contains("rust"));
        }
        // Scores should be in (0, 1]
        for r in &results {
            assert!(r.score > 0.0 && r.score <= 1.0);
            assert!((r.text_score - r.score).abs() < f64::EPSILON);
            assert!(r.vector_score == 0.0);
        }
    }

    #[test]
    fn test_search_keyword_no_matches() {
        let store = setup_fts_store(&[(
            "c1",
            "memory/test.md",
            "memory",
            1,
            10,
            "rust programming",
            "nomic",
        )]);

        if !store.fts_available() {
            return;
        }

        let results = search_keyword(&store, "xyznonexistent", 10, None, None).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_keyword_model_filter() {
        let store = setup_fts_store(&[
            (
                "c1",
                "memory/test.md",
                "memory",
                1,
                10,
                "rust programming",
                "nomic",
            ),
            (
                "c2",
                "memory/test.md",
                "memory",
                11,
                20,
                "rust safety",
                "openai",
            ),
        ]);

        if !store.fts_available() {
            return;
        }

        // With model filter: only nomic
        let results = search_keyword(&store, "rust", 10, Some("nomic"), None).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].text.contains("programming"));

        // Without model filter: both
        let results = search_keyword(&store, "rust", 10, None, None).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_search_keyword_project_filter() {
        // Chunks indexed under two distinct project scopes plus one with
        // NULL project_id (global memory file). Project-scoped search must
        // return only the matching project's row.
        let store = setup_fts_store_with_project(&[
            (
                "c_a",
                "sessions/a.md",
                "sessions",
                1,
                10,
                "shared_keyword alpha-only",
                "nomic",
                Some("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"),
            ),
            (
                "c_b",
                "sessions/b.md",
                "sessions",
                1,
                10,
                "shared_keyword beta-only",
                "nomic",
                Some("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb"),
            ),
            (
                "c_g",
                "memory/global.md",
                "memory",
                1,
                10,
                "shared_keyword global-note",
                "nomic",
                None,
            ),
        ]);

        if !store.fts_available() {
            return;
        }

        // Project A scope returns only the A row, excluding B and the global file.
        let res_a = search_keyword(
            &store,
            "shared_keyword",
            10,
            None,
            Some("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"),
        )
        .unwrap();
        assert_eq!(res_a.len(), 1);
        assert!(res_a[0].text.contains("alpha-only"));

        // Project B scope returns only the B row.
        let res_b = search_keyword(
            &store,
            "shared_keyword",
            10,
            None,
            Some("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb"),
        )
        .unwrap();
        assert_eq!(res_b.len(), 1);
        assert!(res_b[0].text.contains("beta-only"));

        // Unscoped search returns all three rows.
        let res_all = search_keyword(&store, "shared_keyword", 10, None, None).unwrap();
        assert_eq!(res_all.len(), 3);
    }
}
