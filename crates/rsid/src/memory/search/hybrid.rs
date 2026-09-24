use std::collections::HashMap;

use super::ScoredChunk;

/// Merge vector search results and keyword search results into a single
/// ranked list using weighted linear combination.
///
/// Score formula: `vector_weight * vector_score + text_weight * text_score`
pub fn merge_hybrid_results(
    vector_results: Vec<ScoredChunk>,
    keyword_results: Vec<ScoredChunk>,
    vector_weight: f64,
    text_weight: f64,
) -> Vec<ScoredChunk> {
    let mut by_id: HashMap<String, ScoredChunk> = HashMap::new();

    // Insert vector results
    for r in vector_results {
        by_id.insert(r.id.clone(), r);
    }

    // Merge keyword results
    for r in keyword_results {
        if let Some(existing) = by_id.get_mut(&r.id) {
            existing.text_score = r.text_score;
            // Use longer snippet if keyword search found a better one
            if !r.text.is_empty() && r.text.len() > existing.text.len() {
                existing.text = r.text;
            }
        } else {
            by_id.insert(r.id.clone(), r);
        }
    }

    // Compute combined scores
    for entry in by_id.values_mut() {
        entry.score = vector_weight * entry.vector_score + text_weight * entry.text_score;
    }

    // Sort: score desc, then path asc, then start_line asc
    let mut results: Vec<ScoredChunk> = by_id.into_values().collect();
    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.start_line.cmp(&b.start_line))
    });

    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::types::MemorySource;

    fn make_chunk(id: &str, score: f64, vector_score: f64, text_score: f64) -> ScoredChunk {
        ScoredChunk {
            id: id.to_string(),
            path: format!("memory/{}.md", id),
            source: MemorySource::Memory,
            start_line: 1,
            end_line: 10,
            text: format!("text for {}", id),
            score,
            vector_score,
            text_score,
        }
    }

    #[test]
    fn test_merge_vector_only() {
        let vector = vec![make_chunk("c1", 0.8, 0.8, 0.0)];
        let keyword = vec![];
        let merged = merge_hybrid_results(vector, keyword, 0.7, 0.3);
        assert_eq!(merged.len(), 1);
        assert!((merged[0].score - 0.7 * 0.8).abs() < f64::EPSILON);
    }

    #[test]
    fn test_merge_keyword_only() {
        let vector = vec![];
        let keyword = vec![make_chunk("c1", 0.6, 0.0, 0.6)];
        let merged = merge_hybrid_results(vector, keyword, 0.7, 0.3);
        assert_eq!(merged.len(), 1);
        assert!((merged[0].score - 0.3 * 0.6).abs() < f64::EPSILON);
    }

    #[test]
    fn test_merge_overlapping_results() {
        let vector = vec![make_chunk("c1", 0.8, 0.8, 0.0)];
        let keyword = vec![make_chunk("c1", 0.6, 0.0, 0.6)];
        let merged = merge_hybrid_results(vector, keyword, 0.7, 0.3);
        assert_eq!(merged.len(), 1);
        let expected = 0.7 * 0.8 + 0.3 * 0.6;
        assert!((merged[0].score - expected).abs() < f64::EPSILON);
    }

    #[test]
    fn test_merge_disjoint_results() {
        let vector = vec![make_chunk("c1", 0.8, 0.8, 0.0)];
        let keyword = vec![make_chunk("c2", 0.6, 0.0, 0.6)];
        let merged = merge_hybrid_results(vector, keyword, 0.7, 0.3);
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn test_merge_empty_both() {
        let merged = merge_hybrid_results(vec![], vec![], 0.7, 0.3);
        assert!(merged.is_empty());
    }

    #[test]
    fn test_merge_score_formula() {
        let vector = vec![make_chunk("c1", 0.0, 0.8, 0.0)];
        let keyword = vec![make_chunk("c1", 0.0, 0.0, 0.6)];
        let merged = merge_hybrid_results(vector, keyword, 0.7, 0.3);
        let expected = 0.7 * 0.8 + 0.3 * 0.6; // = 0.74
        assert!((merged[0].score - expected).abs() < f64::EPSILON);
    }

    #[test]
    fn test_merge_ordering() {
        let vector = vec![
            make_chunk("c1", 0.0, 0.9, 0.0),
            make_chunk("c2", 0.0, 0.5, 0.0),
        ];
        let keyword = vec![];
        let merged = merge_hybrid_results(vector, keyword, 0.7, 0.3);
        assert!(merged[0].score > merged[1].score);
    }

    #[test]
    fn test_merge_tiebreaker_path() {
        let mut c1 = make_chunk("c1", 0.0, 0.5, 0.0);
        c1.path = "b.md".to_string();
        let mut c2 = make_chunk("c2", 0.0, 0.5, 0.0);
        c2.path = "a.md".to_string();
        let merged = merge_hybrid_results(vec![c1, c2], vec![], 0.7, 0.3);
        assert_eq!(merged[0].path, "a.md");
        assert_eq!(merged[1].path, "b.md");
    }

    #[test]
    fn test_merge_tiebreaker_start_line() {
        let mut c1 = make_chunk("c1", 0.0, 0.5, 0.0);
        c1.path = "a.md".to_string();
        c1.start_line = 20;
        let mut c2 = make_chunk("c2", 0.0, 0.5, 0.0);
        c2.path = "a.md".to_string();
        c2.start_line = 10;
        let merged = merge_hybrid_results(vec![c1, c2], vec![], 0.7, 0.3);
        assert_eq!(merged[0].start_line, 10);
        assert_eq!(merged[1].start_line, 20);
    }

    #[test]
    fn test_merge_keyword_updates_snippet() {
        let mut v = make_chunk("c1", 0.0, 0.8, 0.0);
        v.text = "short".to_string();
        let mut k = make_chunk("c1", 0.0, 0.0, 0.6);
        k.text = "longer snippet from keyword search".to_string();
        let merged = merge_hybrid_results(vec![v], vec![k], 0.7, 0.3);
        assert_eq!(merged[0].text, "longer snippet from keyword search");
    }

    #[test]
    fn test_merge_custom_weights() {
        let vector = vec![make_chunk("c1", 0.0, 0.8, 0.0)];
        let keyword = vec![make_chunk("c1", 0.0, 0.0, 0.6)];
        let merged = merge_hybrid_results(vector, keyword, 0.5, 0.5);
        let expected = 0.5 * 0.8 + 0.5 * 0.6; // = 0.7
        assert!((merged[0].score - expected).abs() < f64::EPSILON);
    }

    #[test]
    fn test_merge_zero_vector_weight() {
        let vector = vec![make_chunk("c1", 0.0, 0.8, 0.0)];
        let keyword = vec![make_chunk("c1", 0.0, 0.0, 0.6)];
        let merged = merge_hybrid_results(vector, keyword, 0.0, 1.0);
        assert!((merged[0].score - 0.6).abs() < f64::EPSILON);
    }

    #[test]
    fn test_merge_zero_text_weight() {
        let vector = vec![make_chunk("c1", 0.0, 0.8, 0.0)];
        let keyword = vec![make_chunk("c1", 0.0, 0.0, 0.6)];
        let merged = merge_hybrid_results(vector, keyword, 1.0, 0.0);
        assert!((merged[0].score - 0.8).abs() < f64::EPSILON);
    }
}
