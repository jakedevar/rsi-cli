use std::collections::{HashMap, HashSet};

use super::ScoredChunk;

/// Tokenize text into a set of lowercase alphanumeric tokens for Jaccard similarity.
fn tokenize_for_jaccard(text: &str) -> HashSet<String> {
    let lowered = text.to_lowercase();
    let mut tokens = HashSet::new();
    let mut current = String::new();

    for ch in lowered.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            current.push(ch);
        } else if !current.is_empty() {
            tokens.insert(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.insert(current);
    }

    tokens
}

/// Compute Jaccard similarity between two token sets.
///
/// Returns |A intersect B| / |A union B|.
pub fn jaccard_similarity(set_a: &HashSet<String>, set_b: &HashSet<String>) -> f64 {
    if set_a.is_empty() && set_b.is_empty() {
        return 1.0;
    }
    if set_a.is_empty() || set_b.is_empty() {
        return 0.0;
    }

    let (smaller, larger) = if set_a.len() <= set_b.len() {
        (set_a, set_b)
    } else {
        (set_b, set_a)
    };

    let intersection_size = smaller.iter().filter(|t| larger.contains(*t)).count();
    let union_size = set_a.len() + set_b.len() - intersection_size;

    if union_size == 0 {
        return 0.0;
    }

    intersection_size as f64 / union_size as f64
}

/// Compute the MMR score for a single candidate.
///
/// `MMR = lambda * relevance - (1 - lambda) * max_similarity`
#[inline]
pub fn compute_mmr_score(relevance: f64, max_similarity: f64, lambda: f64) -> f64 {
    lambda * relevance - (1.0 - lambda) * max_similarity
}

