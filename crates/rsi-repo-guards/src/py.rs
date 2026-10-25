//! The pieces of Python semantics the guard ports rely on, so each port reads
//! files, splits lines and prints values exactly as the Python guard did.

use std::fmt::Write as _;
use std::io;
use std::path::{Component, Path, PathBuf};

/// Python `str.isspace()` for one character (Rust's whitespace plus `\x1c`-`\x1f`).
#[must_use]
pub fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
}

/// Python `str.strip()`.
#[must_use]
pub fn strip(s: &str) -> &str {
    s.trim_matches(is_space)
}

/// Python `str.lstrip()`.
#[must_use]
pub fn lstrip(s: &str) -> &str {
    s.trim_start_matches(is_space)
}

/// Python `str.rstrip()`.
#[must_use]
pub fn rstrip(s: &str) -> &str {
    s.trim_end_matches(is_space)
}

/// Python `str.split()` (no separator): runs of whitespace separate fields.
pub fn split_whitespace(s: &str) -> impl Iterator<Item = &str> {
    s.split(is_space).filter(|part| !part.is_empty())
}

/// Python `str.splitlines()`: every Unicode line boundary, `\r\n` as one, no
/// trailing empty line.
#[must_use]
pub fn splitlines(s: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = s.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        let boundary = matches!(
            c,
            '\n' | '\r'
                | '\x0b'
                | '\x0c'
                | '\x1c'
                | '\x1d'
                | '\x1e'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        );
        if !boundary {
            continue;
        }
        lines.push(&s[start..at]);
        let mut end = at + c.len_utf8();
        if c == '\r' && chars.peek().map(|(_, next)| *next) == Some('\n') {
            chars.next();
            end += 1;
        }
        start = end;
    }
    if start < s.len() {
        lines.push(&s[start..]);
    }
    lines
}

/// Python's universal-newline translation on text-mode reads: `\r\n` and a lone
/// `\r` both become `\n`.
#[must_use]
pub fn universal_newlines(s: &str) -> String {
    if !s.contains('\r') {
        return s.to_string();
    }
    s.replace("\r\n", "\n").replace('\r', "\n")
}

/// `open(path, encoding="utf-8", errors="replace").read()`.
pub fn read_text_lossy(path: &Path) -> io::Result<String> {
    let bytes = std::fs::read(path)?;
    Ok(universal_newlines(&String::from_utf8_lossy(&bytes)))
}

/// `open(path, encoding="utf-8").read()`: a decoding error is an `InvalidData` error.
pub fn read_text(path: &Path) -> io::Result<String> {
    let bytes = std::fs::read(path)?;
    let text = String::from_utf8(bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: {error}", path.display()),
        )
    })?;
    Ok(universal_newlines(&text))
}

/// `text.count("\n", 0, end)`.
#[must_use]
pub fn count_newlines(text: &str, end: usize) -> usize {
    text.as_bytes()[..end.min(text.len())]
        .iter()
        .filter(|b| **b == b'\n')
        .count()
}

/// Python `repr()` of a string.
#[must_use]
pub fn repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if is_printable(c) => out.push(c),
            c if (c as u32) < 0x100 => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c if (c as u32) < 0x10000 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => {
                let _ = write!(out, "\\U{:08x}", c as u32);
            }
        }
    }
    out.push(quote);
    out
}

/// Python `str.isprintable()` for one character, closely enough for repr: no
/// control, separator (other than space) or common format character.
fn is_printable(c: char) -> bool {
    if c == ' ' {
        return true;
    }
    !(c.is_control()
        || c.is_whitespace()
        || matches!(c, '\u{ad}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2064}' | '\u{feff}'))
}

/// Python `os.path.normpath` on a POSIX path string.
#[must_use]
pub fn normpath(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let initial = if path.starts_with("//") && !path.starts_with("///") {
        2
    } else {
        usize::from(path.starts_with('/'))
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part != ".." || (initial == 0 && parts.is_empty()) || parts.last() == Some(&"..") {
            parts.push(part);
        } else if !parts.is_empty() {
            parts.pop();
        }
    }
    let joined = format!("{}{}", "/".repeat(initial), parts.join("/"));
    if joined.is_empty() {
        ".".to_string()
    } else {
        joined
    }
}

/// `str(PurePosixPath(path))`: empty and `.` parts dropped, `..` kept.
#[must_use]
pub fn path_str(path: &str) -> String {
    let initial = if path.starts_with("//") && !path.starts_with("///") {
        2
    } else {
        usize::from(path.starts_with('/'))
    };
    let parts: Vec<&str> = path
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    let joined = format!("{}{}", "/".repeat(initial), parts.join("/"));
    if joined.is_empty() {
        ".".to_string()
    } else {
        joined
    }
}

/// The display form of `path` with `/` separators (Python's `as_posix()`).
#[must_use]
pub fn as_posix(path: &Path) -> String {
    let text = path.to_string_lossy();
    if std::path::MAIN_SEPARATOR == '/' {
        text.into_owned()
    } else {
        text.replace('\\', "/")
    }
}

