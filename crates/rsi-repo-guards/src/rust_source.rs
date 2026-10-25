//! Offset-preserving Rust lexer shared by the S3 source guards.
use crate::re::{self, Re};

/// Hide comments and literals while preserving byte offsets and newlines.
/// Lifetimes remain code; nested block comments and hashed raw strings do not.
pub fn mask(source: &str, path: &str) -> Result<String, String> {
    let mut bytes = source.as_bytes().to_vec();
    for (start, end, _) in literals(source, path)? {
        for b in &mut bytes[start..end] {
            if *b != b'\n' {
                *b = b' ';
            }
        }
    }
    String::from_utf8(bytes).map_err(|e| e.to_string())
}

/// Literal/comment spans; the boolean distinguishes strings from comments/chars.
pub fn literals(source: &str, path: &str) -> Result<Vec<(usize, usize, bool)>, String> {
    let chars = Re::new(r"'(?:\\(?:x[0-9a-fA-F]{2}|u\{[0-9a-fA-F_]+\}|.)|[^\\'\n])'");
    let b = source.as_bytes();
    let mut spans = Vec::new();
    let mut at = 0;
    while at < b.len() {
        let start = at;
        let mut string = false;
        if source[at..].starts_with("//") {
            at = source[at..].find('\n').map_or(b.len(), |n| at + n);
        } else if source[at..].starts_with("/*") {
            at += 2;
            let mut depth = 1;
            while at < b.len() && depth > 0 {
                if b[at..].starts_with(b"/*") {
                    depth += 1;
                    at += 2;
                } else if b[at..].starts_with(b"*/") {
                    depth -= 1;
                    at += 2;
                } else {
                    at += 1;
                }
            }
            if depth != 0 {
                return Err(format!("{path}: unterminated block comment"));
            }
        } else {
            let prefix = if b[at..].starts_with(b"br") || b[at..].starts_with(b"rb") {
                2
            } else if b[at] == b'r' {
                1
            } else {
                0
            };
            let boundary = at == 0
                || source[..at]
                    .chars()
                    .next_back()
                    .is_none_or(|c| !c.is_alphanumeric() && c != '_');
            let mut quote = at + prefix;
            while quote < b.len() && b[quote] == b'#' {
                quote += 1;
            }
            if prefix > 0 && boundary && b.get(quote) == Some(&b'"') {
                let terminator = format!("\"{}", "#".repeat(quote - at - prefix));
                at = source[quote + 1..]
                    .find(&terminator)
                    .map(|n| quote + 1 + n + terminator.len())
                    .ok_or_else(|| format!("{path}: unterminated raw string"))?;
                string = true;
            } else if b[at] == b'"' {
                at += 1;
                loop {
                    if at >= b.len() {
                        return Err(format!("{path}: unterminated string"));
                    }
                    if b[at] == b'\\' {
                        at += 2;
                    } else if b[at] == b'"' {
                        at += 1;
                        break;
                    } else {
                        at += 1;
                    }
                }
                string = true;
            } else if b[at] == b'\'' {
                if let Some(m) = chars.search_at(source, at).filter(|m| re::start(m) == at) {
                    at = re::end(&m);
                } else {
                    at += 1;
                    continue;
                }
            } else {
                at += source[at..].chars().next().map_or(1, char::len_utf8);
                continue;
            }
        }
        spans.push((start, at, string));
    }
    Ok(spans)
}

/// End of a balanced delimiter group in already masked code.
pub fn closing(code: &str, start: usize) -> Result<usize, String> {
    let mut stack = Vec::new();
    for (at, b) in code.bytes().enumerate().skip(start) {
        match b {
            b'(' => stack.push(b')'),
            b'[' => stack.push(b']'),
            b'{' => stack.push(b'}'),
            _ if stack.last() == Some(&b) => {
                stack.pop();
                if stack.is_empty() {
                    return Ok(at + 1);
                }
            }
            _ => {}
        }
    }
    Err("unbalanced Rust group".into())
}

pub fn line(source: &str, at: usize) -> usize {
    source[..at].bytes().filter(|b| *b == b'\n').count() + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn masking_preserves_offsets_and_lifetimes() {
        let s = "fn f<'a>(s: &'a str) { let x = r##\"{a}\"##; /* nested /* } */ */ let y = '\\''; } // é\n";
        let m = mask(s, "test.rs").unwrap();
        assert_eq!(s.len(), m.len());
        assert!(m.contains("<'a>(s: &'a str)"));
        assert_eq!(m.matches('{').count(), 1);
        assert_eq!(m.matches('}').count(), 1);
        assert!(m.ends_with('\n'));
        for (s, message) in [
            ("/*", "block comment"),
            ("r#\"x", "raw string"),
            ("\"x", "string"),
        ] {
            assert_eq!(
                mask(s, "x.rs").unwrap_err(),
                format!("x.rs: unterminated {message}")
            );
        }
    }
}
