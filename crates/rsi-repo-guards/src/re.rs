//! Python `re` on top of `fancy-regex` (backtracking, look-around and
//! back-references, leftmost-first like Python). Patterns are the Python
//! guards' patterns verbatim; a pattern that does not compile is a bug the
//! module tests catch, and a match that exhausts the backtracking budget stops
//! the guard (fail closed) instead of passing silently.

use std::borrow::Cow;
use std::sync::LazyLock;

pub use fancy_regex::Captures;

/// A compiled pattern.
pub struct Re(std::sync::Arc<fancy_regex::Regex>);

/// Compiled patterns by source text. A derivation builds the same dynamic pattern
/// (`\bfn\s+NAME\b`) for thousands of functions; compiling each time dominated a
/// whole-repository run. Bounded so a long-lived test process cannot grow without limit.
static CACHE: LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<fancy_regex::Regex>>>,
> = LazyLock::new(Default::default);
const CACHE_LIMIT: usize = 20_000;

/// A lazily compiled pattern for a `static`.
pub type Lazy = LazyLock<Re>;

/// Generous: Python has no limit, and a refused match must never read as clean.
const BACKTRACK_LIMIT: usize = 200_000_000;

impl Re {
    /// Compile `pattern`; panics when it is not a valid pattern.
    #[must_use]
    pub fn new(pattern: &str) -> Self {
        if let Some(found) = CACHE
            .lock()
            .ok()
            .and_then(|cache| cache.get(pattern).cloned())
        {
            return Self(found);
        }
        let compiled = match fancy_regex::RegexBuilder::new(pattern)
            .backtrack_limit(BACKTRACK_LIMIT)
            .build()
        {
            Ok(re) => std::sync::Arc::new(re),
            Err(error) => panic!("invalid guard pattern {pattern:?}: {error}"),
        };
        if let Ok(mut cache) = CACHE.lock() {
            if cache.len() < CACHE_LIMIT {
                cache.insert(pattern.to_owned(), compiled.clone());
            }
        }
        Self(compiled)
    }

    /// `pattern.search(text, pos)`.
    #[must_use]
    pub fn search_at<'t>(&self, text: &'t str, pos: usize) -> Option<Captures<'t>> {
        if pos > text.len() {
            return None;
        }
        match self.0.captures_from_pos(text, pos) {
            Ok(found) => found,
            Err(error) => panic!("pattern {:?} failed: {error}", self.0.as_str()),
        }
    }

    /// `pattern.search(text)`.
    #[must_use]
    pub fn search<'t>(&self, text: &'t str) -> Option<Captures<'t>> {
        self.search_at(text, 0)
    }

    /// `bool(pattern.search(text))`.
    #[must_use]
    pub fn is_match(&self, text: &str) -> bool {
        self.find_range_at(text, 0).is_some()
    }

    /// The byte range of the first match at or after `pos`; no capture groups
    /// are allocated (the plain-regex fast path of `find_from_pos`).
    #[must_use]
    pub fn find_range_at(&self, text: &str, pos: usize) -> Option<std::ops::Range<usize>> {
        if pos > text.len() {
            return None;
        }
        match self.0.find_from_pos(text, pos) {
            Ok(found) => found.map(|m| m.range()),
            Err(error) => panic!("pattern {:?} failed: {error}", self.0.as_str()),
        }
    }

    /// `pattern.finditer(text)` reduced to the whole-match byte ranges.
    #[must_use]
    pub fn find_ranges(&self, text: &str) -> Vec<std::ops::Range<usize>> {
        let mut found = Vec::new();
        let mut pos = 0;
        while let Some(range) = self.find_range_at(text, pos) {
            pos = if range.is_empty() {
                next_char(text, range.end)
            } else {
                range.end
            };
            found.push(range);
            if pos > text.len() {
                break;
            }
        }
        found
    }

    /// `finditer` for a pattern whose look-around assertions were removed from
    /// the pattern text so the plain (linear-time) engine can run it, with the
    /// removed assertions re-checked on each candidate by `accept`.
    ///
    /// Exact when, at one start offset, the pattern has a single viable parse
    /// (the caller's contract): a rejected candidate then means the original
    /// pattern fails at that start, and the search resumes one character later,
    /// as a backtracking search does.
    #[must_use]
    pub fn find_all_checked<'t>(
        &self,
        text: &'t str,
        accept: &dyn Fn(&Captures<'t>) -> bool,
    ) -> Vec<Captures<'t>> {
        let mut found = Vec::new();
        let mut pos = 0;
        while let Some(caps) = self.search_at(text, pos) {
            let whole = caps.get(0).map_or(pos..pos, |m| m.range());
            if !accept(&caps) {
                pos = next_char(text, whole.start);
                continue;
            }
            pos = if whole.is_empty() {
                next_char(text, whole.end)
            } else {
                whole.end
            };
            found.push(caps);
            if pos > text.len() {
                break;
            }
        }
        found
    }

    /// The first accepted match (see `find_all_checked`).
    #[must_use]
    pub fn search_checked<'t>(
        &self,
        text: &'t str,
        accept: &dyn Fn(&Captures<'t>) -> bool,
    ) -> Option<Captures<'t>> {
        let mut pos = 0;
        while let Some(caps) = self.search_at(text, pos) {
            if accept(&caps) {
                return Some(caps);
            }
            pos = next_char(text, caps.get(0).map_or(pos, |m| m.start()));
        }
        None
    }

    /// `pattern.finditer(text)`.
    #[must_use]
    pub fn find_all<'t>(&self, text: &'t str) -> Vec<Captures<'t>> {
        let mut found = Vec::new();
        let mut pos = 0;
        while let Some(caps) = self.search_at(text, pos) {
            let whole = caps.get(0).map_or(pos..pos, |m| m.range());
            pos = if whole.is_empty() {
                next_char(text, whole.end)
            } else {
                whole.end
            };
            found.push(caps);
            if pos > text.len() {
                break;
            }
        }
        found
    }

    /// `pattern.findall(text)` for a pattern with one group: the group texts.
    #[must_use]
    pub fn findall1(&self, text: &str) -> Vec<String> {
        self.find_all(text)
            .iter()
            .map(|caps| group(caps, 1).to_string())
            .collect()
    }

    /// `pattern.sub(rep, text)` with a function replacement.
    #[must_use]
    pub fn sub_with(&self, text: &str, rep: &dyn Fn(&Captures<'_>) -> String) -> String {
        let mut out = String::with_capacity(text.len());
        let mut last = 0;
        for caps in self.find_all(text) {
            let whole = caps.get(0).map_or(0..0, |m| m.range());
            out.push_str(&text[last..whole.start]);
            out.push_str(&rep(&caps));
            last = whole.end;
        }
        out.push_str(&text[last..]);
        out
    }

    /// `pattern.sub(literal, text)` (no group references in `literal`).
    #[must_use]
    pub fn sub<'t>(&self, text: &'t str, literal: &str) -> Cow<'t, str> {
        if self.search(text).is_none() {
            return Cow::Borrowed(text);
        }
        Cow::Owned(self.sub_with(text, &|_| literal.to_string()))
    }
}

