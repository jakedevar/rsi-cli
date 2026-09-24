use std::collections::HashSet;
use std::sync::LazyLock;

/// English stop words that add no search value.
/// Loaded once as a static HashSet for O(1) lookup.
///
/// Includes articles, pronouns, common verbs, prepositions, conjunctions,
/// vague time references, and vague noun references.
///
/// This is English-only per the master plan scoping decision (Jake's primary language).
static STOP_WORDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    let words: &[&str] = &[
        // Articles and determiners
        "a",
        "an",
        "the",
        "this",
        "that",
        "these",
        "those",
        // Pronouns
        "i",
        "me",
        "my",
        "we",
        "our",
        "you",
        "your",
        "he",
        "she",
        "it",
        "they",
        "them",
        // Common verbs
        "is",
        "are",
        "was",
        "were",
        "be",
        "been",
        "being",
        "have",
        "has",
        "had",
        "do",
        "does",
        "did",
        "will",
        "would",
        "could",
        "should",
        "can",
        "may",
        "might",
        // Prepositions
        "in",
        "on",
        "at",
        "to",
        "for",
        "of",
        "with",
        "by",
        "from",
        "about",
        "into",
        "through",
        "during",
        "before",
        "after",
        "above",
        "below",
        "between",
        "under",
        "over",
        // Conjunctions
        "and",
        "or",
        "but",
        "if",
        "then",
        "because",
        "as",
        "while",
        "when",
        "where",
        "what",
        "which",
        "who",
        "how",
        "why",
        // Time references (vague, not useful for FTS)
        "yesterday",
        "today",
        "tomorrow",
        "earlier",
        "later",
        "recently",
        "ago",
        "just",
        "now",
        // Vague references
        "thing",
        "things",
        "stuff",
        "something",
        "anything",
        "everything",
        "nothing",
        // Question/request words
        "please",
        "help",
        "find",
        "show",
        "get",
        "tell",
        "give",
    ];
    words.iter().copied().collect()
});

/// Extract meaningful keywords from a user query for FTS search.
///
/// Tokenizes the query, removes English stop words, filters out
/// very short tokens (< 3 ASCII chars), pure numbers, and pure punctuation.
/// Returns deduplicated keywords in order of appearance.
pub fn extract_keywords(query: &str) -> Vec<String> {
    let tokens = tokenize(query);
    let mut keywords = Vec::new();
    let mut seen = HashSet::new();

    for token in tokens {
        if STOP_WORDS.contains(token.as_str()) {
            continue;
        }
        if !is_valid_keyword(&token) {
            continue;
        }
        if seen.contains(&token) {
            continue;
        }
        seen.insert(token.clone());
        keywords.push(token);
    }

    keywords
}

/// Tokenize text into lowercase alphanumeric tokens.
///
/// Splits on whitespace and punctuation, lowercases everything.
fn tokenize(text: &str) -> Vec<String> {
    let normalized = text.to_lowercase();
    let mut tokens = Vec::new();
    let mut current = String::new();

    for ch in normalized.chars() {
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

/// Check if a token is a valid keyword for FTS search.
///
/// Rejects: empty tokens, short ASCII-only words (< 3 chars), pure numbers.
fn is_valid_keyword(token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    // Skip very short English words (likely stop words or fragments)
    if token.chars().all(|c| c.is_ascii_alphabetic()) && token.len() < 3 {
        return false;
    }
    // Skip pure numbers
    if token.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_keywords_basic() {
        assert_eq!(
            extract_keywords("what was the solution for the bug"),
            vec!["solution", "bug"]
        );
    }

    #[test]
    fn test_extract_keywords_conversational() {
        assert_eq!(
            extract_keywords("that thing we discussed about the API"),
            vec!["discussed", "api"]
        );
    }

    #[test]
    fn test_extract_keywords_empty() {
        assert_eq!(extract_keywords(""), Vec::<String>::new());
    }

    #[test]
    fn test_extract_keywords_only_stop_words() {
        assert_eq!(extract_keywords("the is a to for"), Vec::<String>::new());
    }

    #[test]
    fn test_extract_keywords_short_tokens_filtered() {
        assert_eq!(extract_keywords("a do if go"), Vec::<String>::new());
    }

    #[test]
    fn test_extract_keywords_numbers_filtered() {
        assert_eq!(extract_keywords("42 100 2026"), Vec::<String>::new());
    }

    #[test]
    fn test_extract_keywords_mixed_valid_invalid() {
        assert_eq!(
            extract_keywords("the rust memory search 42"),
            vec!["rust", "memory", "search"]
        );
    }

    #[test]
    fn test_extract_keywords_punctuation_stripped() {
        assert_eq!(
            extract_keywords("hello, world! foo-bar_baz"),
            vec!["hello", "world", "foo", "bar_baz"]
        );
    }

    #[test]
    fn test_extract_keywords_deduplication() {
        assert_eq!(extract_keywords("memory memory memory"), vec!["memory"]);
    }

    #[test]
    fn test_extract_keywords_case_insensitive() {
        assert_eq!(extract_keywords("API api Api"), vec!["api"]);
    }

    #[test]
    fn test_extract_keywords_unicode_tokens() {
        assert_eq!(extract_keywords("the uber design"), vec!["uber", "design"]);
    }

    #[test]
    fn test_extract_keywords_underscores_kept() {
        assert_eq!(
            extract_keywords("max_tokens embed_batch"),
            vec!["max_tokens", "embed_batch"]
        );
    }

    #[test]
    fn test_tokenize_basic() {
        assert_eq!(tokenize("hello world"), vec!["hello", "world"]);
    }

    #[test]
    fn test_tokenize_empty() {
        assert_eq!(tokenize(""), Vec::<String>::new());
    }

    #[test]
    fn test_tokenize_punctuation() {
        assert_eq!(
            tokenize("foo.bar,baz!qux"),
            vec!["foo", "bar", "baz", "qux"]
        );
    }

    #[test]
    fn test_tokenize_lowercased() {
        assert_eq!(tokenize("Hello WORLD"), vec!["hello", "world"]);
    }

    #[test]
    fn test_is_valid_keyword_empty() {
        assert!(!is_valid_keyword(""));
    }

    #[test]
    fn test_is_valid_keyword_short_ascii() {
        assert!(!is_valid_keyword("ab"));
    }

    #[test]
    fn test_is_valid_keyword_three_char_ascii() {
        assert!(is_valid_keyword("abc"));
    }

    #[test]
    fn test_is_valid_keyword_pure_numbers() {
        assert!(!is_valid_keyword("123"));
    }

    #[test]
    fn test_is_valid_keyword_alphanumeric() {
        assert!(is_valid_keyword("abc123"));
    }

    #[test]
    fn test_stop_words_contains_expected() {
        assert!(STOP_WORDS.contains("the"));
        assert!(STOP_WORDS.contains("is"));
        assert!(STOP_WORDS.contains("yesterday"));
        assert!(STOP_WORDS.contains("something"));
    }

    #[test]
    fn test_stop_words_does_not_contain_keywords() {
        assert!(!STOP_WORDS.contains("rust"));
        assert!(!STOP_WORDS.contains("memory"));
        assert!(!STOP_WORDS.contains("database"));
    }
}
