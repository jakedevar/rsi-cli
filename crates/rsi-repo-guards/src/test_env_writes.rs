//! Refuse a process-environment write in Rust test code that can race (#190).
//!
//! libtest runs a test binary's tests on parallel threads of one process, so a
//! `std::env::set_var` or `remove_var` in one test is seen by every concurrent
//! test: #187 was a test that removed the real `CARGO_TARGET_DIR` and broke a
//! neighbour that spawned a shell in it. In test code under `crates/`, a raw write
//! passes only when its name is unique to one test:
//!
//!   * the name is a string literal, or a `const NAME: &str = "..."` of the same
//!     file, and contains `TEST`;
//!   * it is not a shared name (`PATH`, `HOME`, `TMPDIR`, `CARGO_TARGET_DIR`,
//!     `RUST_*`, provider keys, ...);
//!   * no other `.rs` file under `crates/` names it, and every raw write of it in
//!     its file sits in one function.
//!
//! Otherwise pass the value through the code under test (`Command::env` on the
//! child, an env map or a builder), or, when the code under test reads the
//! process environment itself, write through the one shared environment lock:
//! `temp_env::with_var(s)` / `temp_env::async_with_vars` (a process-wide lock that
//! restores every value afterwards; those calls are not raw writes). A raw write
//! that must stay needs an entry in `tools/test-env-writes-allowlist.json` with its
//! reason; an entry that no longer matches a write is refused as stale.
//!
//! This is a lexical guard, not a Rust parser. "Test code" is a file under a
//! `tests/` directory, a `tests.rs`/`*_tests.rs` file or one in a `*tests/`
//! directory, a module file its parent declares under `#[cfg(test)]` (and that
//! module's own directory), an inline `#[cfg(test)]` module, and a `#[test]` body.
//!
//! Usage: `rsi-repo-guard test-env-writes [--root DIR] [--allowlist PATH]`.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::cli::{self, Opt};
use crate::re::{self, Lazy, Re, end, group, start};
use crate::theme_test_guards::mask;
use crate::{Ctx, py};

const ALLOWLIST: &str = "tools/test-env-writes-allowlist.json";

static TRIVIA: Lazy = Lazy::new(|| {
    Re::new(concat!(
        r#"(?s)//[^\n]*|/\*.*?\*/|\bb?r(#*)".*?"\1|b?"(?:[^"\\]|\\.)*""#,
        r#"|'(?:[^'\\\n]|\\(?:u\{[0-9a-fA-F_]+\}|x[0-9a-fA-F]{2}|.))'"#,
    ))
});
static RAW_WRITE: Lazy =
    Lazy::new(|| Re::new(r"(?<![\w.])(?:(?:std\s*::\s*)?env\s*::\s*)?(set_var|remove_var)\s*\("));