/// Re-rank search results using Maximal Marginal Relevance (MMR).
///
/// Iteratively selects results that balance relevance with diversity.
pub fn mmr_rerank(results: Vec<ScoredChunk>, lambda: f64) -> Vec<ScoredChunk> {
    if results.len() <= 1 {
        return results;
    }

    let lambda = lambda.clamp(0.0, 1.0);

    if lambda == 1.0 {
        let mut sorted = results;
        sorted.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        return sorted;
    }

    // Pre-tokenize all items
    let token_sets: HashMap<String, HashSet<String>> = results
        .iter()
        .map(|r| (r.id.clone(), tokenize_for_jaccard(&r.text)))
        .collect();

    // Normalize scores to [0, 1]
    let max_score = results
        .iter()
        .map(|r| r.score)
        .fold(f64::NEG_INFINITY, f64::max);
    let min_score = results
        .iter()
        .map(|r| r.score)
        .fold(f64::INFINITY, f64::min);
    let score_range = max_score - min_score;

    let normalize = |score: f64| -> f64 {
        if score_range == 0.0 {
            1.0
        } else {
            (score - min_score) / score_range
        }
    };

    let mut selected: Vec<ScoredChunk> = Vec::with_capacity(results.len());
    let mut remaining: Vec<ScoredChunk> = results;

    while !remaining.is_empty() {
        let mut best_idx = 0;
        let mut best_mmr = f64::NEG_INFINITY;
        let mut best_original_score = f64::NEG_INFINITY;

        for (i, candidate) in remaining.iter().enumerate() {
            let normalized_relevance = normalize(candidate.score);

            let max_sim = if selected.is_empty() {
                0.0
            } else {
                let candidate_tokens = &token_sets[&candidate.id];
                selected
                    .iter()
                    .map(|s| jaccard_similarity(candidate_tokens, &token_sets[&s.id]))
                    .fold(0.0f64, f64::max)
            };

            let mmr_score = compute_mmr_score(normalized_relevance, max_sim, lambda);

            if mmr_score > best_mmr
                || (mmr_score == best_mmr && candidate.score > best_original_score)
            {
                best_mmr = mmr_score;
                best_original_score = candidate.score;
                best_idx = i;
            }
        }

        selected.push(remaining.swap_remove(best_idx));
    }

    selected
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::types::MemorySource;

    fn make_chunk(id: &str, score: f64, text: &str) -> ScoredChunk {
        ScoredChunk {
            id: id.to_string(),
            path: format!("memory/{}.md", id),
            source: MemorySource::Memory,
            start_line: 1,
            end_line: 10,
            text: text.to_string(),
            score,
            vector_score: score,
            text_score: 0.0,
        }
    }

    // --- Jaccard tests ---

    #[test]
    fn test_jaccard_both_empty() {
        let a = HashSet::new();
        let b = HashSet::new();
        assert!((jaccard_similarity(&a, &b) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_jaccard_one_empty() {
        let a: HashSet<String> = ["hello".to_string()].into();
        let b = HashSet::new();
        assert!(jaccard_similarity(&a, &b).abs() < f64::EPSILON);
    }

    #[test]
    fn test_jaccard_identical() {
        let a: HashSet<String> = ["hello".to_string(), "world".to_string()].into();
        let b = a.clone();
        assert!((jaccard_similarity(&a, &b) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_jaccard_disjoint() {
        let a: HashSet<String> = ["hello".to_string()].into();
        let b: HashSet<String> = ["world".to_string()].into();
        assert!(jaccard_similarity(&a, &b).abs() < f64::EPSILON);
    }

    #[test]
    fn test_jaccard_partial_overlap() {
        let a: HashSet<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        let b: HashSet<String> = ["b", "c", "d"].iter().map(|s| s.to_string()).collect();
        // intersection = {b, c} = 2, union = {a, b, c, d} = 4
        assert!((jaccard_similarity(&a, &b) - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_jaccard_subset() {
        let a: HashSet<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
        let b: HashSet<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        // intersection = 2, union = 3
        assert!((jaccard_similarity(&a, &b) - 2.0 / 3.0).abs() < 1e-10);
    }

    // --- tokenize_for_jaccard tests ---

    #[test]
    fn test_tokenize_for_jaccard_basic() {
        let tokens = tokenize_for_jaccard("hello world");
        assert!(tokens.contains("hello"));
        assert!(tokens.contains("world"));
        assert_eq!(tokens.len(), 2);
    }

    #[test]
    fn test_tokenize_for_jaccard_lowercased() {
        let tokens = tokenize_for_jaccard("Hello WORLD");
        assert!(tokens.contains("hello"));
        assert!(tokens.contains("world"));
    }

    #[test]
    fn test_tokenize_for_jaccard_punctuation() {
        let tokens = tokenize_for_jaccard("foo.bar,baz");
        assert!(tokens.contains("foo"));
        assert!(tokens.contains("bar"));
        assert!(tokens.contains("baz"));
    }

    #[test]
    fn test_tokenize_for_jaccard_empty() {
        assert!(tokenize_for_jaccard("").is_empty());
    }

    #[test]
    fn test_tokenize_for_jaccard_dedup() {
        let tokens = tokenize_for_jaccard("hello hello");
        assert_eq!(tokens.len(), 1);
        assert!(tokens.contains("hello"));
    }

    // --- MMR tests ---

    #[test]
    fn test_mmr_rerank_empty() {
        let result = mmr_rerank(vec![], 0.7);
        assert!(result.is_empty());
    }

    #[test]
    fn test_mmr_rerank_single() {
        let input = vec![make_chunk("c1", 0.9, "hello world")];
        let result = mmr_rerank(input, 0.7);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "c1");
    }

    #[test]
    fn test_mmr_rerank_lambda_one() {
        let input = vec![
            make_chunk("c1", 0.5, "text1"),
            make_chunk("c2", 0.9, "text2"),
            make_chunk("c3", 0.7, "text3"),
        ];
        let result = mmr_rerank(input, 1.0);
        assert_eq!(result[0].id, "c2"); // highest score
        assert_eq!(result[1].id, "c3");
        assert_eq!(result[2].id, "c1"); // lowest score
    }

    #[test]
    fn test_mmr_rerank_identical_content() {
        let input = vec![
            make_chunk("c1", 0.9, "same text here"),
            make_chunk("c2", 0.8, "same text here"),
            make_chunk("c3", 0.7, "same text here"),
        ];
        let result = mmr_rerank(input, 0.7);
        // First should be highest scored
        assert_eq!(result[0].id, "c1");
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn test_mmr_rerank_diverse_content() {
        let input = vec![
            make_chunk("c1", 0.9, "rust programming language systems"),
            make_chunk("c2", 0.85, "python machine learning data science"),
            make_chunk("c3", 0.8, "javascript web development frontend"),
        ];
        let result = mmr_rerank(input, 0.7);
        // With diverse content, order should stay close to relevance order
        assert_eq!(result[0].id, "c1");
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn test_mmr_rerank_mixed() {
        let input = vec![
            make_chunk("c1", 0.9, "rust memory safety ownership"),
            make_chunk("c2", 0.85, "rust memory allocation borrow checker"),
            make_chunk("c3", 0.8, "python machine learning neural networks"),
        ];
        let result = mmr_rerank(input, 0.7);
        // c1 first (highest score), c3 may be promoted over c2 due to diversity
        assert_eq!(result[0].id, "c1");
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn test_mmr_rerank_lambda_zero() {
        let input = vec![
            make_chunk("c1", 0.9, "same text"),
            make_chunk("c2", 0.8, "same text"),
            make_chunk("c3", 0.7, "completely different content"),
        ];
        let result = mmr_rerank(input, 0.0);
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn test_mmr_rerank_preserves_all_items() {
        let input = vec![
            make_chunk("c1", 0.9, "text1"),
            make_chunk("c2", 0.8, "text2"),
            make_chunk("c3", 0.7, "text3"),
            make_chunk("c4", 0.6, "text4"),
        ];
        let result = mmr_rerank(input, 0.7);
        assert_eq!(result.len(), 4);
    }

    #[test]
    fn test_mmr_rerank_tiebreaker() {
        let input = vec![
            make_chunk("c1", 0.8, "unique content alpha"),
            make_chunk("c2", 0.9, "unique content beta"),
        ];
        let result = mmr_rerank(input, 0.7);
        // c2 has higher score, should be first
        assert_eq!(result[0].id, "c2");
    }

    // --- compute_mmr_score tests ---

    #[test]
    fn test_compute_mmr_score_basic() {
        let score = compute_mmr_score(0.8, 0.5, 0.7);
        let expected = 0.7 * 0.8 - 0.3 * 0.5; // = 0.56 - 0.15 = 0.41
        assert!((score - expected).abs() < f64::EPSILON);
    }

    #[test]
    fn test_compute_mmr_score_zero_similarity() {
        let score = compute_mmr_score(0.8, 0.0, 0.7);
        let expected = 0.7 * 0.8; // = 0.56
        assert!((score - expected).abs() < f64::EPSILON);
    }

    #[test]
    fn test_compute_mmr_score_max_similarity() {
        let score = compute_mmr_score(0.8, 1.0, 0.7);
        let expected = 0.7 * 0.8 - 0.3 * 1.0; // = 0.56 - 0.3 = 0.26
        assert!((score - expected).abs() < f64::EPSILON);
    }
}
