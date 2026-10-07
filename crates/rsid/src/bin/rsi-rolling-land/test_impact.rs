//! #1280: the tests an unfiltered `rsid`/`rsid-store` change can observe.
//!
//! A test observes a change when its execution can reach changed code, and
//! every edge of such a call chain names its callee: a path or method call
//! names the function, a type or constant is named where it is used, and an
//! implicit trait call (`Drop`, `Display`, `?`, operators) runs on a value of
//! the impl's self type, which the chain names where the value is made or
//! typed. So the selection is a closure over names, not over `use` edges (an
//! inherent method in `store_support/topology_usage.rs` is called from
//! `topology/` with no `use` of its module, #1280):
//!
//! 1. The changed lines (both sides of `git diff -U0`, comment-only lines
//!    ignored) seed the items that contain them; a changed line outside every
//!    item seeds its whole module subtree.
//! 2. An affected item makes every item that names it affected: its name, an
//!    enum's variants and an inherent associated function (no receiver)
//!    together with its type, `Self` or an alias; a trait impl member also
//!    makes its self type affected, and every member of an `impl T` names `T`.
//!    Rust privacy bounds a private name to its module subtree.
//! 3. The affected `#[test]` functions are the selection.
//!
//! Code that reads sources as text observes changes it does not name: an
//! `include_str!` of a changed source is seeded, a data file seeds every
//! literal that names it, and test code that resolves the crate root
//! (`CARGO_MANIFEST_DIR`, directly or through a test helper) and names a
//! source tree (a source scanner) is seeded for every change under `src/`.
//!
//! Anything the names cannot prove runs every library test of both packages
//! (`Everything`): an unmounted source (migrations), an item-level macro
//! invocation the closure reaches, an undeterminable impl self type, a
//! multiply mounted library source, a closure that finds no test, or more
//! than [`MAX_SELECTED_TESTS`] tests.
//! Found nothing never means run nothing.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

/// The packages whose library tests the shared rsid lib build runs.
pub(crate) const LIB_PACKAGES: [&str; 2] = ["rsid", "rsid-store"];

/// Above this many selected tests the gate runs every library test instead of
/// a filterset of exact names: the whole run then costs about the same, and
/// the filterset stays far below Windows' 32 KiB command line.
pub(crate) const MAX_SELECTED_TESTS: usize = 200;

/// The library tests an unfiltered change runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LibTests {
    /// Every library test of both packages; the reason names why.
    Everything(String),
    /// Exactly these tests (full libtest names); empty only when no library
    /// source changed.
    Tests(BTreeSet<String>),
}

/// The tests of an unfiltered rsid-family change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UnfilteredSelection {
    pub(crate) lib: LibTests,
    /// `(package, "bin:NAME" | "test:NAME")`: a binary or integration test
    /// target whose own sources changed and that declares tests.
    pub(crate) targets: BTreeSet<(String, String)>,
}

/// Path → text of the files the selection reads (every `.rs` file and the
/// manifests under the rsid-family crate directories).
pub(crate) type Tree = BTreeMap<String, String>;

/// One file of a `git diff -U0`: the changed line numbers on each side.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct FileDiff {
    pub(crate) old_path: Option<String>,
    pub(crate) new_path: Option<String>,
    pub(crate) old_lines: Vec<u32>,
    pub(crate) new_lines: Vec<u32>,
    pub(crate) binary: bool,
}

/// The selection for `candidate` against `base`, read from `repo`. `None`
/// means every shard: a crate root, manifest or build script changed, or the
/// diff or a tree could not be read.
pub(crate) fn select(repo: &Path, base: &str, candidate: &str) -> Option<UnfilteredSelection> {
    let mut args = vec![
        "diff",
        "-U0",
        "--no-color",
        "--no-ext-diff",
        "--no-renames",
        "--no-textconv",
        base,
        candidate,
        "--",
    ];
    let pathspecs: Vec<String> = LIB_PACKAGES
        .iter()
        .map(|package| format!("crates/{package}"))
        .collect();
    args.extend(pathspecs.iter().map(String::as_str));
    let output = Command::new("git")
        .args(&args)
        .current_dir(repo)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let diffs = parse_diff(&String::from_utf8_lossy(&output.stdout))?;
    if diffs.is_empty() {
        return Some(UnfilteredSelection {
            lib: LibTests::Tests(BTreeSet::new()),
            targets: BTreeSet::new(),
        });
    }
    let candidate_tree = load_tree(repo, candidate).ok()?;
    let base_tree = load_tree(repo, base).ok()?;
    select_from(&diffs, &base_tree, &candidate_tree)
}

/// The selection for parsed diffs over the two trees (see [`select`]).
pub(crate) fn select_from(
    diffs: &[FileDiff],
    base: &Tree,
    candidate: &Tree,
) -> Option<UnfilteredSelection> {
    let mut targets = BTreeSet::new();
    let mut everything: Option<String> = None;
    let mut lib_changed = false;
    // Paths in the candidate (new side) and base (old side) per diff.
    let mut sides: Vec<(&str, Side, &[u32])> = Vec::new();
    for diff in diffs {
        for (path, side, lines) in [
            (
                diff.old_path.as_deref(),
                Side::Base,
                diff.old_lines.as_slice(),
            ),
            (
                diff.new_path.as_deref(),
                Side::Candidate,
                diff.new_lines.as_slice(),
            ),
        ] {
            let Some(path) = path else { continue };
            let Some((_, rest)) = split_package(path) else {
                continue;
            };
            if matches!(rest, "Cargo.toml" | "build.rs" | "src/lib.rs") {
                return None;
            }
            if diff.binary {
                if rest.starts_with("src/") {
                    everything.get_or_insert_with(|| format!("binary source {path} changed"));
                }
                continue;
            }
            sides.push((path, side, lines));
        }
    }
    let base_program = Program::build(base);
    let candidate_program = Program::build(candidate);
    let mut seeds: Vec<usize> = Vec::new();
    let mut seed_keys: Vec<Key> = Vec::new();
    let mut scanned = false;
    for (path, side, lines) in sides {
        let program = match side {
            Side::Base => &base_program,
            Side::Candidate => &candidate_program,
        };
        let tree = match side {
            Side::Base => base,
            Side::Candidate => candidate,
        };
        let (package, rest) = split_package(path).expect("filtered above");
        // A source scanner observes every change under either crate's src/.
        if rest.starts_with("src/") && !scanned {
            scanned = true;
            if !candidate_program.source_readers.is_empty() {
                lib_changed = true;
                seeds.extend(candidate_program.source_readers.iter().copied());
            }
        }
        // A binary or integration test target that owns the file runs its
        // own tests.
        for owner in program.owners.get(path).into_iter().flatten() {
            if let Some(target) = program.runnable_targets.get(owner) {
                targets.insert((package.to_owned(), target.clone()));
            }
        }
        if !path.ends_with(".rs") {
            // A data file runs what reads it by a literal path; one under
            // src/ that nothing names runs every library test.
            let readers = program.literal_readers(path);
            if readers.is_empty() && rest.starts_with("src/") {
                everything.get_or_insert_with(|| format!("unread source file {path} changed"));
            }
            for reader in readers {
                match reader {
                    Reader::Item(index) => {
                        lib_changed = true;
                        push_seed(
                            program,
                            side,
                            index,
                            &mut seeds,
                            &mut seed_keys,
                            &mut everything,
                        );
                    }
                    Reader::Target(owner) => {
                        if let Some(target) = program.runnable_targets.get(&owner) {
                            targets.insert((package.to_owned(), target.clone()));
                        }
                    }
                }
            }
            continue;
        }
        // Code that reads this source as text (`include_str!("x.rs")`).
        for reader in program.literal_readers(path) {
            match reader {
                Reader::Item(index) => {
                    lib_changed = true;
                    push_seed(
                        program,
                        side,
                        index,
                        &mut seeds,
                        &mut seed_keys,
                        &mut everything,
                    );
                }
                Reader::Target(owner) => {
                    if let Some(target) = program.runnable_targets.get(&owner) {
                        targets.insert((package.to_owned(), target.clone()));
                    }
                }
            }
        }
        let Some(file) = program.lib_files.get(path) else {
            let owned = program.owners.contains_key(path);
            if !owned && rest.starts_with("src/") && side_has_tokens(tree, path, lines) {
                everything.get_or_insert_with(|| format!("unmounted source {path} changed"));
            }
            continue;
        };
        let changed: Vec<u32> = lines
            .iter()
            .copied()
            .filter(|line| file.parsed.token_lines.contains(line))
            .collect();
        if changed.is_empty() {
            continue;
        }
        lib_changed = true;
        for line in changed {
            let containing = program.items_at(path, line);
            if containing.is_empty() {
                // Outside every item (an impl's closing brace, an inner
                // attribute): the whole module subtree it sits in.
                let module = file.module_at(line);
                for index in program.subtree_items(&file.crate_name, &module) {
                    push_seed(
                        program,
                        side,
                        index,
                        &mut seeds,
                        &mut seed_keys,
                        &mut everything,
                    );
                }
                continue;
            }
            for index in containing {
                push_seed(
                    program,
                    side,
                    index,
                    &mut seeds,
                    &mut seed_keys,
                    &mut everything,
                );
            }
        }
    }
    // A path-only library index cannot prove reachability through every
    // module/owner of a reused source (including its descendants). Check
    // both trees: a base-side key otherwise carries only the retained mount.
    if lib_changed {
        everything = everything
            .or_else(|| base_program.mount_error.clone())
            .or_else(|| candidate_program.mount_error.clone());
    }
    let lib = if let Some(reason) = everything {
        LibTests::Everything(reason)
    } else if !lib_changed {
        LibTests::Tests(BTreeSet::new())
    } else {
        match candidate_program.closure(&seeds, seed_keys) {
            Err(reason) => LibTests::Everything(reason),
            Ok(tests) if tests.is_empty() => LibTests::Everything(
                "the changed library code names no test that reaches it".into(),
            ),
            Ok(tests) if tests.len() > MAX_SELECTED_TESTS => LibTests::Everything(format!(
                "{} tests can observe the change (more than {MAX_SELECTED_TESTS})",
                tests.len()
            )),
            Ok(tests) => LibTests::Tests(tests),
        }
    };
    Some(UnfilteredSelection { lib, targets })
}

fn push_seed(
    program: &Program,
    side: Side,
    index: usize,
    seeds: &mut Vec<usize>,
    seed_keys: &mut Vec<Key>,
    everything: &mut Option<String>,
) {
    match side {
        Side::Candidate => seeds.push(index),
        // A base-side item no longer exists as such in the candidate: the
        // candidate items that still name it are affected.
        Side::Base => match program.keys(index) {
            Ok(keys) => seed_keys.extend(keys),
            Err(reason) => {
                everything.get_or_insert(reason);
            }
        },
    }
}

fn side_has_tokens(tree: &Tree, path: &str, lines: &[u32]) -> bool {
    let Some(text) = tree.get(path) else {
        return !lines.is_empty();
    };
    let tokens = tokenize(text);
    let mut token_lines = HashSet::new();
    for token in &tokens {
        for line in token.line..=token.end_line {
            token_lines.insert(line);
        }
    }
    lines.iter().any(|line| token_lines.contains(line))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Base,
    Candidate,
}

/// `crates/<package>/<rest>` for an rsid-family package.
fn split_package(path: &str) -> Option<(&'static str, &str)> {
    LIB_PACKAGES.iter().find_map(|package| {
        path.strip_prefix("crates/")
            .and_then(|rest| rest.strip_prefix(package))
            .and_then(|rest| rest.strip_prefix('/'))
            .map(|rest| (*package, rest))
    })
}

/// Parse `git diff -U0` output. `None` for a shape it cannot read.
pub(crate) fn parse_diff(text: &str) -> Option<Vec<FileDiff>> {
    let mut diffs: Vec<FileDiff> = Vec::new();
    let mut old_line = 0u32;
    let mut new_line = 0u32;
    let mut in_hunk = false;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            in_hunk = false;
            let mut diff = FileDiff::default();
            // Fallback paths for a binary or mode-only change.
            if let Some((old, new)) = rest.split_once(" b/") {
                diff.old_path = old.strip_prefix("a/").map(str::to_owned);
                diff.new_path = Some(new.to_owned());
            }
            diffs.push(diff);
            continue;
        }
        let diff = diffs.last_mut()?;
        if !in_hunk {
            if let Some(path) = line.strip_prefix("--- ") {
                diff.old_path = diff_path(path, "a/")?;
                continue;
            }
            if let Some(path) = line.strip_prefix("+++ ") {
                diff.new_path = diff_path(path, "b/")?;
                continue;
            }
            if line.starts_with("Binary files ") {
                diff.binary = true;
                continue;
            }
            if line.starts_with("new file mode") {
                diff.old_path = None;
                continue;
            }
            if line.starts_with("deleted file mode") {
                diff.new_path = None;
                continue;
            }
        }
        if let Some(rest) = line.strip_prefix("@@ ") {
            let mut parts = rest.split_whitespace();
            let old = parts.next()?.strip_prefix('-')?;
            let new = parts.next()?.strip_prefix('+')?;
            old_line = old.split(',').next()?.parse().ok()?;
            new_line = new.split(',').next()?.parse().ok()?;
            in_hunk = true;
            continue;
        }
        if !in_hunk {
            continue;
        }
        if line.starts_with('-') {
            diff.old_lines.push(old_line);
            old_line += 1;
        } else if line.starts_with('+') {
            diff.new_lines.push(new_line);
            new_line += 1;
        } else if line.starts_with(' ') {
            old_line += 1;
            new_line += 1;
        }
    }
    Some(diffs)
}