/// Group `index` of a match, `""` when it did not take part (Python's `None`).
#[must_use]
pub fn group<'t>(caps: &Captures<'t>, index: usize) -> &'t str {
    caps.get(index).map_or("", |m| m.as_str())
}

/// Start offset of the whole match.
#[must_use]
pub fn start(caps: &Captures<'_>) -> usize {
    caps.get(0).map_or(0, |m| m.start())
}

/// End offset of the whole match.
#[must_use]
pub fn end(caps: &Captures<'_>) -> usize {
    caps.get(0).map_or(0, |m| m.end())
}

/// The offset of the character after the one at `at` (or `at + 1` at the end).
#[must_use]
pub fn next_char(text: &str, at: usize) -> usize {
    text[at.min(text.len())..]
        .chars()
        .next()
        .map_or(at + 1, |c| at + c.len_utf8())
}

/// `re.escape` for a literal inside a pattern.
#[must_use]
pub fn escape(text: &str) -> String {
    fancy_regex::escape(text).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookbehind_backref_and_finditer_follow_python() {
        let quad = Re::new(r"(?<![0-9.])([0-9]{1,3}(?:\.[0-9]{1,3}){3})(?![0-9])(?!\.[0-9])");
        assert_eq!(
            quad.findall1("a 10.2.3.4 b 10.2.3.4.5 c 10.0.0.1"),
            vec!["10.2.3.4", "10.0.0.1"]
        );
        let raw = Re::new(r#"\br(#*)".*?"\1"#);
        assert_eq!(
            group(&raw.search(r###"x r##"a"#b"## y"###).unwrap(), 0),
            r###"r##"a"#b"##"###
        );
        let word = Re::new(r"(?<![\w.])set_var");
        assert_eq!(word.find_all("é.set_var éset_var set_var").len(), 1);
        assert_eq!(Re::new(r"x*").find_all("axxb").len(), 4);
    }

    #[test]
    fn range_and_checked_searches_follow_finditer() {
        let x = Re::new(r"x*");
        assert_eq!(x.find_ranges("axxb"), vec![0..0, 1..3, 3..3, 4..4]);
        assert_eq!(x.find_range_at("axxb", 1), Some(1..3));
        assert_eq!(x.find_range_at("ab", 3), None);
        assert!(x.is_match(""));
        assert!(!Re::new("y").is_match("xxx"));
        // A rejected candidate resumes one character after its start.
        let ab = Re::new(r"(a+)b");
        let long = |c: &Captures<'_>| group(c, 1).len() > 1;
        let found = ab.find_all_checked("ab aab ab aaab", &long);
        let spans: Vec<&str> = found.iter().map(|c| group(c, 0)).collect();
        assert_eq!(spans, vec!["aab", "aaab"]);
        assert_eq!(
            ab.search_checked("ab ab aab", &long).map(|c| start(&c)),
            Some(6)
        );
        assert!(ab.search_checked("ab ab", &long).is_none());
    }
}