static FN: Lazy = Lazy::new(|| Re::new(r"\bfn\s+(\w+)"));
static CFG: Lazy = Lazy::new(|| Re::new(r"#\[cfg\(((?:[^\[\]]|\[[^\]]*\])*)\)\]"));
/// `ATTRS.match(text, pos)`: anchored, applied to `&text[pos..]`.
static ATTRS: Lazy = Lazy::new(|| Re::new(r"^\s*(?:#\[(?:[^\[\]]|\[[^\]]*\])*\]\s*)*"));
static TEST_ATTR: Lazy = Lazy::new(|| Re::new(r"#\[(?:tokio::)?test(?:\([^\]]*\))?\]"));
/// `MOD_DECL.match(text, pos)`: anchored, applied to `&text[pos..]`.
static MOD_DECL: Lazy = Lazy::new(|| Re::new(r"^(?:pub(?:\s*\([^)]*\))?\s+)?mod\s+(\w+)\s*([;{])"));
static PATH_ATTR: Lazy = Lazy::new(|| Re::new(r#"#\[path\s*=\s*"([^"]+)"\]"#));
static TEST_WORD: Lazy = Lazy::new(|| Re::new(r"\btest\b"));
static NOT_TEST: Lazy = Lazy::new(|| Re::new(r"\bnot\s*\(\s*test\b"));
static LITERAL: Lazy = Lazy::new(|| Re::new(r#"^"([^"\\]*)"$"#));
static NAME: Lazy = Lazy::new(|| Re::new(r"^[A-Za-z_][A-Za-z0-9_]*$"));

const SHARED_EXACT: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "PWD",
    "OLDPWD",
    "TERM",
    "TMPDIR",
    "TMP",
    "TEMP",
    "SQLITE_TMPDIR",
    "CARGO",
    "CARGO_HOME",
    "CARGO_TARGET_DIR",
    "CARGO_BUILD_JOBS",
    "CARGO_INCREMENTAL",
    "CARGO_MANIFEST_DIR",
    "CARGO_PKG_NAME",
    "RUSTFLAGS",
    "RUSTDOCFLAGS",
    "RUSTC_WRAPPER",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_API_BASE_URL",
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "OPENROUTER_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_PROFILE",
    "AWS_REGION",
    "AWS_DEFAULT_REGION",
    "AWS_BEARER_TOKEN_BEDROCK",
    "RSI_SESSION_TOKEN",
    "RSI_OLLAMA_URL",
    "LOCAL_LLM_BASE_URL",
];
const SHARED_PREFIXES: &[&str] = &[
    "RUST_", "RUSTC_", "NEXTEST_", "GIT_", "XDG_", "LC_", "SSH_", "AWS_",
];

/// Offset of the brace closing the one opened just before `from` (`len` when unbalanced).
fn closing_brace(code: &str, from: usize) -> usize {
    let mut depth: i64 = 1;
    for (pos, byte) in code.bytes().enumerate().skip(from) {
        depth += i64::from(byte == b'{') - i64::from(byte == b'}');
        if depth == 0 {
            return pos;
        }
    }
    code.len()
}

/// The first `{` or `;` at or after `from`.
fn next_brace_or_semicolon(code: &str, from: usize) -> Option<(usize, u8)> {
    code.bytes()
        .enumerate()
        .skip(from)
        .find(|(_, b)| *b == b'{' || *b == b';')
}

fn is_test_cfg(predicate: &str) -> bool {
    TEST_WORD.is_match(predicate) && !NOT_TEST.is_match(predicate)
}

/// (start, end) of every fn body, from its `fn` to its matching `}`.
fn fn_spans(code: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    for caps in FN.find_all(code) {
        if let Some((at, b'{')) = next_brace_or_semicolon(code, end(&caps)) {
            spans.push((start(&caps), closing_brace(code, at + 1)));
        }
    }
    spans
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn stem(path: &Path) -> String {
    path.file_stem()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn is_root_module(path: &Path) -> bool {
    matches!(file_name(path).as_str(), "mod.rs" | "lib.rs" | "main.rs")
}

/// Files a `mod name;` in `path` may resolve to.
fn child_paths(path: &Path, name: &str, path_attr: Option<&str>) -> Vec<PathBuf> {
    let mut folder = path.parent().map(Path::to_path_buf).unwrap_or_default();
    if let Some(attr) = path_attr {
        return vec![folder.join(attr)];
    }
    if !is_root_module(path) {
        folder = folder.join(stem(path));
    }
    vec![
        folder.join(format!("{name}.rs")),
        folder.join(name).join("mod.rs"),
    ]
}

fn module_dir(path: &Path) -> PathBuf {
    let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
    if is_root_module(path) {
        parent
    } else {
        parent.join(stem(path))
    }
}

/// Module files declared under `#[cfg(test)]` (directly, by file), resolved.
fn test_module_files(texts: &[(PathBuf, String)]) -> HashSet<PathBuf> {
    let mut found = HashSet::new();
    for (path, text) in texts {
        if !text.contains("cfg(") || !text.contains("mod ") {
            continue;
        }
        for cfg in CFG.find_all(text) {
            if !is_test_cfg(group(&cfg, 1)) {
                continue;
            }
            let attrs_end = end(&cfg)
                + ATTRS
                    .search(&text[end(&cfg)..])
                    .map_or(0, |caps| end(&caps));
            let Some(decl) = MOD_DECL.search(&text[attrs_end..]) else {
                continue;
            };
            if group(&decl, 2) != ";" {
                continue;
            }
            let window = &text[start(&cfg)..attrs_end];
            let path_attr = PATH_ATTR
                .search(window)
                .map(|caps| group(&caps, 1).to_string());
            for child in child_paths(path, group(&decl, 1), path_attr.as_deref()) {
                found.insert(py::resolve(&child));
            }
        }
    }
    found
}

fn whole_file_test(path: &Path, crates: &Path, declared: &HashSet<PathBuf>) -> bool {
    let parts: Vec<String> = path
        .strip_prefix(crates)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let middle = if parts.len() > 2 {
        &parts[1..parts.len() - 1]
    } else {
        &[][..]
    };
    if middle
        .iter()
        .any(|part| part == "tests" || part.ends_with("tests"))
    {
        return true;
    }
    let stem = stem(path);
    if file_name(path) == "tests.rs" || stem.ends_with("_tests") || stem.ends_with("_test") {
        return true;
    }
    let resolved = py::resolve(path);
    declared.iter().any(|file| {
        let dir = module_dir(file);
        resolved == *file || resolved.ancestors().skip(1).any(|ancestor| ancestor == dir)
    })
}

/// (start, end) of inline `#[cfg(test)]` items and `#[test]` bodies.
fn test_regions(code: &str) -> Vec<(usize, usize)> {
    let mut regions = Vec::new();
    for cfg in CFG.find_all(code) {
        if is_test_cfg(group(&cfg, 1)) {
            let attrs_end = end(&cfg)
                + ATTRS
                    .search(&code[end(&cfg)..])
                    .map_or(0, |caps| end(&caps));
            if let Some((at, b'{')) = next_brace_or_semicolon(code, attrs_end) {
                regions.push((start(&cfg), closing_brace(code, at + 1)));
            }
        }
    }
    for attr in TEST_ATTR.find_all(code) {
        if let Some((at, b'{')) = next_brace_or_semicolon(code, end(&attr)) {
            regions.push((start(&attr), closing_brace(code, at + 1)));
        }
    }
    regions
}

/// The first call argument from `from` (just after `(`), as written in `text`.
fn first_argument<'t>(code: &str, text: &'t str, from: usize) -> &'t str {
    let mut depth = 0_i64;
    for (pos, byte) in code.bytes().enumerate().skip(from) {
        match byte {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                if depth == 0 {
                    return py::strip(&text[from..pos]);
                }
                depth -= 1;
            }
            b',' if depth == 0 => return py::strip(&text[from..pos]),
            _ => {}
        }
    }
    py::strip(&text[from.min(text.len())..])
}

/// The variable name a call argument names, or `None` when not a literal.
fn resolve_name(argument: &str, text: &str) -> Option<String> {
    if let Some(caps) = LITERAL.search(argument) {
        return Some(group(&caps, 1).to_string());
    }
    let ident = argument.rsplit("::").next().unwrap_or(argument);
    if NAME.is_match(ident) {
        let pattern = format!(
            r#"\b(?:const|static)\s+{}\s*:\s*&\s*(?:'static\s+)?str\s*=\s*"([^"]*)"\s*;"#,
            re::escape(ident)
        );
        if let Some(caps) = Re::new(&pattern).search(text) {
            return Some(group(&caps, 1).to_string());
        }
    }
    None
}

/// The three characters before `at`, `.rstrip()`ped, end with `fn`.
fn after_fn_keyword(code: &str, at: usize) -> bool {
    let before: String = code[..at]
        .chars()
        .rev()
        .take(3)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    py::rstrip(&before).ends_with("fn")
}

struct EnvWrite {
    path: PathBuf,
    line: usize,
    call: String,
    argument: String,
    name: Option<String>,
    fn_start: Option<usize>,
}

fn writes_in(path: &Path, text: &str, code: &str, whole_file: bool) -> Vec<EnvWrite> {
    let regions = if whole_file {
        None
    } else {
        Some(test_regions(code))
    };
    let spans = fn_spans(code);
    let mut found = Vec::new();
    for call in RAW_WRITE.find_all(code) {
        let at = start(&call);
        if after_fn_keyword(code, at) {
            continue;
        }
        if let Some(regions) = &regions
            && !regions.iter().any(|(a, b)| *a <= at && at < *b)
        {
            continue;
        }
        let argument = first_argument(code, text, end(&call));
        let fn_start = spans
            .iter()
            .filter(|(a, b)| *a <= at && at < *b)
            .max()
            .map(|(a, _)| *a);
        found.push(EnvWrite {
            path: path.to_path_buf(),
            line: py::count_newlines(text, at) + 1,
            call: group(&call, 1).to_string(),
            argument: argument.to_string(),
            name: resolve_name(argument, text),
            fn_start,
        });
    }
    found
}

fn shared_reason(name: &str) -> Option<String> {
    (SHARED_EXACT.contains(&name)
        || SHARED_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix)))
    .then(|| format!("{name} is a shared name other tests and the code under test read"))
}

