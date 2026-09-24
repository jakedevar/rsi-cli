//! Tag normalization helper shared by daemon and TUI.

use regex::Regex;
use std::sync::OnceLock;
use thiserror::Error;

/// Matches one or more whitespace characters (for collapsing to hyphen).
static WHITESPACE_RE: OnceLock<Regex> = OnceLock::new();

/// Validates a normalized tag: must start with `[a-z0-9]` and contain only
/// `[a-z0-9\-_/]`. Compiled once per process.
static TAG_RE: OnceLock<Regex> = OnceLock::new();

fn whitespace_re() -> &'static Regex {
    WHITESPACE_RE.get_or_init(|| {
        Regex::new(r"\s+").unwrap_or_else(|_| unreachable!("WHITESPACE_RE is a valid pattern"))
    })
}

fn tag_re() -> &'static Regex {
    TAG_RE.get_or_init(|| {
        Regex::new(r"^[a-z0-9][a-z0-9\-_/]*$")
            .unwrap_or_else(|_| unreachable!("TAG_RE is a valid pattern"))
    })
}

/// Error returned by [`normalize_tag`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TagError {
    /// The input (after trimming) was empty.
    #[error("tag is empty")]
    Empty,
    /// The input does not match the allowed character set after normalization.
    #[error("tag is malformed")]
    Malformed,
}

/// Normalize a raw tag string from user input.
///
/// Steps:
/// 1. Trim surrounding whitespace.
/// 2. Lowercase.
/// 3. Collapse internal whitespace runs into a single `-`.
/// 4. Validate against `^[a-z0-9][a-z0-9\-_/]*$`.
///
/// # Errors
///
/// Returns `TagError::Empty` when the trimmed input is empty, or
/// `TagError::Malformed` when the normalized form fails validation.
pub fn normalize_tag(input: &str) -> Result<String, TagError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(TagError::Empty);
    }

    let lower = trimmed.to_lowercase();
    let collapsed = whitespace_re().replace_all(&lower, "-").into_owned();

    if collapsed.is_empty() {
        return Err(TagError::Empty);
    }

    if !tag_re().is_match(&collapsed) {
        return Err(TagError::Malformed);
    }

    Ok(collapsed)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_lowercase() {
        assert_eq!(normalize_tag("FooBar").unwrap(), "foobar");
    }

    #[test]
    fn test_normalize_trim() {
        assert_eq!(normalize_tag("  foo  ").unwrap(), "foo");
    }

    #[test]
    fn test_normalize_internal_whitespace_to_hyphen() {
        assert_eq!(normalize_tag("foo bar baz").unwrap(), "foo-bar-baz");
        assert_eq!(normalize_tag("foo\t\tbar").unwrap(), "foo-bar");
    }

    #[test]
    fn test_normalize_preserves_allowed_chars() {
        assert_eq!(normalize_tag("foo-bar_baz/qux").unwrap(), "foo-bar_baz/qux");
    }

    #[test]
    fn test_normalize_empty_input_rejected() {
        assert_eq!(normalize_tag(""), Err(TagError::Empty));
        assert_eq!(normalize_tag("   "), Err(TagError::Empty));
    }

    #[test]
    fn test_normalize_malformed_rejected() {
        assert_eq!(normalize_tag("-foo"), Err(TagError::Malformed));
        assert_eq!(normalize_tag("foo!"), Err(TagError::Malformed));
        // "foo bar!" → "foo-bar!" which fails validation
        assert_eq!(normalize_tag("foo bar!"), Err(TagError::Malformed));
    }

    #[test]
    fn test_normalize_idempotent() {
        let inputs = ["clean-tag", "foo/bar", "a1", "my_tag-v2/sub"];
        for input in inputs {
            let once = normalize_tag(input).unwrap();
            let twice = normalize_tag(&once).unwrap();
            assert_eq!(once, twice, "normalize not idempotent for: {input}");
        }
    }
}