/// A `---`/`+++` path: `None` inside for `/dev/null`; a quoted path (one
/// git had to escape) cannot be read.
fn diff_path(path: &str, prefix: &str) -> Option<Option<String>> {
    if path == "/dev/null" {
        return Some(None);
    }
    if path.starts_with('"') {
        return None;
    }
    Some(Some(path.strip_prefix(prefix).unwrap_or(path).to_owned()))
}

/// Every `.rs` file and manifest under the rsid-family crate directories at
/// `rev`, read in one `git cat-file --batch` round trip.
pub(crate) fn load_tree(repo: &Path, rev: &str) -> Result<Tree, String> {
    let mut args = vec!["ls-tree", "-r", "-z", "--name-only", rev, "--"];
    let pathspecs: Vec<String> = LIB_PACKAGES
        .iter()
        .map(|package| format!("crates/{package}"))
        .collect();
    args.extend(pathspecs.iter().map(String::as_str));
    let listing = Command::new("git")
        .args(&args)
        .current_dir(repo)
        .output()
        .map_err(|error| format!("git ls-tree: {error}"))?;
    if !listing.status.success() {
        return Err(format!(
            "git ls-tree {rev}: {}",
            String::from_utf8_lossy(&listing.stderr).trim()
        ));
    }
    let paths: Vec<String> = listing
        .stdout
        .split(|byte| *byte == 0)
        .filter_map(|path| std::str::from_utf8(path).ok())
        .filter(|path| path.ends_with(".rs") || path.ends_with("/Cargo.toml"))
        .map(str::to_owned)
        .collect();
    let request: String = paths.iter().map(|path| format!("{rev}:{path}\n")).collect();
    let mut child = Command::new("git")
        .args(["cat-file", "--batch"])
        .current_dir(repo)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("git cat-file: {error}"))?;
    let mut stdin = child.stdin.take().ok_or("git cat-file: no stdin")?;
    // Write from another thread: the blobs fill stdout long before the
    // request is written.
    let writer = std::thread::spawn(move || stdin.write_all(request.as_bytes()));
    let mut data = Vec::new();
    child
        .stdout
        .take()
        .ok_or("git cat-file: no stdout")?
        .read_to_end(&mut data)
        .map_err(|error| format!("git cat-file: {error}"))?;
    let _ = writer.join();
    child
        .wait()
        .map_err(|error| format!("git cat-file: {error}"))?;
    let mut tree = Tree::new();
    let mut position = 0;
    for path in paths {
        let end = data[position..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|offset| position + offset)
            .ok_or_else(|| format!("git cat-file: truncated at {path}"))?;
        let header = String::from_utf8_lossy(&data[position..end]).into_owned();
        position = end + 1;
        let fields: Vec<&str> = header.split_whitespace().collect();
        if fields.len() != 3 || fields[1] != "blob" {
            continue;
        }
        let size: usize = fields[2]
            .parse()
            .map_err(|_| format!("git cat-file: bad header {header}"))?;
        let blob = data
            .get(position..position + size)
            .ok_or_else(|| format!("git cat-file: truncated blob {path}"))?;
        tree.insert(path, String::from_utf8_lossy(blob).into_owned());
        position += size + 1;
    }
    Ok(tree)
}

// ---------------------------------------------------------------------------
// Tokens

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Ident,
    Punct,
    Str,
    Lifetime,
    Literal,
}

#[derive(Clone, Copy, Debug)]
struct Token<'a> {
    kind: Kind,
    /// The identifier, the punctuation, or a string literal's raw text.
    text: &'a str,
    line: u32,
    end_line: u32,
}

impl Token<'_> {
    fn is(&self, text: &str) -> bool {
        matches!(self.kind, Kind::Ident | Kind::Punct) && self.text == text
    }

    fn ident(&self) -> Option<&str> {
        (self.kind == Kind::Ident).then_some(self.text)
    }
}

fn ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte >= 0x80
}

/// The tokens of one Rust source: comments dropped, string and character
/// literals kept whole, lifetimes told apart from character literals.
fn tokenize(source: &str) -> Vec<Token<'_>> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    let mut line = 1u32;
    let count_lines = |from: usize, to: usize| -> u32 {
        u32::try_from(bytes[from..to].iter().filter(|b| **b == b'\n').count()).unwrap_or(0)
    };
    while index < bytes.len() {
        let byte = bytes[index];
        let next = bytes.get(index + 1).copied();
        if byte == b'\n' {
            line += 1;
            index += 1;
            continue;
        }
        if byte.is_ascii_whitespace() {
            index += 1;
            continue;
        }
        if byte == b'/' && next == Some(b'/') {
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            continue;
        }
        if byte == b'/' && next == Some(b'*') {
            let start = index;
            let mut depth = 0usize;
            while index < bytes.len() {
                if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'*') {
                    depth += 1;
                    index += 2;
                } else if bytes[index] == b'*' && bytes.get(index + 1) == Some(&b'/') {
                    depth -= 1;
                    index += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    index += 1;
                }
            }
            line += count_lines(start, index.min(bytes.len()));
            continue;
        }
        // Raw strings (r"", r#""#, br"", cr""), byte and C strings, byte
        // characters and raw identifiers.
        if matches!(byte, b'r' | b'b' | b'c') {
            let mut cursor = index;
            if matches!(byte, b'b' | b'c') && bytes.get(cursor + 1) == Some(&b'r') {
                cursor += 1;
            }
            if bytes[cursor] == b'r' {
                let mut hashes = 0;
                let mut probe = cursor + 1;
                while bytes.get(probe) == Some(&b'#') {
                    hashes += 1;
                    probe += 1;
                }
                if bytes.get(probe) == Some(&b'"') {
                    let start = index;
                    let mut end = probe + 1;
                    loop {
                        if end >= bytes.len() {
                            break;
                        }
                        if bytes[end] == b'"'
                            && bytes[end + 1..]
                                .iter()
                                .take(hashes)
                                .filter(|b| **b == b'#')
                                .count()
                                == hashes
                        {
                            end += 1 + hashes;
                            break;
                        }
                        end += 1;
                    }
                    let end = end.min(bytes.len());
                    let lines = count_lines(start, end);
                    tokens.push(Token {
                        kind: Kind::Str,
                        text: &source[start..end],
                        line,
                        end_line: line + lines,
                    });
                    line += lines;
                    index = end;
                    continue;
                }
                if byte == b'r'
                    && hashes == 1
                    && bytes
                        .get(probe)
                        .is_some_and(|b| ident_byte(*b) && !b.is_ascii_digit())
                {
                    // r#ident: the identifier without its prefix.
                    let start = probe;
                    let mut end = probe;
                    while end < bytes.len() && ident_byte(bytes[end]) {
                        end += 1;
                    }
                    tokens.push(Token {
                        kind: Kind::Ident,
                        text: &source[start..end],
                        line,
                        end_line: line,
                    });
                    index = end;
                    continue;
                }
            }
            if matches!(byte, b'b' | b'c') && next == Some(b'"') {
                index = push_quoted(source, index, index + 1, b'"', &mut line, &mut tokens);
                continue;
            }
            if byte == b'b' && next == Some(b'\'') {
                index = push_quoted(source, index, index + 1, b'\'', &mut line, &mut tokens);
                continue;
            }
        }
        if byte == b'"' {
            index = push_quoted(source, index, index, b'"', &mut line, &mut tokens);
            continue;
        }
        if byte == b'\'' {
            // A character literal ('a', '\n', 'é') or a lifetime/label ('a).
            let char_len = source[index + 1..].chars().next().map_or(1, char::len_utf8);
            if next == Some(b'\\') || bytes.get(index + 1 + char_len) == Some(&b'\'') {
                index = push_quoted(source, index, index, b'\'', &mut line, &mut tokens);
                continue;
            }
            let start = index;
            index += 1;
            while index < bytes.len() && ident_byte(bytes[index]) {
                index += 1;
            }
            tokens.push(Token {
                kind: Kind::Lifetime,
                text: &source[start..index],
                line,
                end_line: line,
            });
            continue;
        }
        if ident_byte(byte) {
            let start = index;
            while index < bytes.len() && ident_byte(bytes[index]) {
                index += 1;
            }
            tokens.push(Token {
                kind: if byte.is_ascii_digit() {
                    Kind::Literal
                } else {
                    Kind::Ident
                },
                text: &source[start..index],
                line,
                end_line: line,
            });
            continue;
        }
        let width = match (byte, next) {
            (b'-', Some(b'>')) | (b'=', Some(b'>')) | (b':', Some(b':')) => 2,
            _ => source[index..].chars().next().map_or(1, char::len_utf8),
        };
        tokens.push(Token {
            kind: Kind::Punct,
            text: &source[index..index + width],
            line,
            end_line: line,
        });
        index += width;
    }
    tokens
}

/// Push the quoted literal starting at `start` whose opening quote is at
/// `quote_at`; returns the index after it.
fn push_quoted<'a>(
    source: &'a str,
    start: usize,
    quote_at: usize,
    quote: u8,
    line: &mut u32,
    tokens: &mut Vec<Token<'a>>,
) -> usize {
    let bytes = source.as_bytes();
    let mut end = quote_at + 1;
    let mut lines = 0;
    while end < bytes.len() {
        match bytes[end] {
            b'\\' => end += 2,
            b'\n' => {
                lines += 1;
                end += 1;
            }
            byte if byte == quote => {
                end += 1;
                break;
            }
            _ => end += 1,
        }
    }
    let end = end.min(bytes.len());
    // An escaped newline was skipped by the two-byte step: count it too.
    let lines = lines
        .max(u32::try_from(bytes[start..end].iter().filter(|b| **b == b'\n').count()).unwrap_or(0));
    tokens.push(Token {
        kind: if quote == b'"' {
            Kind::Str
        } else {
            Kind::Literal
        },
        text: &source[start..end],
        line: *line,
        end_line: *line + lines,
    });
    *line += lines;
    end
}

/// The value of a plain or raw string literal token (escapes kept as written).
fn literal_value(raw: &str) -> &str {
    let raw = raw.trim_start_matches(['b', 'c', 'r']);
    let raw = raw.trim_matches('#');
    raw.strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(raw)
}

/// Identifiers a string literal can name: inline format captures
/// (`"{NAME}"`, `"{NAME:?}"`) and serde-style attribute paths
/// (`default = "path::to::fn"`).
fn literal_names(value: &str, attribute: bool, names: &mut BTreeSet<String>) {
    if attribute {
        for word in value.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
            if word
                .chars()
                .next()
                .is_some_and(|c| c.is_alphabetic() || c == '_')
            {
                names.insert(word.to_owned());
            }
        }
        return;
    }
    let mut rest = value;
    while let Some(open) = rest.find('{') {
        rest = &rest[open + 1..];
        let end = rest
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        let word = &rest[..end];
        if word
            .chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_')
            && matches!(rest[end..].chars().next(), Some('}' | ':'))
        {
            names.insert(word.to_owned());
        }
    }
}

// ---------------------------------------------------------------------------
// Items

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ItemKind {
    Fn,
    Type,
    Enum,
    Trait,
    Value,
    Macro,
    Use,
    Mod,
    ImplHeader,
    /// An item-level macro invocation: what it expands to is unknown.
    Opaque,
}

#[derive(Clone, Debug)]
struct ImplCtx {
    /// The self type's name; `None` when it cannot be read.
    self_ty: Option<String>,
    /// The trait's name for `impl Trait for T`.
    trait_name: Option<String>,
}

#[derive(Clone, Debug)]
struct Item {
    names: Vec<String>,
    kind: ItemKind,
    /// Inline modules between the file's module and the item.
    inline: Vec<String>,
    start: u32,
    end: u32,
    public: bool,
    test: bool,
    ignored: bool,
    receiver: bool,
    in_trait: bool,
    impl_ctx: Option<ImplCtx>,
    variants: Vec<String>,
    mentions: BTreeSet<String>,
    /// Path-like string literals (a data file the item reads by name).
    literals: Vec<String>,
    /// `use ...::*`: its mentions reach the whole module.
    glob: bool,
    /// A `macro_rules!` definition: the item names its expansion defines,
    /// when they can be read from the body.
    expands_to: Option<Vec<String>>,
    /// An item-level invocation of the named macro.
    invokes: Option<String>,
}