/// The allowlist: (path, name) -> reason.
fn load_allowlist(path: &Path) -> Result<BTreeMap<(String, String), String>, String> {
    if !path.is_file() {
        return Ok(BTreeMap::new());
    }
    let shown = py::path_str(&py::as_posix(path));
    let text = py::read_text(path).map_err(|error| error.to_string())?;
    let data: serde_json::Value =
        serde_json::from_str(&text).map_err(|error| format!("{shown}: {error}"))?;
    let entries = data
        .get("entries")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| format!("{shown}: no \"entries\" list"))?;
    let mut allowed = BTreeMap::new();
    for entry in entries {
        let text_of = |value: Option<&serde_json::Value>| match value {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Null) | None => "None".to_string(),
            Some(other) => other.to_string(),
        };
        let reason = text_of(
            entry
                .get("reason")
                .or(Some(&serde_json::Value::String(String::new()))),
        );
        if py::strip(&reason).is_empty() {
            return Err(format!(
                "{shown}: entry for {} has no reason",
                text_of(entry.get("path"))
            ));
        }
        let file = entry
            .get("path")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("{shown}: an entry has no \"path\""))?;
        let names = entry
            .get("names")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| format!("{shown}: entry for {file} has no \"names\" list"))?;
        for name in names {
            let name = name
                .as_str()
                .ok_or_else(|| format!("{shown}: entry for {file} has a non-string name"))?;
            allowed.insert((file.to_string(), name.to_string()), reason.clone());
        }
    }
    Ok(allowed)
}