/// `PurePath.suffix`.
#[must_use]
pub fn suffix(name: &str) -> &str {
    match name.rfind('.') {
        Some(at) if at > 0 && at + 1 < name.len() => &name[at..],
        _ => "",
    }
}

/// `os.path.splitext(path)[1]` for a POSIX path string.
#[must_use]
pub fn splitext_ext(path: &str) -> &str {
    let sep = path.rfind('/').map_or(0, |at| at + 1);
    let Some(dot) = path.rfind('.') else {
        return "";
    };
    if dot < sep {
        return "";
    }
    if path[sep..dot].bytes().any(|b| b != b'.') {
        &path[dot..]
    } else {
        ""
    }
}

/// `os.path.basename` (the text after the last `/`).
#[must_use]
pub fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// `Path.resolve()` (non-strict): absolute, symlinks resolved as far as the
/// path exists, `.` and `..` folded.
#[must_use]
pub fn resolve(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    if let Ok(found) = std::fs::canonicalize(&absolute) {
        return found;
    }
    let mut result = PathBuf::new();
    let mut resolved_prefix = true;
    for component in absolute.components() {
        match component {
            Component::ParentDir => {
                result.pop();
            }
            Component::CurDir => {}
            other => {
                result.push(other.as_os_str());
                if resolved_prefix {
                    match std::fs::canonicalize(&result) {
                        Ok(found) => result = found,
                        Err(_) => resolved_prefix = false,
                    }
                }
            }
        }
    }
    result
}

/// Every file under `root` whose name satisfies `keep`, in Python `sorted()`
/// `Path` order (component-wise); directory symlinks are not followed, as
/// `Path.rglob` does not follow them.
#[must_use]
pub fn rglob(root: &Path, keep: &dyn Fn(&str) -> bool) -> Vec<PathBuf> {
    let mut found = Vec::new();
    walk(root, keep, &|_| true, &mut found);
    found.sort();
    found
}

/// [`rglob`] that also skips directories `descend` refuses.
#[must_use]
pub fn rglob_pruned(
    root: &Path,
    keep: &dyn Fn(&str) -> bool,
    descend: &dyn Fn(&str) -> bool,
) -> Vec<PathBuf> {
    let mut found = Vec::new();
    walk(root, keep, descend, &mut found);
    found.sort();
    found
}

fn walk(
    dir: &Path,
    keep: &dyn Fn(&str) -> bool,
    descend: &dyn Fn(&str) -> bool,
    found: &mut Vec<PathBuf>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if descend(&name) {
                walk(&path, keep, descend, found);
            }
        } else if path.is_file() && keep(&name) {
            found.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitlines_matches_python() {
        assert_eq!(
            splitlines("a\nb\r\nc\rd\x0ce\u{2028}f\n"),
            vec!["a", "b", "c", "d", "e", "f"]
        );
        assert_eq!(splitlines(""), Vec::<&str>::new());
        assert_eq!(splitlines("a\n\nb"), vec!["a", "", "b"]);
        assert_eq!(splitlines("a\x1fb"), vec!["a\x1fb"]);
    }

    #[test]
    fn repr_matches_python() {
        assert_eq!(repr("GIT_DIR"), "'GIT_DIR'");
        assert_eq!(repr("it's"), "\"it's\"");
        assert_eq!(repr("a'b\"c"), "'a\\'b\"c'");
        assert_eq!(repr("tab\there\\"), "'tab\\there\\\\'");
        assert_eq!(repr("\u{1}é"), "'\\x01é'");
    }

    #[test]
    fn paths_match_python() {
        assert_eq!(
            normpath("crates/a/src/../../../thoughts/n.md"),
            "thoughts/n.md"
        );
        assert_eq!(normpath("../x/./y"), "../x/y");
        assert_eq!(normpath("/a/../../b"), "/b");
        assert_eq!(path_str("./crates//rsi/"), "crates/rsi");
        assert_eq!(path_str(""), ".");
        assert_eq!(suffix("a.sh"), ".sh");
        assert_eq!(suffix(".sh"), "");
        assert_eq!(suffix("x."), "");
        assert_eq!(splitext_ext("scripts/tool"), "");
        assert_eq!(splitext_ext("scripts/.hidden"), "");
        assert_eq!(splitext_ext("scripts/a.b/tool"), "");
        assert_eq!(splitext_ext("scripts/x."), ".");
        assert_eq!(splitext_ext("scripts/p.py"), ".py");
    }

    #[test]
    fn whitespace_matches_python() {
        assert_eq!(strip("\x1c a \u{a0}"), "a");
        assert_eq!(
            split_whitespace(" a\tb\x1fc ").collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
        assert_eq!(universal_newlines("a\r\nb\rc"), "a\nb\nc");
    }
}
