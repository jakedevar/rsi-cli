//! Native held-out and generality audit. Public findings deliberately contain no term or task.
use super::{
    Result,
    dataset::{self, Task},
    split::Split,
};
use rustpython_parser::{
    Parse,
    ast::{self, Visitor},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
};
pub const DEFAULT_EXCLUDES: [&str; 2] =
    ["eval/bench/tb2/dataset.toml", "eval/bench/tb2/split.json"];
const CANDIDATES: &str = "eval/bench/tb2/candidates/";
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub label: &'static str,
    pub file: String,
    pub line: usize,
    pub kind: &'static str,
}
impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {}:{} {}",
            self.label, self.file, self.line, self.kind
        )
    }
}
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum Terms {
    #[value(name = "held_out")]
    HeldOut,
    Search,
    All,
}
#[derive(Debug, Clone)]
struct Term {
    kind: &'static str,
    text: String,
}
fn literal_lines(text: &str) -> BTreeSet<String> {
    text.lines()
        .map(str::trim)
        .filter(|s| s.chars().count() >= 16)
        .map(str::to_owned)
        .collect()
}
fn strip_docstring(body: &mut Vec<ast::Stmt>) {
    if matches!(body.first(),Some(ast::Stmt::Expr(e)) if matches!(&*e.value,ast::Expr::Constant(_)))
    {
        body.remove(0);
    }
}
#[derive(Default)]
struct Literals(BTreeSet<String>);
impl Visitor for Literals {
    fn visit_stmt(&mut self, mut node: ast::Stmt) {
        match &mut node {
            ast::Stmt::FunctionDef(n) => strip_docstring(&mut n.body),
            ast::Stmt::AsyncFunctionDef(n) => strip_docstring(&mut n.body),
            ast::Stmt::ClassDef(n) => strip_docstring(&mut n.body),
            _ => {}
        }
        self.generic_visit_stmt(node);
    }
    fn visit_expr_constant(&mut self, node: ast::ExprConstant) {
        if let ast::Constant::Str(text) = node.value {
            self.0.extend(literal_lines(&text));
        }
    }
}
fn py_literals(text: &str) -> BTreeSet<String> {
    match ast::Suite::parse(text, "<benchmark test>") {
        Ok(mut suite) => {
            strip_docstring(&mut suite);
            let mut lits = Literals::default();
            for node in suite {
                lits.visit_stmt(node);
            }
            lits.0
        }
        Err(_) => literal_lines(text),
    }
}
fn test_terms(dir: &Path) -> Result<(BTreeSet<String>, BTreeSet<String>)> {
    fn walk(dir: &Path, names: &mut BTreeSet<String>, lits: &mut BTreeSet<String>) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            // Do not descend through directory symlinks (rglob does not either).
            if fs::symlink_metadata(&path)?.is_dir() {
                walk(&path, names, lits)?;
            } else if path.is_file() {
                names.insert(
                    path.file_name()
                        .ok_or("missing test filename")?
                        .to_string_lossy()
                        .into_owned(),
                );
                if path.extension().is_some_and(|s| s == "sh")
                    || fs::metadata(&path)?.len() > 256 * 1024
                {
                    continue;
                }
                if let Ok(text) = fs::read_to_string(&path) {
                    lits.extend(if path.extension().is_some_and(|s| s == "py") {
                        py_literals(&text)
                    } else {
                        literal_lines(&text)
                    });
                }
            }
        }
        Ok(())
    }
    let (mut names, mut lits) = (BTreeSet::new(), BTreeSet::new());
    let tests = dir.join("tests");
    if tests.is_dir() {
        walk(&tests, &mut names, &mut lits)?;
    }
    Ok((names, lits))
}
fn build_terms(
    checkout: Option<&Path>,
    tasks: &[Task],
    selected: &[String],
    literals: bool,
) -> Result<Vec<Term>> {
    let mut terms: Vec<_> = selected
        .iter()
        .map(|n| Term {
            kind: "task-name",
            text: n.clone(),
        })
        .collect();
    let Some(root) = checkout else {
        return Ok(terms);
    };
    let mut per_task = BTreeMap::new();
    let (mut names, mut lits) = (
        BTreeMap::<String, usize>::new(),
        BTreeMap::<String, usize>::new(),
    );
    for t in tasks {
        let pair = test_terms(&root.join(&t.name))?;
        for name in &pair.0 {
            *names.entry(name.clone()).or_default() += 1;
        }
        for lit in &pair.1 {
            *lits.entry(lit.clone()).or_default() += 1;
        }
        per_task.insert(&t.name, pair);
    }
    for name in selected {
        if let Some((ns, ls)) = per_task.get(name) {
            terms.extend(ns.iter().filter(|n| names[*n] == 1).map(|n| Term {
                kind: "test-file",
                text: n.clone(),
            }));
            if literals {
                terms.extend(ls.iter().filter(|s| lits[*s] == 1).map(|s| Term {
                    kind: "literal",
                    text: s.clone(),
                }));
            }
        }
    }
    Ok(terms)
}
fn token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}
fn has_token(text: &str, name: &str) -> bool {
    text.match_indices(name).any(|(i, _)| {
        !text[..i].chars().next_back().is_some_and(token_char)
            && !text[i + name.len()..]
                .chars()
                .next()
                .is_some_and(token_char)
    })
}
pub fn normalize(path: &Path, root: &Path) -> String {
    let absolute = path.is_absolute();
    let path = if absolute {
        path.canonicalize().unwrap_or_else(|_| path.into())
    } else {
        path.into()
    };
    let root = root.canonicalize().unwrap_or_else(|_| root.into());
    let relative = path.strip_prefix(&root).unwrap_or(&path);
    let mut normalized = PathBuf::new();
    for c in relative.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir if normalized.file_name().is_some_and(|n| n != "..") => {
                normalized.pop();
            }
            _ => normalized.push(c.as_os_str()),
        }
    }
    normalized.to_string_lossy().replace('\\', "/")
}
fn excluded(path: &str, patterns: &[String]) -> bool {
    !path.starts_with(CANDIDATES)
        && patterns
            .iter()
            .any(|p| glob::Pattern::new(p).is_ok_and(|p| p.matches(path)))
}
pub fn candidate_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut names = vec![
        "overlay.toml".to_string(),
        "instructions.md".into(),
        "notes.md".into(),
    ];
    let overlay = dir.join("overlay.toml");
    if overlay.is_file() {
        // Parse TOML so single-quoted instruction paths are also included.
        let doc: toml::Value = toml::from_str(&fs::read_to_string(overlay)?)?;
        if let Some(name) = doc.get("instructions").and_then(toml::Value::as_str) {
            names.push(name.into());
        }
    }
    let mut files = Vec::new();
    for name in names {
        let path = dir.join(name);
        if path.is_file() && !files.contains(&path) {
            files.push(path);
        }
    }
    if files.is_empty() {
        return Err(format!(
            "{}: no candidate files (overlay.toml, instructions, notes.md)",
            dir.display()
        )
        .into());
    }
    Ok(files)
}
pub type Item = (String, usize, String);
pub fn file_lines(paths: &[PathBuf]) -> Result<Vec<Item>> {
    let mut items = Vec::new();
    for path in paths {
        let files = if path.is_dir() {
            candidate_files(path)?
        } else {
            vec![path.clone()]
        };
        for file in files {
            let bytes = fs::read(&file)?;
            let Ok(text) = String::from_utf8(bytes) else {
                continue;
            };
            let path = file.canonicalize()?;
            for (i, line) in text.lines().enumerate() {
                items.push((path.to_string_lossy().into_owned(), i + 1, line.into()));
            }
        }
    }
    Ok(items)
}
pub fn added_lines(diff: &str) -> Vec<Item> {
    let mut items = Vec::new();
    let mut path = None;
    let mut lineno = 0;
    let hunk = regex::Regex::new(r"^@@ -\d+(?:,\d+)? \+(\d+)").expect("constant regex");
    for raw in diff.lines() {
        if let Some(tgt) = raw.strip_prefix("+++ ") {
            let tgt = tgt.split('\t').next().unwrap_or("");
            path = if tgt == "/dev/null" {
                None
            } else {
                Some(tgt.strip_prefix("b/").unwrap_or(tgt).to_string())
            };
        } else if raw.starts_with("@@") {
            lineno = hunk
                .captures(raw)
                .and_then(|c| c[1].parse().ok())
                .unwrap_or(0);
        } else if let Some(path) = &path {
            if let Some(text) = raw.strip_prefix('+') {
                items.push((path.clone(), lineno, text.into()));
                lineno += 1;
            } else if raw.starts_with(' ') {
                lineno += 1;
            }
        }
    }
    items
}
pub fn audit_items(
    root: &Path,
    dataset: &Path,
    split: &Path,
    checkout: Option<&Path>,
    mode: Terms,
    items: &[Item],
    excludes: &[String],
) -> Result<Vec<Finding>> {
    let tasks = dataset::load(dataset)?.task;
    let split: Split = serde_json::from_str(&fs::read_to_string(split)?)?;
    let mut passes = Vec::new();
    if matches!(mode, Terms::HeldOut | Terms::All) {
        passes.push(("HIT", build_terms(checkout, &tasks, &split.held_out, true)?));
    }
    if matches!(mode, Terms::Search | Terms::All) {
        passes.push((
            "GENERALITY",
            build_terms(checkout, &tasks, &split.search, false)?,
        ));
    }
    let mut findings = Vec::new();
    for (label, terms) in passes {
        for (path, line, text) in items {
            let file = normalize(Path::new(path), root);
            if excluded(&file, excludes) {
                continue;
            }
            for term in &terms {
                if if term.kind == "task-name" {
                    has_token(text, &term.text)
                } else {
                    text.contains(&term.text)
                } {
                    findings.push(Finding {
                        label,
                        file: file.clone(),
                        line: *line,
                        kind: term.kind,
                    });
                }
            }
        }
    }
    Ok(findings)
}
/// Audit explicit files against the pin files in a repository. No subprocess or interpreter.
pub fn audit_files(
    root: &Path,
    files: &[PathBuf],
    checkout: Option<&Path>,
    mode: Terms,
) -> Result<Vec<Finding>> {
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let paths: Vec<_> = files
        .iter()
        .map(|p| {
            if p.is_absolute() {
                p.clone()
            } else {
                root.join(p)
            }
        })
        .collect();
    audit_items(
        root,
        &root.join(super::DATASET),
        &root.join(super::SPLIT),
        checkout,
        mode,
        &file_lines(&paths)?,
        &DEFAULT_EXCLUDES.map(str::to_owned),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_parser_extracts_decoded_literals_skips_docstrings_and_falls_back_on_syntax_errors() {
        let source = "\"\"\"module documentation that is long enough\"\"\"\nclass C:\n    \"\"\"class documentation that is long enough\"\"\"\n    def f(self):\n        \"\"\"function documentation that is long enough\"\"\"\n        assert out == 'first expected output\\nsecond expected output'\nasync def g():\n    \"\"\"async documentation that is long enough\"\"\"\n    return 'third expected output'\n";
        assert_eq!(
            py_literals(source),
            BTreeSet::from([
                "first expected output".into(),
                "second expected output".into(),
                "third expected output".into()
            ])
        );
        assert_eq!(
            py_literals("this is invalid syntax with long content $$$"),
            literal_lines("this is invalid syntax with long content $$$")
        );
    }
}