/// True when `name` occurs in `text` with no ASCII word character on either side.
fn names_word(text: &str, name: &str) -> bool {
    let word = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
    let mut from = 0;
    while let Some(found) = text[from..].find(name) {
        let at = from + found;
        let after = at + name.len();
        if !word(text[..at].chars().next_back()) && !word(text[after..].chars().next()) {
            return true;
        }
        from = re::next_char(text, at);
        if from > text.len() || name.is_empty() {
            break;
        }
    }
    false
}

/// Findings as printable lines (empty when the tree passes); `Err` when the
/// allowlist cannot be read.
pub fn check(root: &Path, allowlist: Option<&Path>) -> Result<Vec<String>, String> {
    let crates = root.join("crates");
    let default_allowlist = root.join(ALLOWLIST);
    let allowed = load_allowlist(allowlist.unwrap_or(&default_allowlist))?;
    let mut texts: Vec<(PathBuf, String)> = Vec::new();
    for path in py::rglob_pruned(&crates, &|name| name.ends_with(".rs"), &|dir| {
        dir != "target"
    }) {
        let text =
            py::read_text_lossy(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        texts.push((path, text));
    }
    let declared = test_module_files(&texts);
    let mut writes: Vec<EnvWrite> = Vec::new();
    for (path, text) in &texts {
        if !text.contains("set_var") && !text.contains("remove_var") {
            continue;
        }
        let code = mask(text, &TRIVIA);
        writes.extend(writes_in(
            path,
            text,
            &code,
            whole_file_test(path, &crates, &declared),
        ));
    }
    let mut by_name: BTreeMap<(PathBuf, String), BTreeSet<Option<usize>>> = BTreeMap::new();
    for write in &writes {
        if let Some(name) = &write.name {
            by_name
                .entry((write.path.clone(), name.clone()))
                .or_default()
                .insert(write.fn_start);
        }
    }
    let relative = |path: &Path| py::as_posix(path.strip_prefix(root).unwrap_or(path));
    let mut used = BTreeSet::new();
    let mut findings = Vec::new();
    for write in &writes {
        let rel = relative(&write.path);
        let key = (
            rel.clone(),
            write.name.clone().unwrap_or_else(|| write.argument.clone()),
        );
        if allowed.contains_key(&key) {
            used.insert(key);
            continue;
        }
        let problem = match &write.name {
            None => Some("the name is not a literal or a same-file const, so it cannot be shown unique".to_string()),
            Some(name) => shared_reason(name)
                .or_else(|| {
                    (!name.contains("TEST"))
                        .then(|| format!("{name} has no TEST in it, so it may be a name the code under test reads"))
                })
                .or_else(|| {
                    let mut others: Vec<String> = texts
                        .iter()
                        .filter(|(path, text)| *path != write.path && names_word(text, name))
                        .map(|(path, _)| relative(path))
                        .collect();
                    others.sort();
                    if !others.is_empty() {
                        let shown: Vec<&str> = others.iter().take(3).map(String::as_str).collect();
                        Some(format!("{name} is also named in {}", shown.join(", ")))
                    } else if by_name.get(&(write.path.clone(), name.clone())).is_some_and(|fns| fns.len() > 1) {
                        Some(format!("{name} is written by more than one function of this file"))
                    } else {
                        None
                    }
                }),
        };
        if let Some(problem) = problem {
            findings.push(format!(
                "{rel}:{}: {}({}): {problem}",
                write.line, write.call, write.argument
            ));
        }
    }
    for key in allowed.keys() {
        if !used.contains(key) {
            findings.push(format!(
                "{}: allowlist entry {} matches no env write; remove it",
                key.0,
                py::repr(&key.1)
            ));
        }
    }
    Ok(findings)
}

/// Entry point: `test-env-writes [--root DIR] [--allowlist PATH]`.
pub fn main(args: &[String], ctx: &Ctx, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let opts = [
        Opt {
            name: "--root",
            value: true,
        },
        Opt {
            name: "--allowlist",
            value: true,
        },
    ];
    let parsed = match cli::parse(
        "check_test_env_writes",
        "[-h] [--root ROOT] [--allowlist ALLOWLIST]",
        args,
        &opts,
        &[],
        Some(0),
        err,
    ) {
        Ok(parsed) => parsed,
        Err(exit) => return exit.code(),
    };
    let root = ctx.path(parsed.value("--root").unwrap_or("."));
    let allowlist = parsed.value("--allowlist").map(|path| ctx.path(path));
    let findings = match check(&root, allowlist.as_deref()) {
        Ok(findings) => findings,
        Err(error) => {
            let _ = writeln!(err, "check_test_env_writes: {error}");
            return 1;
        }
    };
    for finding in &findings {
        let _ = writeln!(out, "{finding}");
    }
    if !findings.is_empty() {
        let _ = writeln!(
            out,
            "check_test_env_writes: a test's process-environment write is seen by every"
        );
        let _ = writeln!(
            out,
            "concurrent test of its binary (#190). Pass the value through the code under"
        );
        let _ = writeln!(
            out,
            "test (Command::env, an env map or a builder), use a name unique to one test"
        );
        let _ = writeln!(
            out,
            "(contains TEST, named in no other file), or write through the one shared lock"
        );
        let _ = writeln!(
            out,
            "(temp_env::with_var(s) / temp_env::async_with_vars). A justified raw write"
        );
        let _ = writeln!(out, "needs an entry with its reason in {ALLOWLIST}.");
    }
    i32::from(!findings.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::write;

    // The fixtures below are Rust sources held in string literals, which this
    // guard masks, so its own scan of this file finds no raw write.
    struct Tree {
        dir: tempfile::TempDir,
    }

    impl Tree {
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().unwrap(),
            }
        }

        fn root(&self) -> &Path {
            self.dir.path()
        }

        fn write(&self, rel: &str, text: &str) {
            write(self.root(), rel, text);
        }

        fn allow(&self, entries: &serde_json::Value) {
            write(
                self.root(),
                "allowlist.json",
                &serde_json::json!({ "entries": entries }).to_string(),
            );
        }

        fn findings(&self) -> Result<Vec<String>, String> {
            check(self.root(), Some(&self.root().join("allowlist.json")))
        }
    }

    #[test]
    fn a_shared_name_in_an_inline_test_module_is_refused() {
        let tree = Tree::new();
        tree.write(
            "crates/demo/src/lib.rs",
            "
            pub fn run() {}
            #[cfg(test)]
            mod tests {
                #[test]
                fn scrubs() {
                    unsafe { std::env::set_var(\"CARGO_TARGET_DIR\", \"/x\") };
                    unsafe { std::env::remove_var(\"PATH\") };
                }
            }
        ",
        );
        let findings = tree.findings().unwrap();
        assert_eq!(findings.len(), 2, "{findings:?}");
        assert!(
            findings[0].contains("crates/demo/src/lib.rs:7: set_var(\"CARGO_TARGET_DIR\")"),
            "{findings:?}"
        );
        assert!(findings[0].contains("CARGO_TARGET_DIR is a shared name"));
        assert!(findings[1].contains("PATH is a shared name"));
    }

    #[test]
    fn a_name_unique_to_one_test_passes_literal_or_const() {
        let tree = Tree::new();
        tree.write(
            "crates/demo/src/lib.rs",
            "
            #[cfg(test)]
            mod tests {
                const SECRET: &str = \"RSI_DEMO_TEST_SECRET\";
                #[test]
                fn scrubs() {
                    unsafe { std::env::set_var(SECRET, \"x\") };
                    unsafe { std::env::set_var(\"RSI_DEMO_TEST_OTHER\", \"y\") };
                    unsafe { std::env::remove_var(SECRET) };
                    unsafe { std::env::remove_var(\"RSI_DEMO_TEST_OTHER\") };
                }
            }
        ",
        );
        assert_eq!(tree.findings().unwrap(), Vec::<String>::new());
    }

    #[test]
    fn a_name_another_file_names_or_another_function_writes_is_refused() {
        let tree = Tree::new();
        tree.write(
            "crates/demo/src/lib.rs",
            "
            #[cfg(test)]
            mod tests {
                #[test]
                fn one() { unsafe { std::env::set_var(\"RSI_DEMO_TEST_A\", \"1\") }; }
                #[test]
                fn two() { unsafe { std::env::set_var(\"RSI_DEMO_TEST_A\", \"2\") }; }
                #[test]
                fn three() { unsafe { std::env::set_var(\"RSI_DEMO_TEST_B\", \"3\") }; }
            }
        ",
        );
        tree.write(
            "crates/other/src/lib.rs",
            "pub fn read() { std::env::var(\"RSI_DEMO_TEST_B\"); }\n",
        );
        let findings = tree.findings().unwrap();
        assert_eq!(findings.len(), 3, "{findings:?}");
        assert!(findings[0].contains("RSI_DEMO_TEST_A is written by more than one function"));
        assert!(findings[1].contains("RSI_DEMO_TEST_A is written by more than one function"));
        assert!(findings[2].contains("RSI_DEMO_TEST_B is also named in crates/other/src/lib.rs"));
    }

    #[test]
    fn a_name_without_test_or_not_a_literal_is_refused() {
        let tree = Tree::new();
        tree.write(
            "crates/demo/tests/flow.rs",
            "
            fn helper(key: &str) { unsafe { std::env::set_var(key, \"v\") }; }
            #[test]
            fn flow() { unsafe { std::env::set_var(\"RSI_DEMO_URL\", \"v\") }; }
        ",
        );
        let findings = tree.findings().unwrap();
        assert_eq!(findings.len(), 2, "{findings:?}");
        assert!(findings[0].contains("set_var(key): the name is not a literal"));
        assert!(findings[1].contains("RSI_DEMO_URL has no TEST in it"));
    }

    #[test]
    fn production_code_temp_env_and_literals_are_not_test_writes() {
        let tree = Tree::new();
        tree.write(
            "crates/demo/src/lib.rs",
            "
            pub fn startup() {
                unsafe { std::env::remove_var(\"RSID_SHUTDOWN_MARKER\") };
            }
            #[cfg(not(test))]
            mod live {
                pub fn go() { unsafe { std::env::set_var(\"PATH\", \"/bin\") }; }
            }
            #[cfg(test)]
            mod tests {
                #[test]
                fn locked() {
                    let banned = \"std::env::set_var(\\\"PATH\\\", x)\"; // set_var(\"HOME\", y)
                    temp_env::with_vars([(\"PATH\", Some(\"/fake\"))], || {});
                }
            }
        ",
        );
        assert_eq!(tree.findings().unwrap(), Vec::<String>::new());
    }

    #[test]
    fn module_files_declared_under_cfg_test_are_test_code() {
        let tree = Tree::new();
        tree.write(
            "crates/demo/src/lib.rs",
            "
            #[cfg(test)]
            mod helpers;
            #[cfg(any(test, feature = \"test-seam\"))]
            #[path = \"seam_impl.rs\"]
            mod seam;
            mod live;
        ",
        );
        tree.write(
            "crates/demo/src/helpers.rs",
            "pub fn a() { unsafe { std::env::set_var(\"HOME\", \"/h\") }; }\n",
        );
        tree.write(
            "crates/demo/src/helpers/deep.rs",
            "pub fn b() { unsafe { std::env::set_var(\"TMPDIR\", \"/t\") }; }\n",
        );
        tree.write(
            "crates/demo/src/seam_impl.rs",
            "pub fn c() { unsafe { std::env::set_var(\"USER\", \"u\") }; }\n",
        );
        tree.write(
            "crates/demo/src/live.rs",
            "pub fn d() { unsafe { std::env::set_var(\"LANG\", \"C\") }; }\n",
        );
        let mut files: Vec<String> = tree
            .findings()
            .unwrap()
            .iter()
            .map(|finding| {
                finding
                    .split_once(':')
                    .map_or(finding.clone(), |(file, _)| file.to_string())
            })
            .collect();
        files.sort();
        assert_eq!(
            files,
            vec![
                "crates/demo/src/helpers.rs",
                "crates/demo/src/helpers/deep.rs",
                "crates/demo/src/seam_impl.rs"
            ]
        );
    }

    #[test]
    fn the_allowlist_accepts_named_writes_and_refuses_stale_entries() {
        let tree = Tree::new();
        tree.write(
            "crates/demo/src/main_tests.rs",
            "
            #[test]
            fn fixture() {
                unsafe { std::env::set_var(\"PATH\", \"/fake\") };
                unsafe { std::env::set_var(self.key, \"v\") };
            }
        ",
        );
        tree.allow(&serde_json::json!([{
            "path": "crates/demo/src/main_tests.rs",
            "names": ["PATH", "self.key", "GIT_DIR"],
            "reason": "serialised by the binary's own lock",
        }]));
        assert_eq!(
            tree.findings().unwrap(),
            vec![
                "crates/demo/src/main_tests.rs: allowlist entry 'GIT_DIR' matches no env write; remove it"
            ]
        );
        tree.allow(&serde_json::json!([{
            "path": "crates/demo/src/main_tests.rs", "names": ["PATH"], "reason": " ",
        }]));
        let error = tree.findings().unwrap_err();
        assert!(error.contains("has no reason"), "{error}");
        let (code, _, err) = crate::testutil::run(
            main,
            &[
                "--root",
                tree.root().to_str().unwrap(),
                "--allowlist",
                "allowlist.json",
            ],
            &Ctx::new(tree.root(), &[]),
        );
        assert_eq!(code, 1);
        assert!(err.contains("has no reason"), "{err}");
    }

    #[test]
    fn the_cli_prints_the_remedy_and_refuses_unknown_options() {
        let tree = Tree::new();
        tree.write(
            "crates/demo/tests/flow.rs",
            "#[test]\nfn flow() { unsafe { std::env::set_var(\"HOME\", \"v\") }; }\n",
        );
        let ctx = Ctx::new(tree.root(), &[]);
        let (code, out, _) = crate::testutil::run(main, &[], &ctx);
        assert_eq!(code, 1);
        assert!(
            out.starts_with(
                "crates/demo/tests/flow.rs:2: set_var(\"HOME\"): HOME is a shared name"
            ),
            "{out}"
        );
        assert!(out.ends_with(
            "needs an entry with its reason in tools/test-env-writes-allowlist.json.\n"
        ));
        let (code, _, err) = crate::testutil::run(main, &["--bogus"], &ctx);
        assert_eq!(code, 2);
        assert!(err.contains("unrecognized arguments: --bogus"), "{err}");
    }

    #[test]
    fn the_repository_tree_passes() {
        assert_eq!(
            check(&crate::testutil::repo_root(), None).unwrap(),
            Vec::<String>::new()
        );
    }
}