#[derive(Clone, Debug)]
struct ModDecl {
    name: String,
    path_attr: Option<String>,
    inline: Vec<String>,
    line: u32,
    cfg_test: bool,
}

#[derive(Debug, Default)]
struct ParsedFile {
    items: Vec<Item>,
    mods: Vec<ModDecl>,
    /// (path, inline modules, line) of each `include!("path")`.
    includes: Vec<(String, Vec<String>, u32)>,
    /// Line spans of test-only code: `#[cfg(test)]` items, modules and impl
    /// blocks, and test functions.
    test_regions: Vec<(u32, u32)>,
    /// (inline path, first line, last line) of each inline module.
    inline_mods: Vec<(Vec<String>, u32, u32)>,
    token_lines: HashSet<u32>,
}

impl ParsedFile {
    fn in_test(&self, line: u32) -> bool {
        self.test_regions
            .iter()
            .any(|(first, last)| *first <= line && line <= *last)
    }
}

struct Parser<'a> {
    tokens: Vec<Token<'a>>,
    file: ParsedFile,
    /// Whether the item being pushed carries `#[cfg(test)]`.
    cfg_test_now: bool,
}

#[derive(Default)]
struct Pending {
    start: Option<u32>,
    cfg_test: bool,
    public: bool,
    test: bool,
    ignored: bool,
    path_attr: Option<String>,
    mentions: BTreeSet<String>,
}

fn parse_file(source: &str) -> ParsedFile {
    let tokens = tokenize(source);
    let mut file = ParsedFile::default();
    for token in &tokens {
        for line in token.line..=token.end_line {
            file.token_lines.insert(line);
        }
    }
    let mut parser = Parser {
        tokens,
        file,
        cfg_test_now: false,
    };
    let end = parser.tokens.len();
    parser.level(0, end, &[], None, false);
    parser.file
}

impl<'a> Parser<'a> {
    fn token(&self, index: usize) -> Option<&Token<'a>> {
        self.tokens.get(index)
    }

    fn is(&self, index: usize, text: &str) -> bool {
        self.token(index).is_some_and(|token| token.is(text))
    }

    /// The index of the delimiter closing the one at `open`.
    fn close(&self, open: usize) -> usize {
        let mut depth = 0usize;
        for index in open..self.tokens.len() {
            let token = &self.tokens[index];
            if token.kind != Kind::Punct {
                continue;
            }
            match token.text {
                "(" | "[" | "{" => depth += 1,
                ")" | "]" | "}" => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return index;
                    }
                }
                _ => {}
            }
        }
        self.tokens.len().saturating_sub(1)
    }

    /// The end of an item starting its declaration at `from`: the close of
    /// its first `{` block when `braced`, otherwise the `;` that ends it
    /// (bracketed groups and expression blocks skipped).
    fn item_end(&self, from: usize, end: usize, braced: bool) -> usize {
        let mut index = from;
        while index < end {
            let token = &self.tokens[index];
            if token.kind == Kind::Punct {
                match token.text {
                    "{" if braced => return self.close(index),
                    "(" | "[" | "{" => {
                        index = self.close(index) + 1;
                        continue;
                    }
                    ";" => return index,
                    _ => {}
                }
            }
            index += 1;
        }
        end.saturating_sub(1)
    }

    fn mentions(
        &self,
        from: usize,
        to: usize,
        into: &mut BTreeSet<String>,
        literals: &mut Vec<String>,
    ) {
        let last = to.min(self.tokens.len().saturating_sub(1));
        for at in from..=last {
            let token = &self.tokens[at];
            match token.kind {
                Kind::Ident => {
                    into.insert(token.text.to_owned());
                }
                Kind::Str => {
                    let value = literal_value(token.text);
                    literal_names(value, false, into);
                    if !value.is_empty()
                        && value.len() < 256
                        && !value.contains(char::is_whitespace)
                    {
                        literals.push(value.to_owned());
                        if self.included_at(at) {
                            literals.push(format!("{INCLUDED}{value}"));
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Whether the string literal at `at` is the argument of
    /// `include_str!(...)` or `include_bytes!(...)`.
    fn included_at(&self, at: usize) -> bool {
        at >= 3
            && self.is(at - 1, "(")
            && self.is(at - 2, "!")
            && self.tokens[at - 3]
                .ident()
                .is_some_and(|name| matches!(name, "include_str" | "include_bytes"))
    }

    /// Parse the items between `from` and `end` at one nesting level.
    fn level(
        &mut self,
        from: usize,
        end: usize,
        inline: &[String],
        impl_ctx: Option<&ImplCtx>,
        in_trait: bool,
    ) {
        let mut index = from;
        let mut pending = Pending::default();
        while index < end {
            let token = self.tokens[index];
            let start_line = pending.start.unwrap_or(token.line);
            if token.is("#") {
                let inner = self.is(index + 1, "!");
                let open = index + 1 + usize::from(inner);
                if !self.is(open, "[") {
                    index += 1;
                    continue;
                }
                let close = self.close(open);
                if !inner {
                    pending.start.get_or_insert(token.line);
                    self.attribute(open + 1, close, &mut pending);
                }
                index = close + 1;
                continue;
            }
            self.cfg_test_now = pending.cfg_test;
            match token.ident() {
                Some("pub") => {
                    pending.start.get_or_insert(token.line);
                    pending.public = true;
                    index += 1;
                    if self.is(index, "(") {
                        index = self.close(index) + 1;
                    }
                    continue;
                }
                Some("unsafe" | "async" | "default" | "auto") => {
                    pending.start.get_or_insert(token.line);
                    index += 1;
                    continue;
                }
                Some("const")
                    if self
                        .token(index + 1)
                        .and_then(Token::ident)
                        .is_some_and(|next| {
                            matches!(next, "fn" | "unsafe" | "async" | "extern")
                        }) =>
                {
                    pending.start.get_or_insert(token.line);
                    index += 1;
                    continue;
                }
                Some("extern") => {
                    pending.start.get_or_insert(token.line);
                    if self.is(index + 1, "crate") {
                        index = self.item_end(index, end, false) + 1;
                        pending = Pending::default();
                        continue;
                    }
                    if self
                        .token(index + 1)
                        .is_some_and(|next| next.kind == Kind::Str)
                    {
                        if self.is(index + 2, "{") {
                            index = self.close(index + 2) + 1;
                            pending = Pending::default();
                            continue;
                        }
                        index += 2;
                        continue;
                    }
                    index += 1;
                    continue;
                }
                Some("mod") => {
                    let Some(name) = self
                        .token(index + 1)
                        .and_then(Token::ident)
                        .map(str::to_owned)
                    else {
                        index += 1;
                        continue;
                    };
                    let mut path = inline.to_vec();
                    path.push(name.clone());
                    if self.is(index + 2, "{") {
                        let close = self.close(index + 2);
                        let last = self.tokens[close].end_line;
                        self.file.inline_mods.push((path.clone(), start_line, last));
                        if pending.cfg_test {
                            self.file.test_regions.push((start_line, last));
                        }
                        self.level(index + 3, close, &path, None, false);
                        index = close + 1;
                    } else {
                        let last = self.token(index + 2).map_or(token.line, |t| t.end_line);
                        self.file.mods.push(ModDecl {
                            name: name.clone(),
                            path_attr: pending.path_attr.take(),
                            inline: inline.to_vec(),
                            line: token.line,
                            cfg_test: pending.cfg_test,
                        });
                        let mut mentions = std::mem::take(&mut pending.mentions);
                        mentions.insert(name.clone());
                        self.push(Item {
                            names: vec![name],
                            kind: ItemKind::Mod,
                            inline: inline.to_vec(),
                            start: start_line,
                            end: last,
                            public: pending.public,
                            test: false,
                            ignored: false,
                            receiver: false,
                            in_trait: false,
                            impl_ctx: None,
                            variants: Vec::new(),
                            mentions,
                            literals: Vec::new(),
                            glob: false,
                            expands_to: None,
                            invokes: None,
                        });
                        index += 3;
                    }
                    pending = Pending::default();
                    continue;
                }
                Some("impl") => {
                    let (ctx, open) = self.impl_header(index + 1, end);
                    let header_end = self.token(open).map_or(token.line, |t| t.line);
                    let mut mentions = std::mem::take(&mut pending.mentions);
                    let mut literals = Vec::new();
                    self.mentions(
                        index,
                        open.saturating_sub(1).max(index),
                        &mut mentions,
                        &mut literals,
                    );
                    // A changed header changes what its self type does.
                    let names: Vec<String> = ctx.self_ty.iter().cloned().collect();
                    self.push(Item {
                        names,
                        kind: if ctx.self_ty.is_some() {
                            ItemKind::ImplHeader
                        } else {
                            ItemKind::Opaque
                        },
                        inline: inline.to_vec(),
                        start: start_line,
                        end: header_end,
                        public: true,
                        test: false,
                        ignored: false,
                        receiver: false,
                        in_trait: false,
                        impl_ctx: Some(ctx.clone()),
                        variants: Vec::new(),
                        mentions,
                        literals,
                        glob: false,
                        expands_to: None,
                        invokes: None,
                    });
                    if self.is(open, "{") {
                        let close = self.close(open);
                        if pending.cfg_test {
                            self.file
                                .test_regions
                                .push((start_line, self.tokens[close].end_line));
                        }
                        self.level(open + 1, close, inline, Some(&ctx), false);
                        index = close + 1;
                    } else {
                        index = open + 1;
                    }
                    pending = Pending::default();
                    continue;
                }
                Some("use") => {
                    let stop = self.item_end(index, end, false);
                    self.use_item(index, stop, inline, start_line, &mut pending);
                    index = stop + 1;
                    pending = Pending::default();
                    continue;
                }
                Some("macro_rules") if self.is(index + 1, "!") => {
                    let name = self
                        .token(index + 2)
                        .and_then(Token::ident)
                        .map(str::to_owned);
                    let open = index + 3;
                    let close = if self.token(open).is_some_and(|t| t.kind == Kind::Punct) {
                        self.close(open)
                    } else {
                        open
                    };
                    let mut stop = close;
                    if self.is(stop + 1, ";") {
                        stop += 1;
                    }
                    // A macro that writes test functions hides their names.
                    let generates_tests = (open..close).any(|at| {
                        self.is(at, "#") && self.is(at + 1, "[") && self.is(at + 2, "test")
                    });
                    let mut mentions = std::mem::take(&mut pending.mentions);
                    let mut literals = Vec::new();
                    self.mentions(index, stop, &mut mentions, &mut literals);
                    self.push(Item {
                        names: name.into_iter().collect(),
                        kind: if generates_tests {
                            ItemKind::Opaque
                        } else {
                            ItemKind::Macro
                        },
                        inline: inline.to_vec(),
                        start: start_line,
                        end: self.tokens[stop.min(end.saturating_sub(1))].end_line,
                        public: true,
                        test: false,
                        ignored: false,
                        receiver: false,
                        in_trait,
                        impl_ctx: impl_ctx.cloned(),
                        variants: Vec::new(),
                        mentions,
                        literals,
                        glob: false,
                        expands_to: if generates_tests {
                            None
                        } else {
                            self.expansion_names(open, close)
                        },
                        invokes: None,
                    });
                    index = stop + 1;
                    pending = Pending::default();
                    continue;
                }
                Some(
                    keyword @ ("fn" | "struct" | "enum" | "union" | "trait" | "type" | "const"
                    | "static"),
                ) if self.item_name(index, keyword).is_some()
                    || matches!(keyword, "const" | "static") =>
                {
                    index = self.declaration(
                        index,
                        end,
                        keyword,
                        inline,
                        impl_ctx,
                        in_trait,
                        start_line,
                        &mut pending,
                    );
                    pending = Pending::default();
                    continue;
                }
                Some(_) if self.macro_call(index).is_some() => {
                    let (bang, path) = self.macro_call(index).expect("checked");
                    let open = bang + 1;
                    let close = self.close(open);
                    let mut stop = close;
                    if self.is(stop + 1, ";") {
                        stop += 1;
                    }
                    if path == "include" {
                        if let Some(target) = self
                            .token(open + 1)
                            .filter(|t| t.kind == Kind::Str)
                            .map(|t| literal_value(t.text).to_owned())
                        {
                            self.file
                                .includes
                                .push((target, inline.to_vec(), token.line));
                            index = stop + 1;
                            pending = Pending::default();
                            continue;
                        }
                    }
                    let mut mentions = std::mem::take(&mut pending.mentions);
                    let mut literals = Vec::new();
                    self.mentions(index, stop, &mut mentions, &mut literals);
                    self.push(Item {
                        names: Vec::new(),
                        kind: ItemKind::Opaque,
                        inline: inline.to_vec(),
                        start: start_line,
                        end: self.tokens[stop.min(end.saturating_sub(1))].end_line,
                        public: pending.public,
                        test: false,
                        ignored: false,
                        receiver: false,
                        in_trait,
                        impl_ctx: impl_ctx.cloned(),
                        variants: Vec::new(),
                        mentions,
                        literals,
                        glob: false,
                        expands_to: None,
                        invokes: Some(path),
                    });
                    index = stop + 1;
                    pending = Pending::default();
                    continue;
                }
                _ => {}
            }
            if token.is(";") {
                pending = Pending::default();
            } else if token.is("{") || token.is("(") || token.is("[") {
                index = self.close(index) + 1;
                pending = Pending::default();
                continue;
            }
            index += 1;
        }
    }

    /// The item names a `macro_rules!` body (tokens `open..close`) defines,
    /// or `None` when they cannot be read: the body writes items whose names
    /// or kinds come from the caller (`fn $name`, an `item` or `tt`
    /// fragment), implements traits, nests macros, or declares modules.
    fn expansion_names(&self, open: usize, close: usize) -> Option<Vec<String>> {
        const OPAQUE_KEYWORDS: &[&str] = &[
            "enum",
            "trait",
            "union",
            "mod",
            "impl",
            "use",
            "extern",
            "macro_rules",
        ];
        const PLAIN_MACROS: &[&str] = &[
            "stringify",
            "concat",
            "format",
            "vec",
            "matches",
            "assert",
            "assert_eq",
            "assert_ne",
            "debug_assert",
            "panic",
            "write",
            "writeln",
            "line",
            "file",
            "column",
            "module_path",
            "cfg",
            "compile_error",
        ];
        const DECLARATIONS: &[&str] = &["fn", "const", "static", "struct", "type"];
        let mut names = Vec::new();
        for at in open + 1..close {
            let token = self.token(at)?;
            let Some(word) = token.ident() else {
                continue;
            };
            if OPAQUE_KEYWORDS.contains(&word) {
                return None;
            }
            if matches!(word, "item" | "tt") && self.is(at - 1, ":") {
                return None;
            }
            if self.is(at + 1, "!")
                && self
                    .token(at + 2)
                    .is_some_and(|t| t.is("(") || t.is("[") || t.is("{"))
                && !PLAIN_MACROS.contains(&word)
            {
                return None;
            }
            if DECLARATIONS.contains(&word) && !self.is(at - 1, "$") {
                let next = self.token(at + 1)?;
                if next.is("$") {
                    return None;
                }
                if let Some(name) = next.ident()
                    && !matches!(name, "fn" | "unsafe" | "async" | "extern" | "mut" | "dyn")
                {
                    names.push(name.to_owned());
                }
            }
        }
        Some(names)
    }

    /// `path::to::name!` starting at `index`: the `!` index and the last
    /// path segment.
    fn macro_call(&self, index: usize) -> Option<(usize, String)> {
        let mut cursor = index;
        let mut last;
        loop {
            let token = self.token(cursor)?;
            if token.kind != Kind::Ident {
                return None;
            }
            last = token.text.to_owned();
            cursor += 1;
            if self.is(cursor, "::") {
                cursor += 1;
                continue;
            }
            break;
        }
        if self.is(cursor, "!")
            && self
                .token(cursor + 1)
                .is_some_and(|t| t.is("(") || t.is("[") || t.is("{"))
        {
            return Some((cursor, last));
        }
        None
    }

    fn attribute(&self, from: usize, to: usize, pending: &mut Pending) {
        let mut path = Vec::new();
        let mut cursor = from;
        while cursor < to {
            let token = &self.tokens[cursor];
            match token.kind {
                Kind::Ident => path.push(token.text),
                Kind::Punct if token.text == "::" => {}
                _ => break,
            }
            cursor += 1;
        }
        if path.last() == Some(&"test") {
            pending.test = true;
        }
        if path == ["ignore"] {
            pending.ignored = true;
        }
        // `#[cfg(test)]` and `#[cfg(all(test, ...))]`.
        if path == ["cfg"] && self.is(cursor, "(") {
            let plain = self.is(cursor + 1, "test") && self.is(cursor + 2, ")");
            let all = self.is(cursor + 1, "all")
                && self.is(cursor + 2, "(")
                && self.is(cursor + 3, "test")
                && (self.is(cursor + 4, ",") || self.is(cursor + 4, ")"));
            pending.cfg_test |= plain || all;
        }
        if path == ["path"] && self.is(cursor, "=") {
            if let Some(value) = self.token(cursor + 1).filter(|t| t.kind == Kind::Str) {
                pending.path_attr = Some(literal_value(value.text).to_owned());
            }
        }
        for token in &self.tokens[from..to] {
            match token.kind {
                Kind::Ident => {
                    pending.mentions.insert(token.text.to_owned());
                }
                Kind::Str => literal_names(literal_value(token.text), true, &mut pending.mentions),
                _ => {}
            }
        }
    }

    /// The declared name after a keyword; `None` for an anonymous `const _`.
    fn item_name(&self, index: usize, keyword: &str) -> Option<String> {
        let mut at = index + 1;
        if keyword == "static" && self.is(at, "mut") {
            at += 1;
        }
        let name = self.token(at)?.ident()?;
        if keyword == "union" && !self.token(at + 1).is_some_and(|t| t.is("{") || t.is("<")) {
            return None;
        }
        (name != "_").then(|| name.to_owned())
    }

    #[allow(clippy::too_many_arguments)]
    fn declaration(
        &mut self,
        index: usize,
        end: usize,
        keyword: &str,
        inline: &[String],
        impl_ctx: Option<&ImplCtx>,
        in_trait: bool,
        start_line: u32,
        pending: &mut Pending,
    ) -> usize {
        let name = self.item_name(index, keyword);
        let braced = matches!(keyword, "fn" | "struct" | "enum" | "union" | "trait");
        let stop = self.item_end(index + 1, end, braced);
        let mut mentions = std::mem::take(&mut pending.mentions);
        let mut literals = Vec::new();
        self.mentions(index, stop, &mut mentions, &mut literals);
        let receiver = keyword == "fn" && self.has_receiver(index + 2, stop);
        let variants = if keyword == "enum" {
            self.variants(index, stop)
        } else {
            Vec::new()
        };
        let kind = match keyword {
            "fn" => ItemKind::Fn,
            "enum" => ItemKind::Enum,
            "trait" => ItemKind::Trait,
            "struct" | "union" | "type" => ItemKind::Type,
            _ => ItemKind::Value,
        };
        self.push(Item {
            names: name.into_iter().collect(),
            kind,
            inline: inline.to_vec(),
            start: start_line,
            end: self.tokens[stop.min(self.tokens.len() - 1)].end_line,
            public: pending.public,
            test: pending.test && keyword == "fn",
            ignored: pending.ignored,
            receiver,
            in_trait,
            impl_ctx: impl_ctx.cloned(),
            variants,
            mentions,
            literals,
            glob: false,
            expands_to: None,
            invokes: None,
        });
        if keyword == "trait" {
            if let Some(open) = (index..stop).find(|at| self.is(*at, "{")) {
                self.level(open + 1, stop, inline, None, true);
            }
        }
        stop + 1
    }

    /// Whether the first parameter of the `fn` whose name precedes `from`
    /// is a `self` receiver.
    fn has_receiver(&self, from: usize, stop: usize) -> bool {
        let mut cursor = from;
        if self.is(cursor, "<") {
            let mut depth = 0i32;
            while cursor < stop {
                if self.is(cursor, "<") {
                    depth += 1;
                } else if self.is(cursor, ">") {
                    depth -= 1;
                    if depth == 0 {
                        cursor += 1;
                        break;
                    }
                }
                cursor += 1;
            }
        }
        if !self.is(cursor, "(") {
            return false;
        }
        let close = self.close(cursor);
        let mut at = cursor + 1;
        while at < close && !self.is(at, ",") {
            if self.is(at, "self") {
                return true;
            }
            if self.is(at, "(") || self.is(at, "[") {
                at = self.close(at);
            }
            at += 1;
        }
        false
    }

    fn variants(&self, index: usize, stop: usize) -> Vec<String> {
        let Some(open) = (index..stop).find(|at| self.is(*at, "{")) else {
            return Vec::new();
        };
        let mut variants = Vec::new();
        let mut at = open + 1;
        let mut expect = true;
        while at < stop {
            let token = &self.tokens[at];
            if token.is("#") && self.is(at + 1, "[") {
                at = self.close(at + 1) + 1;
                continue;
            }
            if token.is("(") || token.is("{") || token.is("[") {
                at = self.close(at) + 1;
                continue;
            }
            if token.is(",") {
                expect = true;
            } else if expect {
                if let Some(name) = token.ident() {
                    variants.push(name.to_owned());
                }
                expect = false;
            }
            at += 1;
        }
        variants
    }

    /// The impl header from just after `impl` to its `{`: the self type and
    /// trait names, and the index of the `{`.
    fn impl_header(&self, from: usize, end: usize) -> (ImplCtx, usize) {
        let mut cursor = from;
        if self.is(cursor, "<") {
            let mut depth = 0i32;
            while cursor < end {
                if self.is(cursor, "<") {
                    depth += 1;
                } else if self.is(cursor, ">") {
                    depth -= 1;
                    if depth == 0 {
                        cursor += 1;
                        break;
                    }
                }
                cursor += 1;
            }
        }
        let mut open = cursor;
        while open < end && !self.is(open, "{") && !self.is(open, ";") {
            if self.is(open, "(") || self.is(open, "[") {
                open = self.close(open);
            }
            open += 1;
        }
        let mut header = cursor;
        let mut depth = 0i32;
        let mut trait_part: Option<(usize, usize)> = None;
        let mut type_from = cursor;
        let mut header_end = open;
        while header < open {
            if self.is(header, "<") {
                depth += 1;
            } else if self.is(header, ">") {
                depth -= 1;
            } else if depth == 0 && self.is(header, "for") {
                trait_part = Some((cursor, header));
                type_from = header + 1;
            } else if depth == 0 && self.is(header, "where") {
                header_end = header;
                break;
            }
            header += 1;
        }
        let path_name = |from: usize, to: usize| -> Option<String> {
            let mut depth = 0i32;
            let mut last = None;
            for at in from..to {
                let token = &self.tokens[at];
                if token.is("<") {
                    if depth == 0 && last.is_some() {
                        break;
                    }
                    depth += 1;
                } else if token.is(">") {
                    depth -= 1;
                } else if depth == 0
                    && let Some(name) = token.ident()
                    && !matches!(name, "dyn" | "mut" | "crate" | "super" | "self" | "const")
                {
                    last = Some(name.to_owned());
                }
            }
            last.or_else(|| {
                (from..to).find_map(|at| {
                    self.tokens[at]
                        .ident()
                        .filter(|name| !matches!(*name, "dyn" | "mut"))
                        .map(str::to_owned)
                })
            })
        };
        let ctx = ImplCtx {
            self_ty: path_name(type_from, header_end),
            trait_name: trait_part.and_then(|(from, to)| path_name(from, to)),
        };
        (ctx, open)
    }

    fn use_item(
        &mut self,
        index: usize,
        stop: usize,
        inline: &[String],
        start: u32,
        pending: &mut Pending,
    ) {
        let mut names = Vec::new();
        let mut glob = false;
        let mut at = index + 1;
        while at < stop {
            let token = &self.tokens[at];
            if token.is("*") {
                glob = true;
            } else if let Some(name) = token.ident() {
                let next = self.token(at + 1);
                if next.is_some_and(|t| t.is("as")) {
                    if let Some(alias) = self.token(at + 2).and_then(Token::ident) {
                        if alias != "_" {
                            names.push(alias.to_owned());
                        }
                    }
                    at += 3;
                    continue;
                }
                let leaf =
                    next.is_none_or(|t| t.is(",") || t.is("}") || t.is(";")) || at + 1 >= stop;
                if leaf {
                    if name == "self" {
                        // `use a::{self}` imports `a`: everything it names.
                        glob = true;
                    } else {
                        names.push(name.to_owned());
                    }
                }
            }
            at += 1;
        }
        let mut mentions = std::mem::take(&mut pending.mentions);
        let mut literals = Vec::new();
        self.mentions(index, stop, &mut mentions, &mut literals);
        self.push(Item {
            names,
            kind: ItemKind::Use,
            inline: inline.to_vec(),
            start,
            end: self.tokens[stop.min(self.tokens.len() - 1)].end_line,
            public: pending.public,
            test: false,
            ignored: false,
            receiver: false,
            in_trait: false,
            impl_ctx: None,
            variants: Vec::new(),
            mentions,
            literals,
            glob,
            expands_to: None,
            invokes: None,
        });
    }

    fn push(&mut self, item: Item) {
        if self.cfg_test_now || item.test {
            self.file.test_regions.push((item.start, item.end));
        }
        self.file.items.push(item);
    }
}

// ---------------------------------------------------------------------------
// Program: the mounted files of every target and the name closure

/// A compilation target a file belongs to.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Owner {
    Lib(&'static str),
    Bin(&'static str, String),
    Test(&'static str, String),
}

struct LibFile {
    crate_name: &'static str,
    module: String,
    parsed: ParsedFile,
    test: bool,
}

impl LibFile {
    /// The module (with inline modules) a line sits in.
    fn module_at(&self, line: u32) -> String {
        let inline = self
            .parsed
            .inline_mods
            .iter()
            .filter(|(_, first, last)| *first <= line && line <= *last)
            .max_by_key(|(path, _, _)| path.len())
            .map(|(path, _, _)| path.clone())
            .unwrap_or_default();
        join_module(&self.module, &inline)
    }
}

fn join_module(base: &str, inline: &[String]) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if !base.is_empty() {
        parts.push(base);
    }
    parts.extend(inline.iter().map(String::as_str));
    parts.join("::")
}

/// A flattened library item.
struct FlatItem {
    path: String,
    crate_name: &'static str,
    module: String,
    item: Item,
    /// Compiled for tests only.
    in_test: bool,
}

/// Who reads a data file by name.
enum Reader {
    Item(usize),
    Target(Owner),
}

/// One key the closure propagates: a name, optionally qualified by a type
/// that a matching item must also name, visible everywhere or in a module
/// subtree of one crate.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    name: String,
    qualifier: Option<String>,
    scope: Option<(&'static str, String)>,
}

struct Program {
    lib_files: BTreeMap<String, LibFile>,
    /// A path-only index lost a library mount: narrow selection is unsafe.
    mount_error: Option<String>,
    items: Vec<FlatItem>,
    by_file: HashMap<String, Vec<usize>>,
    by_mention: HashMap<String, Vec<usize>>,
    globs: HashMap<(&'static str, String), BTreeSet<String>>,
    aliases: HashMap<String, BTreeSet<String>>,
    local_traits: HashSet<String>,
    owners: BTreeMap<String, BTreeSet<Owner>>,
    /// Target → its `bin:`/`test:` filter, for targets that declare tests and
    /// need no feature.
    runnable_targets: BTreeMap<Owner, String>,
    /// Non-library target files (bins, integration tests) and their texts.
    target_texts: BTreeMap<String, String>,
    /// Test code that reads the crate tree by `CARGO_MANIFEST_DIR` (source
    /// scanners): it observes any source change without naming it.
    source_readers: Vec<usize>,
}

/// The marker of a literal an item compiles in with `include_str!` or
/// `include_bytes!` (kept beside the plain literal).
const INCLUDED: &str = "\u{1}include:";

/// A literal naming a source tree or a Rust source (`src`, `../x/src`,
/// `src/store`, `lib.rs`).
fn source_tree_path(literal: &str) -> bool {
    let literal = literal.trim_end_matches('/');
    literal == "src"
        || literal.ends_with("/src")
        || literal.starts_with("src/")
        || literal.contains("/src/")
        || literal.ends_with(".rs")
}

fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    parts.join("/")
}

fn dirname(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// One file mounted into a target: its module path, its parse, and whether
/// it is compiled for tests only (mounted under `#[cfg(test)]`).
struct Mounted {
    module: String,
    parsed: ParsedFile,
    test: bool,
}

/// Every file mounted from `root` through `mod`, `#[path]` and `include!`,
/// and a reason the path-only index cannot represent all module contexts.
fn mount(tree: &Tree, root: &str) -> (BTreeMap<String, Mounted>, Option<String>) {
    let mut mapped: BTreeMap<String, Mounted> = BTreeMap::new();
    let mut error = None;
    if !tree.contains_key(root) {
        return (mapped, error);
    }
    let mut queue = vec![(root.to_owned(), String::new(), true, false)];
    while let Some((parent, module, is_root, test)) = queue.pop() {
        let parsed = parse_file(tree.get(&parent).map_or("", String::as_str));
        let directory = dirname(&parent).to_owned();
        let file_name = parent.rsplit('/').next().unwrap_or(&parent);
        let own = if is_root || file_name == "mod.rs" {
            directory.clone()
        } else {
            format!("{directory}/{}", file_name.trim_end_matches(".rs"))
        };
        let mut children = Vec::new();
        for declaration in &parsed.mods {
            let inline_dir = if declaration.inline.is_empty() {
                own.clone()
            } else {
                format!("{own}/{}", declaration.inline.join("/"))
            };
            let candidates = match &declaration.path_attr {
                Some(attr) if declaration.inline.is_empty() => {
                    vec![normalize(&format!("{directory}/{attr}"))]
                }
                Some(attr) => vec![normalize(&format!("{inline_dir}/{attr}"))],
                None => vec![
                    normalize(&format!("{inline_dir}/{}.rs", declaration.name)),
                    normalize(&format!("{inline_dir}/{}/mod.rs", declaration.name)),
                ],
            };
            if let Some(target) = candidates.into_iter().find(|path| tree.contains_key(path)) {
                let mut path = declaration.inline.clone();
                path.push(declaration.name.clone());
                let child_test = test || declaration.cfg_test || parsed.in_test(declaration.line);
                children.push((target, join_module(&module, &path), child_test));
            }
        }
        for (include, inline, line) in &parsed.includes {
            let target = normalize(&format!("{directory}/{include}"));
            if tree.contains_key(&target) {
                children.push((
                    target,
                    join_module(&module, inline),
                    test || parsed.in_test(*line),
                ));
            }
        }
        mapped.insert(
            parent,
            Mounted {
                module,
                parsed,
                test,
            },
        );
        for (target, child_module, child_test) in children {
            if mapped.contains_key(&target) || queue.iter().any(|(path, ..)| *path == target) {
                // Also bounds cyclic mounts. Do not choose one conditional
                // mount: it may be inactive on the gate platform.
                error.get_or_insert_with(|| format!("multiply mounted source {target}"));
            } else {
                queue.push((target, child_module, false, child_test));
            }
        }
    }
    (mapped, error)
}

/// The binary and integration test targets of one package: explicit
/// `[[bin]]`/`[[test]]` entries plus Cargo's auto-discovered ones.
fn package_targets(tree: &Tree, package: &'static str) -> Vec<(Owner, String, bool)> {
    let prefix = format!("crates/{package}/");
    let mut targets: Vec<(Owner, String, bool)> = Vec::new();
    let manifest = tree
        .get(&format!("{prefix}Cargo.toml"))
        .and_then(|text| toml::from_str::<toml::Table>(text).ok());
    for (section, is_bin) in [("bin", true), ("test", false)] {
        let entries = manifest
            .as_ref()
            .and_then(|manifest| manifest.get(section))
            .and_then(toml::Value::as_array)
            .cloned()
            .unwrap_or_default();
        for entry in entries {
            let Some(name) = entry.get("name").and_then(toml::Value::as_str) else {
                continue;
            };
            let default_path = if is_bin {
                format!("src/bin/{name}.rs")
            } else {
                format!("tests/{name}.rs")
            };
            let path = entry
                .get("path")
                .and_then(toml::Value::as_str)
                .map_or(default_path, str::to_owned);
            let gated = entry
                .get("required-features")
                .and_then(toml::Value::as_array)
                .is_some_and(|features| !features.is_empty());
            let owner = if is_bin {
                Owner::Bin(package, name.to_owned())
            } else {
                Owner::Test(package, name.to_owned())
            };
            targets.push((owner, normalize(&format!("{prefix}{path}")), gated));
        }
    }
    let auto = |root: &str| !targets.iter().any(|(_, path, _)| path == root);
    let mut discovered = Vec::new();
    for path in tree.keys() {
        let Some(rest) = path.strip_prefix(&prefix) else {
            continue;
        };
        let parts: Vec<&str> = rest.split('/').collect();
        let found = match parts.as_slice() {
            ["src", "main.rs"] => Some(Owner::Bin(package, package.to_owned())),
            ["src", "bin", file] if file.ends_with(".rs") => {
                Some(Owner::Bin(package, file.trim_end_matches(".rs").to_owned()))
            }
            ["src", "bin", dir, "main.rs"] => Some(Owner::Bin(package, (*dir).to_owned())),
            ["tests", file] if file.ends_with(".rs") => Some(Owner::Test(
                package,
                file.trim_end_matches(".rs").to_owned(),
            )),
            ["tests", dir, "main.rs"] => Some(Owner::Test(package, (*dir).to_owned())),
            _ => None,
        };
        if let Some(owner) = found
            && auto(path)
            && !targets.iter().any(|(known, _, _)| *known == owner)
        {
            discovered.push((owner, path.clone(), false));
        }
    }
    targets.extend(discovered);
    targets
}

impl Program {
    fn build(tree: &Tree) -> Self {
        let mut program = Program {
            lib_files: BTreeMap::new(),
            mount_error: None,
            items: Vec::new(),
            by_file: HashMap::new(),
            by_mention: HashMap::new(),
            globs: HashMap::new(),
            aliases: HashMap::new(),
            local_traits: HashSet::new(),
            owners: BTreeMap::new(),
            runnable_targets: BTreeMap::new(),
            target_texts: BTreeMap::new(),
            source_readers: Vec::new(),
        };
        for package in LIB_PACKAGES {
            let crate_name: &'static str = package;
            let (files, error) = mount(tree, &format!("crates/{package}/src/lib.rs"));
            program.mount_error = program.mount_error.or(error);
            for (path, mounted) in files {
                program
                    .owners
                    .entry(path.clone())
                    .or_default()
                    .insert(Owner::Lib(crate_name));
                if program.lib_files.contains_key(&path) {
                    program
                        .mount_error
                        .get_or_insert_with(|| format!("multiply mounted library source {path}"));
                }
                program.lib_files.insert(
                    path,
                    LibFile {
                        crate_name,
                        module: mounted.module,
                        parsed: mounted.parsed,
                        test: mounted.test,
                    },
                );
            }
            for (owner, root, gated) in package_targets(tree, package) {
                let mut declares_tests = false;
                let (files, _) = mount(tree, &root);
                for (path, mounted) in files {
                    declares_tests |= mounted.parsed.items.iter().any(|item| item.test);
                    if let Some(text) = tree.get(&path) {
                        program.target_texts.insert(path.clone(), text.clone());
                    }
                    program
                        .owners
                        .entry(path)
                        .or_default()
                        .insert(owner.clone());
                }
                if declares_tests && !gated {
                    let filter = match &owner {
                        Owner::Bin(_, name) => format!("bin:{name}"),
                        Owner::Test(_, name) => format!("test:{name}"),
                        Owner::Lib(_) => continue,
                    };
                    program.runnable_targets.insert(owner, filter);
                }
            }
        }
        for (path, file) in &program.lib_files {
            for item in &file.parsed.items {
                let module = join_module(&file.module, &item.inline);
                program.items.push(FlatItem {
                    path: path.clone(),
                    crate_name: file.crate_name,
                    module,
                    in_test: file.test || file.parsed.in_test(item.start),
                    item: item.clone(),
                });
            }
        }
        program.resolve_macro_invocations();
        for (index, flat) in program.items.iter_mut().enumerate() {
            // Every member of `impl T` names `T`.
            if let Some(self_ty) = flat
                .item
                .impl_ctx
                .as_ref()
                .and_then(|ctx| ctx.self_ty.clone())
            {
                flat.item.mentions.insert(self_ty);
            }
            program
                .by_file
                .entry(flat.path.clone())
                .or_default()
                .push(index);
        }
        // Test code that resolves the crate root (`env!("CARGO_MANIFEST_DIR")`,
        // or a test helper that does) and names a source tree reads sources.
        let root_helpers: HashSet<&str> = program
            .items
            .iter()
            .filter(|flat| {
                flat.in_test
                    && flat
                        .item
                        .literals
                        .iter()
                        .any(|literal| literal == "CARGO_MANIFEST_DIR")
            })
            .flat_map(|flat| flat.item.names.iter().map(String::as_str))
            .collect();
        program.source_readers = program
            .items
            .iter()
            .enumerate()
            .filter(|(_, flat)| {
                let literals = &flat.item.literals;
                flat.in_test
                    && literals.iter().any(|literal| source_tree_path(literal))
                    && (literals
                        .iter()
                        .any(|literal| literal == "CARGO_MANIFEST_DIR")
                        || flat
                            .item
                            .mentions
                            .iter()
                            .any(|name| root_helpers.contains(name.as_str())))
            })
            .map(|(index, _)| index)
            .collect();
        for (index, flat) in program.items.iter().enumerate() {
            for mention in &flat.item.mentions {
                program
                    .by_mention
                    .entry(mention.clone())
                    .or_default()
                    .push(index);
            }
            match flat.item.kind {
                ItemKind::Use if flat.item.glob => {
                    program
                        .globs
                        .entry((flat.crate_name, flat.module.clone()))
                        .or_default()
                        .extend(flat.item.mentions.iter().cloned());
                }
                ItemKind::Trait => {
                    program.local_traits.extend(flat.item.names.iter().cloned());
                }
                _ => {}
            }
            if matches!(flat.item.kind, ItemKind::Use | ItemKind::Type) {
                for mention in &flat.item.mentions {
                    program
                        .aliases
                        .entry(mention.clone())
                        .or_default()
                        .extend(flat.item.names.iter().cloned());
                }
            }
        }
        program
    }

    /// An item-level invocation of a `macro_rules!` macro defined in the
    /// same crate whose expansion names are readable (see
    /// [`Parser::expansion_names`]) is an ordinary public item defining
    /// those names, so a change to it reaches only what names them. Any
    /// other invocation stays opaque.
    fn resolve_macro_invocations(&mut self) {
        let mut defined: HashMap<(&'static str, String), Option<Vec<String>>> = HashMap::new();
        for flat in &self.items {
            if flat.item.kind != ItemKind::Macro {
                continue;
            }
            for name in &flat.item.names {
                let slot = defined
                    .entry((flat.crate_name, name.clone()))
                    .or_insert_with(|| Some(Vec::new()));
                match (slot.as_mut(), &flat.item.expands_to) {
                    (Some(names), Some(more)) => names.extend(more.iter().cloned()),
                    _ => *slot = None,
                }
            }
        }
        for flat in &mut self.items {
            let item = &mut flat.item;
            if item.kind != ItemKind::Opaque
                || item.impl_ctx.is_some()
                || item.in_trait
                || item.test
            {
                continue;
            }
            let Some(Some(names)) = item
                .invokes
                .as_ref()
                .and_then(|name| defined.get(&(flat.crate_name, name.clone())))
            else {
                continue;
            };
            item.names = names.clone();
            item.kind = ItemKind::Value;
            item.public = true;
        }
    }

    /// The items of `path` whose span holds `line`.
    fn items_at(&self, path: &str, line: u32) -> Vec<usize> {
        self.by_file
            .get(path)
            .into_iter()
            .flatten()
            .copied()
            .filter(|index| {
                let item = &self.items[*index].item;
                item.start <= line && line <= item.end
            })
            .collect()
    }

    fn in_subtree(module: &str, root: &str) -> bool {
        root.is_empty()
            || module == root
            || module
                .strip_prefix(root)
                .is_some_and(|rest| rest.starts_with("::"))
    }

    fn subtree_items(&self, crate_name: &str, module: &str) -> Vec<usize> {
        self.items
            .iter()
            .enumerate()
            .filter(|(_, flat)| {
                flat.crate_name == crate_name && Self::in_subtree(&flat.module, module)
            })
            .map(|(index, _)| index)
            .collect()
    }

    /// The library items and target files that read `path`. A Rust source
    /// is read by an `include_str!`/`include_bytes!` that resolves to it (a
    /// path literal elsewhere is test data); a data file by any literal that
    /// names it (its file name or a path ending in it), since it is read at
    /// run time.
    fn literal_readers(&self, path: &str) -> Vec<Reader> {
        let file_name = path.rsplit('/').next().unwrap_or(path);
        let rust = path.ends_with(".rs");
        let names_it = |from: &str, literal: &str| {
            if rust {
                literal.strip_prefix(INCLUDED).is_some_and(|included| {
                    normalize(&format!("{}/{included}", dirname(from))) == path
                })
            } else {
                literal == file_name || literal.ends_with(&format!("/{file_name}"))
            }
        };
        let mut readers: Vec<Reader> = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, flat)| {
                flat.item
                    .literals
                    .iter()
                    .any(|literal| names_it(&flat.path, literal))
            })
            .map(|(index, _)| Reader::Item(index))
            .collect();
        for (target_path, text) in &self.target_texts {
            let tokens = tokenize(text);
            let mentions = tokens.iter().enumerate().any(|(at, token)| {
                if token.kind != Kind::Str {
                    return false;
                }
                let value = literal_value(token.text);
                let included = at >= 3
                    && tokens[at - 1].is("(")
                    && tokens[at - 2].is("!")
                    && tokens[at - 3]
                        .ident()
                        .is_some_and(|name| matches!(name, "include_str" | "include_bytes"));
                if included {
                    names_it(target_path, &format!("{INCLUDED}{value}"))
                } else {
                    names_it(target_path, value)
                }
            });
            if mentions {
                for owner in self.owners.get(target_path).into_iter().flatten() {
                    if !matches!(owner, Owner::Lib(_)) {
                        readers.push(Reader::Target(owner.clone()));
                    }
                }
            }
        }
        readers
    }

    /// The keys an affected item propagates, or why the names cannot.
    fn keys(&self, index: usize) -> Result<Vec<Key>, String> {
        let flat = &self.items[index];
        let item = &flat.item;
        let place = || format!("{} line {}", flat.path, item.start);
        if item.kind == ItemKind::Opaque {
            return Err(format!(
                "an item-level macro or unreadable impl at {} is affected",
                place()
            ));
        }
        let trait_member = item
            .impl_ctx
            .as_ref()
            .is_some_and(|ctx| ctx.trait_name.is_some());
        let global = item.public
            || item.in_trait
            || trait_member
            || matches!(item.kind, ItemKind::Macro | ItemKind::ImplHeader);
        let scope = (!global).then(|| (flat.crate_name, flat.module.clone()));
        let mut keys = Vec::new();
        let inherent_type = item
            .impl_ctx
            .as_ref()
            .filter(|ctx| ctx.trait_name.is_none())
            .and_then(|ctx| ctx.self_ty.clone());
        let qualified = inherent_type.is_some()
            && match item.kind {
                ItemKind::Fn => !item.receiver,
                ItemKind::Value | ItemKind::Type => true,
                _ => false,
            };
        for name in &item.names {
            // A member of an impl of a foreign trait is only called through
            // the trait on a value of the self type (the self-type key).
            if let Some(ctx) = item
                .impl_ctx
                .as_ref()
                .filter(|ctx| ctx.trait_name.is_some())
                && item.kind != ItemKind::ImplHeader
                && !ctx
                    .trait_name
                    .as_ref()
                    .is_some_and(|name| self.local_traits.contains(name))
            {
                continue;
            }
            keys.push(Key {
                name: name.clone(),
                qualifier: if qualified {
                    inherent_type.clone()
                } else {
                    None
                },
                scope: scope.clone(),
            });
        }
        for variant in &item.variants {
            keys.push(Key {
                name: variant.clone(),
                qualifier: item.names.first().cloned(),
                scope: scope.clone(),
            });
        }
        if let Some(ctx) = &item.impl_ctx
            && ctx.trait_name.is_some()
        {
            match &ctx.self_ty {
                Some(self_ty) => keys.push(Key {
                    name: self_ty.clone(),
                    qualifier: None,
                    scope: None,
                }),
                None => {
                    return Err(format!(
                        "a trait impl with an unreadable self type at {} is affected",
                        place()
                    ));
                }
            }
        }
        if item.kind == ItemKind::Use && item.glob {
            // A changed glob import can change any name in its module.
            keys.push(Key {
                name: String::new(),
                qualifier: None,
                scope: Some((flat.crate_name, flat.module.clone())),
            });
        }
        Ok(keys)
    }

    fn matches(&self, key: &Key, index: usize) -> bool {
        let flat = &self.items[index];
        if let Some((crate_name, module)) = &key.scope
            && (flat.crate_name != *crate_name || !Self::in_subtree(&flat.module, module))
        {
            return false;
        }
        let Some(qualifier) = &key.qualifier else {
            return true;
        };
        // Alias edges are deliberately scope-independent: extra matches
        // are safe, but dropping a chained qualifier can hide a regression.
        // A visited set closes chains even when reused names form cycles.
        let mut accepted = HashSet::new();
        let mut pending = vec![qualifier.as_str(), "Self"];
        while let Some(name) = pending.pop() {
            if !accepted.insert(name) {
                continue;
            }
            if let Some(aliases) = self.aliases.get(name) {
                pending.extend(aliases.iter().map(String::as_str));
            }
        }
        if accepted
            .iter()
            .any(|name| flat.item.mentions.contains(*name))
        {
            return true;
        }
        // A glob import of the module or an ancestor can bring the
        // qualifier's members into scope unqualified.
        let mut module = flat.module.as_str();
        loop {
            if let Some(glob) = self.globs.get(&(flat.crate_name, module.to_owned()))
                && accepted.iter().any(|name| glob.contains(*name))
            {
                return true;
            }
            match module.rsplit_once("::") {
                Some((parent, _)) => module = parent,
                None if !module.is_empty() => module = "",
                None => return false,
            }
        }
    }

    /// The tests the seeds reach (full libtest names), or why the names
    /// cannot bound them.
    fn closure(&self, seeds: &[usize], seed_keys: Vec<Key>) -> Result<BTreeSet<String>, String> {
        let mut affected = vec![false; self.items.len()];
        let mut queue: Vec<Key> = seed_keys;
        let mut seen_keys: HashSet<Key> = HashSet::new();
        let mut tests = BTreeSet::new();
        let mut mark = |index: usize,
                        queue: &mut Vec<Key>,
                        tests: &mut BTreeSet<String>|
         -> Result<(), String> {
            if affected[index] {
                return Ok(());
            }
            affected[index] = true;
            let flat = &self.items[index];
            if flat.item.test && !flat.item.ignored {
                if let Some(name) = flat.item.names.first() {
                    tests.insert(join_module(&flat.module, std::slice::from_ref(name)));
                }
            }
            queue.extend(self.keys(index)?);
            Ok(())
        };
        for seed in seeds {
            mark(*seed, &mut queue, &mut tests)?;
        }
        while let Some(key) = queue.pop() {
            if !seen_keys.insert(key.clone()) {
                continue;
            }
            let candidates: Vec<usize> = if key.name.is_empty() {
                // A glob key: every item of its module subtree.
                let (crate_name, module) = key.scope.clone().unwrap_or(("", String::new()));
                self.subtree_items(crate_name, &module)
            } else {
                self.by_mention.get(&key.name).cloned().unwrap_or_default()
            };
            for index in candidates {
                if self.matches(&key, index) {
                    mark(index, &mut queue, &mut tests)?;
                }
            }
        }
        Ok(tests)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(files: &[(&str, &str)]) -> Tree {
        files
            .iter()
            .map(|(path, text)| ((*path).to_owned(), (*text).to_owned()))
            .collect()
    }

    fn changed(path: &str, lines: &[u32]) -> FileDiff {
        FileDiff {
            old_path: Some(path.to_owned()),
            new_path: Some(path.to_owned()),
            old_lines: Vec::new(),
            new_lines: lines.to_vec(),
            binary: false,
        }
    }

    fn line_of(text: &str, needle: &str) -> u32 {
        u32::try_from(
            text.lines()
                .position(|line| line.contains(needle))
                .expect(needle)
                + 1,
        )
        .unwrap()
    }

    fn selected(selection: &UnfilteredSelection) -> BTreeSet<String> {
        match &selection.lib {
            LibTests::Tests(tests) => tests.clone(),
            LibTests::Everything(reason) => {
                panic!("expected a narrow selection, got everything: {reason}")
            }
        }
    }

    const STORE_LIB: &str = "pub mod store;\npub mod store_support;\n";
    const STORE: &str = "pub struct Store;\n";
    const USAGE: &str = "\
use crate::store::Store;

pub const LAUNCH_REFUSED: &str = \"launch_refused\";

impl Store {
    pub fn topology_created_usage(&self) -> u32 {
        2
    }

    pub fn unrelated(&self) -> u32 {
        0
    }
}
";
    const RSID_LIB: &str = "pub mod topology;\npub mod other;\n";
    const TOPOLOGY: &str = "\
use rsid_store::store::Store;

pub fn charge(store: &Store) -> bool {
    store.topology_created_usage() < 3
}

#[cfg(test)]
mod agent_tests;
";
    const AGENT_TESTS: &str = "\
use super::*;

#[test]
fn t4_a5_max_created_sessions_is_charged() {
    assert!(charge(&Store));
}

#[test]
fn unrelated_topology_test() {
    assert_eq!(1, 1);
}
";
    const OTHER: &str = "\
pub fn helper() -> u32 { 1 }

#[cfg(test)]
mod tests {
    #[test]
    fn other_test() {
        assert_eq!(super::helper(), 1);
    }
}
";

    fn workspace() -> Tree {
        tree(&[
            ("crates/rsid-store/src/lib.rs", STORE_LIB),
            ("crates/rsid-store/src/store.rs", STORE),
            (
                "crates/rsid-store/src/store_support/mod.rs",
                "pub mod topology_usage;\n",
            ),
            (
                "crates/rsid-store/src/store_support/topology_usage.rs",
                USAGE,
            ),
            ("crates/rsid/src/lib.rs", RSID_LIB),
            ("crates/rsid/src/topology/mod.rs", TOPOLOGY),
            ("crates/rsid/src/topology/agent_tests.rs", AGENT_TESTS),
            ("crates/rsid/src/other.rs", OTHER),
        ])
    }

    /// #1280: a module with no local tests (`store_support/topology_usage.rs`)
    /// selects the test in another module and package that exercises it
    /// through an inherent method call with no `use` of the module.
    #[test]
    fn a_change_without_local_tests_selects_the_dependent_test() {
        let base = workspace();
        let mut candidate = workspace();
        candidate.insert(
            "crates/rsid-store/src/store_support/topology_usage.rs".into(),
            USAGE.replace("        2\n", "        0\n"),
        );
        let path = "crates/rsid-store/src/store_support/topology_usage.rs";
        let line = line_of(USAGE, "        2");
        let selection = select_from(&[changed(path, &[line])], &base, &candidate).unwrap();
        assert_eq!(
            selected(&selection),
            BTreeSet::from([
                "topology::agent_tests::t4_a5_max_created_sessions_is_charged".to_owned()
            ])
        );
    }

    const FIELDS_MACRO: &str = "\
macro_rules! field_classes {
    (names: [$($name:ident),* $(,)?],) => {
        pub const FIELDS: &[&str] = &[$(stringify!($name)),*];

        pub fn exhaustive(value: &Value) {
            let Value { $($name: _,)* } = value;
        }
    };
}

field_classes! {
    names: [id, title],
}

pub struct Value {
    pub id: u32,
    pub title: u32,
}

#[cfg(test)]
mod tests {
    #[test]
    fn lists_both_fields() {
        assert_eq!(super::FIELDS.len(), 2);
    }
}
";

    fn macro_workspace(fields: &str) -> Tree {
        let mut files = workspace();
        files.insert(
            "crates/rsid/src/lib.rs".into(),
            "pub mod topology;\npub mod other;\npub mod fields;\n".into(),
        );
        files.insert("crates/rsid/src/fields.rs".into(), fields.into());
        files
    }

    /// #1529: an item-level invocation of a local `macro_rules!` macro whose
    /// expansion names are readable selects only the tests that name them.
    #[test]
    fn a_local_macro_invocation_selects_only_the_tests_naming_what_it_defines() {
        let base = macro_workspace(FIELDS_MACRO);
        let candidate_text = FIELDS_MACRO.replace("[id, title]", "[id, title, extra]");
        let candidate = macro_workspace(&candidate_text);
        let line = line_of(FIELDS_MACRO, "names: [id, title]");
        let selection = select_from(
            &[changed("crates/rsid/src/fields.rs", &[line])],
            &base,
            &candidate,
        )
        .unwrap();
        assert_eq!(
            selected(&selection),
            BTreeSet::from(["fields::tests::lists_both_fields".to_owned()])
        );
    }

    /// #1529: a changed macro definition reaches its invocation and so the
    /// tests naming what it defines, not every test.
    #[test]
    fn a_local_macro_definition_change_selects_the_tests_naming_its_expansion() {
        let base = macro_workspace(FIELDS_MACRO);
        let candidate_text = FIELDS_MACRO.replace(
            "pub fn exhaustive(value: &Value) {",
            "pub fn exhaustive(value: &Value) {\n            let _ = 1;",
        );
        let candidate = macro_workspace(&candidate_text);
        let line = line_of(&candidate_text, "let _ = 1;");
        let selection = select_from(
            &[changed("crates/rsid/src/fields.rs", &[line])],
            &base,
            &candidate,
        )
        .unwrap();
        assert_eq!(
            selected(&selection),
            BTreeSet::from(["fields::tests::lists_both_fields".to_owned()])
        );
    }

    /// #1529: an invocation whose expansion cannot be read keeps selecting
    /// everything, and the reason names the file and line to split on.
    #[test]
    fn an_unreadable_macro_invocation_runs_everything_and_names_its_file() {
        let variants = [
            (
                "macro_rules! make {\n    ($name:ident) => {\n        pub fn $name() {}\n    };\n}\nmake! { alpha }\n",
                "make! { alpha }",
            ),
            (
                "macro_rules! make {\n    ($($item:item)*) => { $($item)* };\n}\nmake! { fn alpha() {} }\n",
                "make! { fn alpha",
            ),
            (
                "macro_rules! each {\n    () => {\n        #[test]\n        fn generated() {}\n    };\n}\neach! {}\n",
                "each! {}",
            ),
            ("external_macro! { alpha }\n", "external_macro!"),
        ];
        for (text, needle) in variants {
            let base = macro_workspace(text);
            let candidate = macro_workspace(text);
            let line = line_of(text, needle);
            let selection = select_from(
                &[changed("crates/rsid/src/fields.rs", &[line])],
                &base,
                &candidate,
            )
            .unwrap();
            match selection.lib {
                LibTests::Everything(reason) => {
                    assert!(
                        reason.contains("crates/rsid/src/fields.rs line"),
                        "{reason}"
                    );
                }
                LibTests::Tests(tests) => panic!("{needle}: narrowed to {tests:?}"),
            }
        }
    }

    /// #1280: a widely depended-on item reaches every test that names it,
    /// transitively and across both packages.
    #[test]
    fn a_widely_used_item_selects_every_test_that_reaches_it() {
        let base = workspace();
        let mut candidate = workspace();
        let store = "pub struct Store;\n\nimpl Store {\n    pub fn id(&self) -> u32 { 7 }\n}\n";
        candidate.insert("crates/rsid-store/src/store.rs".into(), store.into());
        candidate.insert(
            "crates/rsid/src/other.rs".into(),
            OTHER.replace(
                "assert_eq!(super::helper(), 1);",
                "assert_eq!(super::helper(), 1);\n        let _ = rsid_store::store::Store;",
            ),
        );
        let selection = select_from(
            &[changed("crates/rsid-store/src/store.rs", &[1])],
            &base,
            &candidate,
        )
        .unwrap();
        assert_eq!(
            selected(&selection),
            BTreeSet::from([
                "other::tests::other_test".to_owned(),
                "topology::agent_tests::t4_a5_max_created_sessions_is_charged".to_owned(),
            ])
        );
    }

    /// Found nothing never means run nothing: library code no test names
    /// runs every library test.
    #[test]
    fn library_code_no_test_reaches_runs_everything() {
        let base = workspace();
        let path = "crates/rsid-store/src/store_support/topology_usage.rs";
        let line = line_of(USAGE, "        0");
        let selection = select_from(&[changed(path, &[line])], &base, &base).unwrap();
        assert!(matches!(selection.lib, LibTests::Everything(_)));
    }

    #[test]
    fn a_private_helper_reaches_only_its_own_module() {
        let mut base = workspace();
        let other_two = "\
fn helper() -> u32 { 2 }

#[cfg(test)]
mod tests {
    #[test]
    fn two_test() {
        assert_eq!(super::helper(), 2);
    }
}
";
        base.insert(
            "crates/rsid/src/lib.rs".into(),
            format!("{RSID_LIB}pub mod two;\n"),
        );
        base.insert("crates/rsid/src/two.rs".into(), other_two.into());
        base.insert(
            "crates/rsid/src/other.rs".into(),
            OTHER.replace("pub fn helper", "fn helper"),
        );
        let selection =
            select_from(&[changed("crates/rsid/src/two.rs", &[1])], &base, &base).unwrap();
        assert_eq!(
            selected(&selection),
            BTreeSet::from(["two::tests::two_test".to_owned()])
        );
    }

    /// #1316: the direct test passes after this body-only change, while the
    /// consumer reached through two import aliases fails.
    #[test]
    fn an_associated_function_change_reaches_chained_import_aliases() {
        let thing = "\
mod consumers;
pub struct Thing;
impl Thing {
    pub fn new() -> u8 {
        0
    }
}
#[test]
fn direct() { let _ = Thing::new(); }
";
        let base = tree(&[
            ("crates/rsid/src/lib.rs", "mod thing;\n"),
            ("crates/rsid/src/thing.rs", thing),
            (
                "crates/rsid/src/thing/consumers.rs",
                "use crate::thing::Thing as First;\nuse First as Second;\n#[test]\nfn renamed() { assert_eq!(Second::new(), 0); }\n",
            ),
        ]);
        let path = "crates/rsid/src/thing.rs";
        let mut candidate = base.clone();
        candidate.insert(path.into(), thing.replace("        0\n", "        1\n"));
        let line = line_of(thing, "        0");
        let mut diff = changed(path, &[line]);
        diff.old_lines = vec![line];
        let selection = select_from(&[diff], &base, &candidate).unwrap();
        assert_eq!(
            selected(&selection),
            BTreeSet::from([
                "thing::direct".to_owned(),
                "thing::consumers::renamed".to_owned(),
            ])
        );
    }

    /// Valid imports in separate modules can form a cycle in the
    /// conservative, scope-independent alias graph.
    #[test]
    fn qualifier_alias_closure_handles_type_aliases_and_cycles() {
        let thing =
            "pub struct Thing;\nimpl Thing {\n    pub fn new() -> u8 {\n        0\n    }\n}\n";
        let base = tree(&[
            ("crates/rsid/src/lib.rs", "mod thing;\nmod a;\nmod b;\n"),
            ("crates/rsid/src/thing.rs", thing),
            (
                "crates/rsid/src/a.rs",
                "use crate::thing::Thing as First;\nuse First as Second;\ntype Third = Second;\n#[test]\nfn renamed() { assert_eq!(Third::new(), 0); }\n",
            ),
            (
                "crates/rsid/src/b.rs",
                "use crate::thing::Thing as Second;\nuse Second as First;\n#[test]\nfn reverse() { assert_eq!(First::new(), 0); }\n",
            ),
        ]);
        let path = "crates/rsid/src/thing.rs";
        let mut candidate = base.clone();
        candidate.insert(path.into(), thing.replace("        0\n", "        1\n"));
        let selection = select_from(
            &[changed(path, &[line_of(thing, "        0")])],
            &base,
            &candidate,
        )
        .unwrap();
        assert_eq!(
            selected(&selection),
            BTreeSet::from(["a::renamed".to_owned(), "b::reverse".to_owned()])
        );
    }

    const SHARED: &str = "\
fn helper() -> u8 {
    0
}
#[test]
fn checks() {
    if module_path!().ends_with(\"second\") {
        assert_eq!(helper(), 0);
    } else {
        let _ = helper();
    }
}
";

    fn duplicate_mount_selection(root: &str) -> UnfilteredSelection {
        let base = tree(&[
            ("crates/rsid/src/lib.rs", root),
            ("crates/rsid/src/shared.rs", SHARED),
        ]);
        let path = "crates/rsid/src/shared.rs";
        let mut candidate = base.clone();
        candidate.insert(path.into(), SHARED.replace("    0\n", "    1\n"));
        let line = line_of(SHARED, "    0");
        let mut diff = changed(path, &[line]);
        diff.old_lines = vec![line];
        select_from(&[diff], &base, &candidate).unwrap()
    }

    /// #1317: first::checks passes but second::checks fails. Retaining just
    /// the first module's private helper key must not produce a narrow set.
    #[test]
    fn duplicate_path_mounts_run_everything() {
        let selection = duplicate_mount_selection(
            "#[path = \"shared.rs\"]\nmod first;\n#[path = \"shared.rs\"]\nmod second;\n",
        );
        assert!(
            matches!(selection.lib, LibTests::Everything(reason) if reason.contains("multiply mounted") && reason.contains("shared.rs"))
        );
    }

    /// The retained mount can be inactive on the gate platform. Selection
    /// must not depend on cfg evaluation or declaration order.
    #[test]
    fn platform_conditional_duplicate_path_mounts_run_everything() {
        for root in [
            "#[cfg(target_os = \"windows\")]\n#[path = \"shared.rs\"]\nmod first;\n#[cfg(not(target_os = \"windows\"))]\n#[path = \"shared.rs\"]\nmod second;\n",
            "#[cfg(not(target_os = \"windows\"))]\n#[path = \"shared.rs\"]\nmod second;\n#[cfg(target_os = \"windows\")]\n#[path = \"shared.rs\"]\nmod first;\n",
        ] {
            let selection = duplicate_mount_selection(root);
            assert!(
                matches!(selection.lib, LibTests::Everything(reason) if reason.contains("multiply mounted"))
            );
        }
    }

    #[test]
    fn duplicate_include_mounts_run_everything() {
        let selection = duplicate_mount_selection(
            "mod first { include!(\"shared.rs\"); }\nmod second { include!(\"shared.rs\"); }\n",
        );
        assert!(
            matches!(selection.lib, LibTests::Everything(reason) if reason.contains("multiply mounted"))
        );
    }

    /// Descendants inherit both module contexts, even though their own
    /// mounts are visited only once. Either diff side can be ambiguous.
    #[test]
    fn duplicate_mount_descendants_run_everything_on_either_diff_side() {
        let repeated = tree(&[
            (
                "crates/rsid/src/lib.rs",
                "#[path = \"shared.rs\"]\nmod first;\n#[path = \"shared.rs\"]\nmod second;\n",
            ),
            ("crates/rsid/src/shared.rs", "mod child;\n"),
            ("crates/rsid/src/shared/child.rs", SHARED),
        ]);
        let mut single = repeated.clone();
        single.insert(
            "crates/rsid/src/lib.rs".into(),
            "#[path = \"shared.rs\"]\nmod first;\n".into(),
        );
        let path = "crates/rsid/src/shared/child.rs";
        let line = line_of(SHARED, "    0");
        for (base, candidate, old_lines, new_lines) in [
            (&repeated, &single, vec![line], vec![]),
            (&single, &repeated, vec![], vec![line]),
        ] {
            let diff = FileDiff {
                old_path: Some(path.into()),
                new_path: Some(path.into()),
                old_lines,
                new_lines,
                binary: false,
            };
            let selection = select_from(&[diff], base, candidate).unwrap();
            assert!(
                matches!(selection.lib, LibTests::Everything(reason) if reason.contains("multiply mounted"))
            );
        }
    }

    #[test]
    fn a_test_only_change_selects_that_test_and_comment_lines_select_nothing() {
        let base = workspace();
        let path = "crates/rsid/src/topology/agent_tests.rs";
        let line = line_of(AGENT_TESTS, "assert_eq!(1, 1)");
        let selection = select_from(&[changed(path, &[line])], &base, &base).unwrap();
        assert_eq!(
            selected(&selection),
            BTreeSet::from(["topology::agent_tests::unrelated_topology_test".to_owned()])
        );
        let mut commented = workspace();
        commented.insert(path.into(), format!("// note\n{AGENT_TESTS}"));
        let selection = select_from(&[changed(path, &[1])], &base, &commented).unwrap();
        assert_eq!(selection.lib, LibTests::Tests(BTreeSet::new()));
    }

    #[test]
    fn a_deleted_function_selects_the_tests_that_named_it_from_the_base_side() {
        let base = workspace();
        let mut candidate = workspace();
        // The candidate drops `charge`; the test that called it now fails to
        // compile or calls something else, and must run.
        let without = TOPOLOGY.replace(
            "pub fn charge(store: &Store) -> bool {\n    store.topology_created_usage() < 3\n}\n",
            "",
        );
        candidate.insert("crates/rsid/src/topology/mod.rs".into(), without);
        let diff = FileDiff {
            old_path: Some("crates/rsid/src/topology/mod.rs".into()),
            new_path: Some("crates/rsid/src/topology/mod.rs".into()),
            old_lines: vec![3, 4, 5],
            new_lines: Vec::new(),
            binary: false,
        };
        let selection = select_from(&[diff], &base, &candidate).unwrap();
        assert_eq!(
            selected(&selection),
            BTreeSet::from([
                "topology::agent_tests::t4_a5_max_created_sessions_is_charged".to_owned()
            ])
        );
    }

    #[test]
    fn crate_wide_unmounted_and_target_changes() {
        let mut base = workspace();
        base.insert(
            "crates/rsid/Cargo.toml".into(),
            "[package]\nname = \"rsid\"\n\n[[bin]]\nname = \"gated\"\npath = \"src/bin/gated.rs\"\nrequired-features = [\"x\"]\n".into(),
        );
        base.insert(
            "crates/rsid/src/bin/tool.rs".into(),
            "fn main() {}\n#[test]\nfn t() {}\n".into(),
        );
        base.insert(
            "crates/rsid/src/bin/gated.rs".into(),
            "fn main() {}\n#[test]\nfn t() {}\n".into(),
        );
        base.insert(
            "crates/rsid/tests/flow.rs".into(),
            "#[test]\nfn f() {}\n".into(),
        );
        base.insert(
            "crates/rsid-store/src/store/migrations/v9.rs".into(),
            "if version < 9 {}\n".into(),
        );
        for manifest in [
            "crates/rsid-store/Cargo.toml",
            "crates/rsid/build.rs",
            "crates/rsid/src/lib.rs",
        ] {
            assert_eq!(select_from(&[changed(manifest, &[1])], &base, &base), None);
        }
        let migration = select_from(
            &[changed(
                "crates/rsid-store/src/store/migrations/v9.rs",
                &[1],
            )],
            &base,
            &base,
        )
        .unwrap();
        assert!(
            matches!(migration.lib, LibTests::Everything(reason) if reason.contains("unmounted"))
        );
        let targets = select_from(
            &[
                changed("crates/rsid/src/bin/tool.rs", &[1]),
                changed("crates/rsid/src/bin/gated.rs", &[1]),
                changed("crates/rsid/tests/flow.rs", &[1]),
                changed("crates/rsi/src/app.rs", &[1]),
            ],
            &base,
            &base,
        )
        .unwrap();
        assert_eq!(targets.lib, LibTests::Tests(BTreeSet::new()));
        assert_eq!(
            targets.targets,
            BTreeSet::from([
                ("rsid".to_owned(), "bin:tool".to_owned()),
                ("rsid".to_owned(), "test:flow".to_owned()),
            ])
        );
    }

    #[test]
    fn a_data_file_runs_the_tests_that_read_it() {
        let mut base = workspace();
        base.insert(
            "crates/rsid/src/other.rs".into(),
            OTHER.replace(
                "pub fn helper() -> u32 { 1 }",
                "pub fn helper() -> u32 { include_str!(\"prompts/help.md\").len() as u32 }",
            ),
        );
        let selection = select_from(
            &[changed("crates/rsid/src/prompts/help.md", &[1])],
            &base,
            &base,
        )
        .unwrap();
        assert_eq!(
            selected(&selection),
            BTreeSet::from(["other::tests::other_test".to_owned()])
        );
        let unread = select_from(
            &[changed("crates/rsid/src/prompts/unread.md", &[1])],
            &base,
            &base,
        )
        .unwrap();
        assert!(matches!(unread.lib, LibTests::Everything(_)));
    }

    /// A source scanner (test code that resolves the crate root and walks a
    /// source tree) observes every source change, even a comment, and an
    /// `include_str!` of a source observes that source.
    #[test]
    fn source_readers_observe_changes_they_do_not_name() {
        let mut base = workspace();
        base.insert(
            "crates/rsid/src/lib.rs".into(),
            format!("{RSID_LIB}#[cfg(test)]\nmod scan;\npub mod reader;\n"),
        );
        base.insert(
            "crates/rsid/src/scan.rs".into(),
            "fn root() -> std::path::PathBuf {\n    std::path::PathBuf::from(env!(\"CARGO_MANIFEST_DIR\"))\n}\n\n#[test]\nfn no_sql_outside_the_store() {\n    assert!(root().join(\"src\").exists());\n}\n".into(),
        );
        base.insert(
            "crates/rsid/src/reader.rs".into(),
            "#[cfg(test)]\nmod tests {\n    #[test]\n    fn router_lists_every_verb() {\n        assert!(include_str!(\"other.rs\").contains(\"helper\"));\n    }\n}\n".into(),
        );
        let mut commented = base.clone();
        commented.insert(
            "crates/rsid/src/other.rs".into(),
            format!("// note\n{OTHER}"),
        );
        let selection = select_from(
            &[changed("crates/rsid/src/other.rs", &[1])],
            &base,
            &commented,
        )
        .unwrap();
        assert_eq!(
            selected(&selection),
            BTreeSet::from([
                "reader::tests::router_lists_every_verb".to_owned(),
                "scan::no_sql_outside_the_store".to_owned(),
            ])
        );
    }

    #[test]
    fn test_regions_follow_cfg_test_items_modules_and_mounts() {
        let tree = tree(&[
            (
                "crates/rsid/src/lib.rs",
                "pub mod a;\n#[cfg(test)]\nmod fixtures;\n",
            ),
            (
                "crates/rsid/src/a.rs",
                "pub fn live() {}\n\n#[cfg(test)]\nfn helper() {}\n\n#[cfg(all(test, unix))]\nmod tests {\n    fn inner() {}\n}\n\n#[test]\nfn t() {}\n",
            ),
            ("crates/rsid/src/fixtures.rs", "pub fn fixture() {}\n"),
        ]);
        let program = Program::build(&tree);
        let in_test: BTreeMap<String, bool> = program
            .items
            .iter()
            .filter(|flat| flat.item.kind == ItemKind::Fn)
            .map(|flat| (flat.item.names[0].clone(), flat.in_test))
            .collect();
        assert_eq!(
            in_test,
            BTreeMap::from([
                ("fixture".to_owned(), true),
                ("helper".to_owned(), true),
                ("inner".to_owned(), true),
                ("live".to_owned(), false),
                ("t".to_owned(), true),
            ])
        );
    }

    #[test]
    fn diffs_parse_both_sides() {
        let text = "\
diff --git a/crates/rsid/src/a.rs b/crates/rsid/src/a.rs
index 1..2 100644
--- a/crates/rsid/src/a.rs
+++ b/crates/rsid/src/a.rs
@@ -3,2 +3 @@ fn x() {
-    old();
-    gone();
+    new();
@@ -10,0 +12,2 @@
+added
+added
diff --git a/crates/rsid/src/new.rs b/crates/rsid/src/new.rs
new file mode 100644
--- /dev/null
+++ b/crates/rsid/src/new.rs
@@ -0,0 +1 @@
+pub fn n() {}
diff --git a/crates/rsid/src/img.png b/crates/rsid/src/img.png
Binary files a/crates/rsid/src/img.png and b/crates/rsid/src/img.png differ
";
        let diffs = parse_diff(text).unwrap();
        assert_eq!(diffs[0].old_lines, vec![3, 4]);
        assert_eq!(diffs[0].new_lines, vec![3, 12, 13]);
        assert_eq!(diffs[1].old_path, None);
        assert_eq!(diffs[1].new_lines, vec![1]);
        assert!(diffs[2].binary);
        assert_eq!(
            diffs[2].new_path.as_deref(),
            Some("crates/rsid/src/img.png")
        );
    }

    #[test]
    fn tokens_skip_comments_and_keep_literals_and_lifetimes() {
        let source = "fn a<'x>(c: char) -> &'x str {\n    // fn hidden() {}\n    let _ = 'q';\n    let _ = r#\"fn raw() { \"# ;\n    /* fn block() {} */ \"{CAPTURED}\"\n}\n";
        let parsed = parse_file(source);
        assert_eq!(parsed.items.len(), 1);
        let item = &parsed.items[0];
        assert_eq!(item.names, vec!["a".to_owned()]);
        assert_eq!((item.start, item.end), (1, 6));
        assert!(item.mentions.contains("CAPTURED"));
        assert!(!parsed.token_lines.contains(&2));
    }

    /// The real source/test pair of #1280: a change to the body of
    /// `topology_created_usage` reaches `t4_a5_max_created_sessions_is_charged`
    /// (or, at worst, runs every library test).
    #[test]
    fn the_real_topology_usage_change_reaches_its_topology_test() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut real = Tree::new();
        for package in LIB_PACKAGES {
            let base = root.join(format!("crates/{package}"));
            let mut stack = vec![base.clone()];
            while let Some(dir) = stack.pop() {
                for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        if !path.ends_with("target") {
                            stack.push(path);
                        }
                        continue;
                    }
                    let relative = path
                        .strip_prefix(&root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/");
                    if relative.ends_with(".rs") || relative.ends_with("/Cargo.toml") {
                        real.insert(relative, std::fs::read_to_string(&path).unwrap_or_default());
                    }
                }
            }
        }
        let path = "crates/rsid-store/src/store_support/topology_usage.rs";
        let text = real.get(path).expect("topology_usage.rs").clone();
        let start = line_of(&text, "fn topology_created_usage");
        let body = start + 2;
        let selection = select_from(&[changed(path, &[body])], &real, &real).unwrap();
        match selection.lib {
            LibTests::Tests(tests) => assert!(
                tests.contains("topology::agent_tests::t4_a5_max_created_sessions_is_charged"),
                "{tests:?}"
            ),
            LibTests::Everything(reason) => assert!(!reason.is_empty()),
        }
    }
}
