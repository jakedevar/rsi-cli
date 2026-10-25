//! Starter digest drift guard (#3070).
//!
//! A bundled starter topology (`crates/rsid/src/topology/starters/*.json`) is
//! pinned twice: the digest fixture in `starters_tests.rs` holds its *current*
//! canonical digest, and `SHIPPED_BEFORE` in `starters.rs` holds the digest of
//! every definition it *replaced* (an unedited stored copy at one of those is
//! upgraded at boot instead of reading as `starter_drift`). Changing a starter's
//! JSON therefore moves both together:
//!
//! 1. the fixture takes the new canonical digest (the failing fixture test prints
//!    it), and
//! 2. the digest the fixture held before is appended to that starter's
//!    `SHIPPED_BEFORE` entry.
//!
//! The Rust tests catch a missed step only when a gate selects and runs them; #3045
//! changed all three issue-pipeline starters and left both pins alone, and the
//! drift surfaced as three baseline reds on later gates. This guard needs no build
//! of `rsid`. It compares the starter sources at `BASE` and `HEAD` (text only) and
//! refuses when a starter's JSON changed but
//!
//! * its fixture still pins the digest it pinned at `BASE`, or
//! * the digest it pinned at `BASE` is not in its `SHIPPED_BEFORE` entry now,
//!
//! and, at `HEAD` alone, when a fixture pins a digest that `SHIPPED_BEFORE`
//! also lists as replaced, a `SHIPPED_BEFORE` entry repeats a digest or drops one
//! it held at `BASE` (history is append-only), or a starter has no fixture or no
//! history entry. Any retired digest literal outside `starters.rs` under the
//! topology directory is also refused: tests must share the current fixture
//! instead of carrying an independently stale pin (#411).
//!
//! It cannot compute the canonical digest itself (that is the typed definition
//! re-serialized by `rsid`), so it never says a pin is *correct*; the Rust
//! fixture tests still do. A change that leaves the typed digest unchanged (a key
//! the definition ignores) is the one thing this guard refuses wrongly: keep such
//! a JSON edit out of the starters directory, or move the pins anyway.
//!
//! Usage: `rsi-repo-guard starter-pins [BASE [HEAD]]` (defaults `origin/rolling`
//! and `HEAD`); `--tree` checks the working tree alone. Exit 0 when the sources
//! are consistent or have no starters.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::Path;

use crate::re::{Lazy, Re, group};
use crate::{Ctx, py};

pub const STARTERS_DIR: &str = "crates/rsid/src/topology";
pub const SOURCE_PATH: &str = "crates/rsid/src/topology/starters.rs";
pub const TESTS_PATH: &str = "crates/rsid/src/topology/starters_tests.rs";

const DIGEST: &str = r"sha256:[0-9a-f]{64}";
/// Where each starter's current digest is pinned in `starters_tests.rs`, keyed by the
/// starter's name constant in `starters.rs`. A starter without a site here is refused,
/// so a new starter cannot ship without a fixture this guard can read.
pub const PIN_SITES: &[(&str, &str)] = &[
    (
        "ISSUE_IMPLEMENT_REVIEW_LAND",
        r#"\(\s*ISSUE_IMPLEMENT_REVIEW_LAND,\s*"(sha256:[0-9a-f]{64})""#,
    ),
    (
        "ROLLING_QA_SWEEP",
        r#"\(\s*ROLLING_QA_SWEEP,\s*"(sha256:[0-9a-f]{64})""#,
    ),
    (
        "ISSUE_PIPELINE",
        r#"const ISSUE_PIPELINE_DIGEST: &str =\s*"(sha256:[0-9a-f]{64})""#,
    ),
    (
        "ISSUE_PIPELINE_REVIEWED",
        r#"const REVIEWED_DIGEST: &str =\s*"(sha256:[0-9a-f]{64})""#,
    ),
    (
        "ISSUE_PIPELINE_POST_REVIEW",
        r#"const POST_REVIEW_DIGEST: &str =\s*"(sha256:[0-9a-f]{64})""#,
    ),
];
static DIGEST_RE: Lazy = Lazy::new(|| Re::new(DIGEST));
static NAME_CONST_RE: Lazy =
    Lazy::new(|| Re::new(r#"pub\(crate\) const (\w+): &str = "([a-z0-9-]+)";"#));
static STARTER_RE: Lazy =
    Lazy::new(|| Re::new(r#"name:\s*(\w+),\s*definition:\s*include_str!\("starters/([^"]+)"\)"#));
const HISTORY_START: &str = "SHIPPED_BEFORE: &[(&str, &[&str])] = &[";
static HISTORY_ENTRY_RE: Lazy = Lazy::new(|| Re::new(r"(?s)\(\s*(\w+),\s*&\[(.*?)\],?\s*\)"));

pub const HOW_TO_MOVE: &str = concat!(
    "a starter JSON change moves both pins: set the fixture in starters_tests.rs to the ",
    "new canonical digest (the failing fixture test prints it) and append the digest the ",
    "fixture held before to that starter's SHIPPED_BEFORE entry in starters.rs"
);

/// The starter sources of one revision: file texts keyed by repository path.
#[derive(Debug, Clone, Default)]
pub struct Sources {
    pub source: Option<String>,
    pub tests: Option<String>,
    pub jsons: BTreeMap<String, String>,
    pub topology_texts: BTreeMap<String, String>,
}

/// One bundled starter as a revision declares it.
#[derive(Debug, Clone)]
pub struct Starter {
    pub name: String,
    pub filename: String,
    pub json_text: Option<String>,
    pub pin: Option<String>,
    pub history: Option<Vec<String>>,
}

impl Sources {
    #[must_use]
    pub fn new(
        source: Option<String>,
        tests: Option<String>,
        jsons: BTreeMap<String, String>,
        topology_texts: BTreeMap<String, String>,
    ) -> Self {
        let mut topology_texts = topology_texts;
        if let Some(tests) = &tests {
            topology_texts.insert(TESTS_PATH.to_string(), tests.clone());
        }
        Self {
            source,
            tests,
            jsons,
            topology_texts,
        }
    }

    /// Every starter this revision bundles, by name constant, in declaration order.
    #[must_use]
    pub fn parse(&self) -> Vec<(String, Starter)> {
        let source = self.source.as_deref().unwrap_or("");
        let names: HashMap<String, String> = NAME_CONST_RE
            .find_all(source)
            .iter()
            .map(|caps| (group(caps, 1).to_string(), group(caps, 2).to_string()))
            .collect();
        let history = parse_history(source);
        let mut starters: Vec<(String, Starter)> = Vec::new();
        for caps in STARTER_RE.find_all(source) {
            let (constant, filename) = (group(&caps, 1), group(&caps, 2));
            let starter = Starter {
                name: names
                    .get(constant)
                    .cloned()
                    .unwrap_or_else(|| constant.to_string()),
                filename: filename.to_string(),
                json_text: self.jsons.get(filename).cloned(),
                pin: find_pin(self.tests.as_deref().unwrap_or(""), constant),
                history: history.get(constant).cloned(),
            };
            match starters.iter_mut().find(|(known, _)| known == constant) {
                Some(slot) => slot.1 = starter,
                None => starters.push((constant.to_string(), starter)),
            }
        }
        starters
    }
}

fn find_pin(tests: &str, constant: &str) -> Option<String> {
    let (_, pattern) = PIN_SITES.iter().find(|(site, _)| *site == constant)?;
    let found = Re::new(pattern).findall1(tests);
    if found.len() == 1 {
        found.into_iter().next()
    } else {
        None
    }
}

fn parse_history(source: &str) -> HashMap<String, Vec<String>> {
    let Some(at) = source.find(HISTORY_START) else {
        return HashMap::new();
    };
    let block = &source[at + HISTORY_START.len()..];
    let block = block.find("\n];").map_or(block, |end| &block[..end]);
    HISTORY_ENTRY_RE
        .find_all(block)
        .iter()
        .map(|caps| {
            let digests = DIGEST_RE
                .find_all(group(caps, 2))
                .iter()
                .map(|d| group(d, 0).to_string())
                .collect();
            (group(caps, 1).to_string(), digests)
        })
        .collect()
}

/// Python `==` on two decoded JSON documents (numbers by value, `true == 1`).
fn json_eq(left: &serde_json::Value, right: &serde_json::Value) -> bool {
    use serde_json::Value;
    let number = |value: &Value| match value {
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        Value::Number(n) => n.as_f64(),
        _ => None,
    };
    match (left, right) {
        (Value::Number(a), Value::Number(b)) if a.is_f64() || b.is_f64() => {
            number(left) == number(right)
        }
        (Value::Number(a), Value::Number(b)) => a == b,
        (Value::Bool(_), Value::Number(_)) | (Value::Number(_), Value::Bool(_)) => {
            number(left) == number(right)
        }
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| json_eq(x, y))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, x)| b.get(key).is_some_and(|y| json_eq(x, y)))
        }
        _ => left == right,
    }
}

/// True when two JSON texts are one document (formatting and key order aside).
fn same_definition(left: Option<&str>, right: Option<&str>) -> bool {
    let parse = |text: Option<&str>| serde_json::from_str::<serde_json::Value>(text.unwrap_or(""));
    match (parse(left), parse(right)) {
        (Ok(a), Ok(b)) => json_eq(&a, &b),
        _ => left == right,
    }
}

/// The refusals for `head` against `base` (`None`: no base, check `head` alone).
#[must_use]
pub fn check(base: Option<&Sources>, head: &Sources) -> Vec<String> {
    let Some(head_source) = head.source.as_deref() else {
        return Vec::new();
    };
    let mut errors = Vec::new();
    let head_starters = head.parse();
    let base_starters: Vec<(String, Starter)> = base
        .filter(|base| base.source.is_some())
        .map(Sources::parse)
        .unwrap_or_default();
    let retired: HashSet<String> = parse_history(head_source).into_values().flatten().collect();
    for (path, text) in &head.topology_texts {
        if path == SOURCE_PATH {
            continue;
        }
        for (index, line) in py::splitlines(text).into_iter().enumerate() {
            let mut digests: Vec<String> = DIGEST_RE
                .find_all(line)
                .iter()
                .map(|caps| group(caps, 0).to_string())
                .filter(|digest| retired.contains(digest))
                .collect();
            digests.sort();
            digests.dedup();
            for digest in digests {
                errors.push(format!(
                    "{path}:{}: retired starter digest literal {digest} is in SHIPPED_BEFORE; use the shared current \
                     fixture in starters_tests.rs instead of duplicating a pin",
                    index + 1
                ));
            }
        }
    }
    for (constant, starter) in &head_starters {
        let label = format!("starter {}", starter.name);
        if !PIN_SITES.iter().any(|(site, _)| site == constant) {
            errors.push(format!(
                "{label}: no pin site in crates/rsi-repo-guards/src/starter_pins.rs PIN_SITES \
                 (add the fixture in starters_tests.rs and its pattern)"
            ));
            continue;
        }
        if starter.json_text.is_none() {
            errors.push(format!(
                "{label}: starters/{} is not in the tree",
                starter.filename
            ));
            continue;
        }
        if starter.pin.is_none() {
            errors.push(format!(
                "{label}: its digest fixture is missing or ambiguous in starters_tests.rs"
            ));
        }
        let Some(history) = &starter.history else {
            errors.push(format!("{label}: no SHIPPED_BEFORE entry in starters.rs"));
            continue;
        };
        if history.iter().collect::<HashSet<_>>().len() != history.len() {
            errors.push(format!("{label}: SHIPPED_BEFORE repeats a digest"));
        }
        if let Some(pin) = &starter.pin
            && history.contains(pin)
        {
            errors.push(format!(
                "{label}: the fixture pins {pin}, which SHIPPED_BEFORE lists as a definition it replaced \
                 (the current digest is never in the history)"
            ));
        }
        let Some((_, earlier)) = base_starters.iter().find(|(known, _)| known == constant) else {
            continue;
        };
        let dropped: Vec<&str> = earlier
            .history
            .iter()
            .flatten()
            .filter(|digest| !history.contains(digest))
            .map(String::as_str)
            .collect();
        if !dropped.is_empty() {
            errors.push(format!(
                "{label}: SHIPPED_BEFORE dropped {} (history is append-only)",
                dropped.join(", ")
            ));
        }
        if earlier.json_text.is_none()
            || same_definition(earlier.json_text.as_deref(), starter.json_text.as_deref())
        {
            continue;
        }
        let (Some(before), Some(now)) = (&earlier.pin, &starter.pin) else {
            continue;
        };
        if now == before {
            errors.push(format!(
                "{label}: starters/{} changed but its digest fixture still pins {before}",
                starter.filename
            ));
        }
        if !history.contains(before) {
            errors.push(format!(
                "{label}: starters/{} changed but SHIPPED_BEFORE lacks the digest it replaced, {before}",
                starter.filename
            ));
        }
    }
    if !errors.is_empty() {
        errors.push(HOW_TO_MOVE.to_string());
    }
    errors
}

/// `git show REF:PATH` as text (universal newlines), `None` when git refuses.
fn git_text(ctx: &Ctx, repo: &Path, reference: &str, path: &str) -> Result<Option<String>, String> {
    let done = ctx
        .git()
        .arg("-C")
        .arg(repo)
        .args(["show", &format!("{reference}:{path}")])
        .output()
        .map_err(|error| format!("git show: {error}"))?;
    if !done.status.success() {
        // Only a path the revision's tree does not list is absent; any other failure is
        // inconclusive and must refuse rather than read as "no such file".
        let listed = ctx
            .git()
            .arg("-C")
            .arg(repo)
            .args(["ls-tree", "--name-only", reference, "--", path])
            .output()
            .map_err(|error| format!("git ls-tree: {error}"))?;
        return if listed.status.success() && listed.stdout.is_empty() {
            Ok(None)
        } else {
            Err(format!(
                "cannot read {reference}:{path}: {}",
                String::from_utf8_lossy(&done.stderr).trim()
            ))
        };
    }
    let text =
        String::from_utf8(done.stdout).map_err(|error| format!("{reference}:{path}: {error}"))?;
    Ok(Some(py::universal_newlines(&text)))
}

/// The starter sources at git revision `reference`.
pub fn load_ref(ctx: &Ctx, repo: &Path, reference: &str) -> Result<Sources, String> {
    let source = git_text(ctx, repo, reference, SOURCE_PATH)?;
    let listed = ctx
        .git()
        .arg("-C")
        .arg(repo)
        .args([
            "ls-tree",
            "-r",
            "--name-only",
            reference,
            "--",
            STARTERS_DIR,
        ])
        .output()
        .map_err(|error| format!("git ls-tree: {error}"))?;
    if !listed.status.success() {
        return Err(format!(
            "git ls-tree -r --name-only {reference} -- {STARTERS_DIR} failed: {}",
            py::strip(&String::from_utf8_lossy(&listed.stderr))
        ));
    }
    let listed = py::universal_newlines(&String::from_utf8_lossy(&listed.stdout));
    let mut jsons = BTreeMap::new();
    for caps in STARTER_RE.find_all(source.as_deref().unwrap_or("")) {
        let filename = group(&caps, 2);
        if let Some(text) = git_text(
            ctx,
            repo,
            reference,
            &format!("{STARTERS_DIR}/starters/{filename}"),
        )? {
            jsons.insert(filename.to_string(), text);
        }
    }
    let mut topology = BTreeMap::new();
    for path in py::splitlines(&listed) {
        if let Some(text) = git_text(ctx, repo, reference, path)? {
            topology.insert(path.to_string(), text);
        }
    }
    let tests = git_text(ctx, repo, reference, TESTS_PATH)?;
    Ok(Sources::new(source, tests, jsons, topology))
}

/// The starter sources of the working tree at `repo`.
pub fn load_tree(repo: &Path) -> Result<Sources, String> {
    let read = |path: &str| -> Result<Option<String>, String> {
        let target = repo.join(path);
        if !target.is_file() {
            return Ok(None);
        }
        py::read_text(&target)
            .map(Some)
            .map_err(|error| error.to_string())
    };
    let source = read(SOURCE_PATH)?;
    let mut jsons = BTreeMap::new();
    for caps in STARTER_RE.find_all(source.as_deref().unwrap_or("")) {
        let filename = group(&caps, 2);
        if let Some(text) = read(&format!("{STARTERS_DIR}/starters/{filename}"))? {
            jsons.insert(filename.to_string(), text);
        }
    }
    let mut topology = BTreeMap::new();
    for path in py::rglob(&repo.join(STARTERS_DIR), &|_| true) {
        let rel = py::as_posix(path.strip_prefix(repo).unwrap_or(&path));
        topology.insert(
            rel,
            py::read_text(&path).map_err(|error| error.to_string())?,
        );
    }
    Ok(Sources::new(source, read(TESTS_PATH)?, jsons, topology))
}

/// Entry point: `starter-pins [BASE [HEAD]] | --tree`.
pub fn main(args: &[String], ctx: &Ctx, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    // #407: the unchanged legacy scoped-test calls this Rust entry point with
    // BASE/HEAD. Enforce additions here until #423 switches that caller; do not
    // edit/extend the Python runner or replace it in this independent slice.
    if !args.iter().any(|arg| arg == "--tree") {
        let code = crate::new_python::main(args, ctx, out, err);
        if code != 0 {
            // Preserve starter-pins' existing error exit (including a missing
            // revision); the standalone new-python CLI still uses 2 for errors.
            return 1;
        }
    }
    let repo = ctx.cwd.clone();
    let positional: Vec<&String> = args.iter().filter(|arg| !arg.starts_with("--")).collect();
    let errors = if args.iter().any(|arg| arg == "--tree") {
        load_tree(&repo).map(|head| check(None, &head))
    } else {
        let base_ref = positional
            .first()
            .map_or("origin/rolling", |arg| arg.as_str());
        let head_ref = positional.get(1).map_or("HEAD", |arg| arg.as_str());
        load_ref(ctx, &repo, base_ref).and_then(|base| {
            let head = load_ref(ctx, &repo, head_ref)?;
            Ok(check(base.source.is_some().then_some(&base), &head))
        })
    };
    let errors = match errors {
        Ok(errors) => errors,
        Err(error) => {
            let _ = writeln!(err, "check_starter_pins: {error}");
            return 1;
        }
    };
    if errors.is_empty() {
        return 0;
    }
    let _ = writeln!(
        err,
        "check_starter_pins: starter digest pins drifted from the shipped JSON (#3070):"
    );
    for error in &errors {
        let _ = writeln!(err, "  {error}");
    }
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{git, repo_root, write};

    fn digest(c: char) -> String {
        format!("sha256:{}", c.to_string().repeat(64))
    }

    const SOURCE: &str = "\
pub(crate) const ISSUE_PIPELINE: &str = \"issue-pipeline\";
pub(crate) const ISSUE_PIPELINE_REVIEWED: &str = \"issue-pipeline-reviewed\";
static STARTERS: [Starter; 2] = [
    Starter {
        id: Uuid::nil(),
        name: ISSUE_PIPELINE,
        definition: include_str!(\"starters/issue-pipeline.json\"),
    },
    Starter {
        id: Uuid::nil(),
        name: ISSUE_PIPELINE_REVIEWED,
        definition: include_str!(\"starters/issue-pipeline-reviewed.json\"),
    },
];
pub(crate) const SHIPPED_BEFORE: &[(&str, &[&str])] = &[
    (
        ISSUE_PIPELINE,
        &[
%(pipeline)s
        ],
    ),
    (
        ISSUE_PIPELINE_REVIEWED,
        &[\"%(reviewed)s\"],
    ),
];

fn shipped_before() {}
";

    const TESTS: &str = "\
const ISSUE_PIPELINE_DIGEST: &str =
    \"%(pipeline)s\";
const REVIEWED_DIGEST: &str =
    \"%(reviewed)s\";
";

    fn fill(template: &str, pipeline: &str, reviewed: &str) -> String {
        template
            .replace("%(pipeline)s", pipeline)
            .replace("%(reviewed)s", reviewed)
    }

    fn sources_with(
        pipeline_pin: char,
        pipeline_history: &[char],
        reviewed_pin: char,
        pipeline_json: &serde_json::Value,
        reviewed_json: &serde_json::Value,
    ) -> Sources {
        let history: Vec<String> = pipeline_history
            .iter()
            .map(|c| format!("            \"{}\",", digest(*c)))
            .collect();
        let jsons = BTreeMap::from([
            ("issue-pipeline.json".to_string(), pipeline_json.to_string()),
            (
                "issue-pipeline-reviewed.json".to_string(),
                reviewed_json.to_string(),
            ),
        ]);
        Sources::new(
            Some(fill(SOURCE, &history.join("\n"), &digest('e'))),
            Some(fill(TESTS, &digest(pipeline_pin), &digest(reviewed_pin))),
            jsons,
            BTreeMap::new(),
        )
    }

    fn sources(pipeline_pin: char, pipeline_history: &[char]) -> Sources {
        sources_with(
            pipeline_pin,
            pipeline_history,
            'b',
            &serde_json::json!({"nodes": ["a"]}),
            &serde_json::json!({"nodes": ["r"]}),
        )
    }

    fn changed(pipeline_pin: char, pipeline_history: &[char]) -> Sources {
        sources_with(
            pipeline_pin,
            pipeline_history,
            'b',
            &serde_json::json!({"nodes": ["a", "b"]}),
            &serde_json::json!({"nodes": ["r"]}),
        )
    }

    fn any(errors: &[String], needle: &str) -> bool {
        errors.iter().any(|e| e.contains(needle))
    }

    #[test]
    fn a_consistent_tree_passes() {
        assert_eq!(
            check(None, &sources('a', &['1', '2'])),
            Vec::<String>::new()
        );
    }

    #[test]
    fn retired_literals_are_refused_anywhere_outside_starters_source() {
        let head = sources('a', &['1']);
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let path = format!("{STARTERS_DIR}/nested/issue_close_tests.rs");
        let mut files = vec![
            (SOURCE_PATH.to_string(), head.source.clone().unwrap()),
            (TESTS_PATH.to_string(), head.tests.clone().unwrap()),
            (
                path.clone(),
                format!("// regression\nconst PIN: &str = \"{}\";\n", digest('1')),
            ),
        ];
        for (name, text) in &head.jsons {
            files.push((format!("{STARTERS_DIR}/starters/{name}"), text.clone()));
        }
        for (name, text) in &files {
            write(repo, name, text);
        }
        git(repo, &["init", "-q"]);
        git(repo, &["add", "."]);
        git(repo, &["commit", "-qm", "fixture"]);
        let ctx = Ctx::new(repo, &[]);
        for mut loaded in [
            load_tree(repo).unwrap(),
            load_ref(&ctx, repo, "HEAD").unwrap(),
        ] {
            let errors = check(None, &loaded);
            assert_eq!(errors.len(), 2, "{errors:?}");
            assert!(errors[0].contains(&format!(
                "{path}:2: retired starter digest literal {}",
                digest('1')
            )));
            assert!(errors[0].contains("shared current fixture"));
            loaded.topology_texts.insert(
                path.clone(),
                format!("const PIN: &str = \"{}\";", digest('a')),
            );
            assert_eq!(check(None, &loaded), Vec::<String>::new());
        }
    }

    #[test]
    fn the_repository_starters_are_consistent() {
        let root = repo_root();
        assert_eq!(
            check(None, &load_tree(&root).unwrap()),
            Vec::<String>::new()
        );
        let (code, _, err) = crate::testutil::run(main, &["--tree"], &Ctx::new(&root, &[]));
        assert_eq!(code, 0, "{err}");
    }

    #[test]
    fn every_repository_starter_has_a_pin_site() {
        let starters = load_tree(&repo_root()).unwrap().parse();
        assert!(!starters.is_empty());
        for (constant, starter) in &starters {
            assert!(
                PIN_SITES.iter().any(|(site, _)| site == constant),
                "{constant}"
            );
            assert!(starter.pin.is_some(), "{}", starter.name);
            assert!(starter.history.is_some(), "{}", starter.name);
        }
    }

    #[test]
    fn a_changed_starter_with_both_pins_untouched_is_refused() {
        // #3045: the JSON moved, the fixture and SHIPPED_BEFORE did not.
        let errors = check(Some(&sources('a', &['1'])), &changed('a', &['1']));
        assert!(
            any(&errors, &format!("fixture still pins {}", digest('a'))),
            "{errors:?}"
        );
        assert!(
            any(
                &errors,
                &format!("lacks the digest it replaced, {}", digest('a'))
            ),
            "{errors:?}"
        );
        assert_eq!(errors.last().unwrap(), HOW_TO_MOVE);
    }

    #[test]
    fn a_moved_fixture_without_the_replaced_digest_is_refused() {
        let errors = check(Some(&sources('a', &['1'])), &changed('c', &['1']));
        assert_eq!(
            errors
                .iter()
                .filter(|e| e.contains("lacks the digest it replaced"))
                .count(),
            1,
            "{errors:?}"
        );
        assert!(!any(&errors, "fixture still pins"), "{errors:?}");
    }

    #[test]
    fn a_replaced_digest_appended_without_moving_the_fixture_is_refused() {
        let errors = check(Some(&sources('a', &['1'])), &changed('a', &['1', 'a']));
        assert!(
            any(&errors, "lists as a definition it replaced"),
            "{errors:?}"
        );
        assert!(any(&errors, "fixture still pins"), "{errors:?}");
    }

    #[test]
    fn moving_both_pins_together_passes() {
        assert_eq!(
            check(Some(&sources('a', &['1'])), &changed('c', &['1', 'a'])),
            Vec::<String>::new()
        );
    }

    #[test]
    fn reformatting_or_reordering_the_json_is_not_a_change() {
        let mut base = sources('a', &['1']);
        let mut head = sources('a', &['1']);
        head.jsons.insert(
            "issue-pipeline.json".into(),
            "{\n    \"nodes\": [\n        \"a\"\n    ]\n}".into(),
        );
        base.jsons
            .insert("issue-pipeline.json".into(), "{\"nodes\": [\"a\"]}".into());
        assert_eq!(check(Some(&base), &head), Vec::<String>::new());
        base.jsons
            .insert("issue-pipeline.json".into(), "{\"b\": 1, \"a\": 2}".into());
        head.jsons.insert(
            "issue-pipeline.json".into(),
            "{\"a\": 2,\n \"b\": 1}".into(),
        );
        assert_eq!(check(Some(&base), &head), Vec::<String>::new());
    }

    #[test]
    fn only_the_changed_starter_is_judged() {
        let base = sources('a', &['1']);
        assert_eq!(
            check(Some(&base), &changed('c', &['1', 'a'])),
            Vec::<String>::new()
        );
        let stale = sources_with(
            'a',
            &['1'],
            'b',
            &serde_json::json!({"nodes": ["a"]}),
            &serde_json::json!({"nodes": ["r", "s"]}),
        );
        let errors = check(Some(&base), &stale);
        assert!(!errors.is_empty());
        assert!(
            errors
                .iter()
                .all(|e| e.contains("issue-pipeline-reviewed") || e == HOW_TO_MOVE),
            "{errors:?}"
        );
    }

    #[test]
    fn history_is_append_only_and_unique() {
        let errors = check(Some(&sources('a', &['1', '2'])), &sources('a', &['1']));
        assert!(
            any(&errors, &format!("dropped {}", digest('2'))),
            "{errors:?}"
        );
        let errors = check(None, &sources('a', &['1', '1']));
        assert!(any(&errors, "repeats a digest"), "{errors:?}");
    }

    #[test]
    fn a_starter_without_a_pin_site_or_history_is_refused() {
        let mut head = sources('a', &['1']);
        head.source = head.source.map(|s| {
            s.replace(
                "ISSUE_PIPELINE_REVIEWED,\n        &[",
                "ISSUE_UNKNOWN,\n        &[",
            )
        });
        assert!(any(&check(None, &head), "no SHIPPED_BEFORE entry"));
        let mut head = sources('a', &['1']);
        head.tests = head
            .tests
            .map(|s| s.replace("REVIEWED_DIGEST", "SOMETHING_ELSE"));
        assert!(any(&check(None, &head), "digest fixture is missing"));
        let mut head = sources('a', &['1']);
        head.source = head.source.map(|s| {
            s.replace(
                "pub(crate) const ISSUE_PIPELINE_REVIEWED",
                "pub(crate) const ISSUE_NEW",
            )
            .replace("name: ISSUE_PIPELINE_REVIEWED,", "name: ISSUE_NEW,")
        });
        let errors = check(None, &head);
        assert!(any(&errors, "no pin site"), "{errors:?}");
    }

    #[test]
    fn a_tree_without_starters_has_nothing_to_check() {
        assert_eq!(check(None, &Sources::default()), Vec::<String>::new());
        assert_eq!(
            check(Some(&Sources::default()), &Sources::default()),
            Vec::<String>::new()
        );
    }

    #[test]
    fn the_cli_prints_refusals_on_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let base = sources('a', &['1']);
        write(repo, SOURCE_PATH, base.source.as_deref().unwrap());
        write(repo, TESTS_PATH, base.tests.as_deref().unwrap());
        for (name, text) in &base.jsons {
            write(repo, &format!("{STARTERS_DIR}/starters/{name}"), text);
        }
        git(repo, &["init", "-q"]);
        git(repo, &["add", "."]);
        git(repo, &["commit", "-qm", "base"]);
        write(
            repo,
            &format!("{STARTERS_DIR}/starters/issue-pipeline.json"),
            "{\"nodes\": [\"a\", \"b\"]}",
        );
        git(repo, &["commit", "-qam", "edit"]);
        let ctx = Ctx::new(repo, &[]);
        let (code, out, err) = crate::testutil::run(main, &["HEAD~1", "HEAD"], &ctx);
        assert_eq!((code, out.as_str()), (1, ""));
        assert!(err.starts_with("check_starter_pins: starter digest pins drifted from the shipped JSON (#3070):\n  starter issue-pipeline: "), "{err}");
        assert_eq!(crate::testutil::run(main, &["HEAD~1", "HEAD~1"], &ctx).0, 0);
        assert_eq!(
            crate::testutil::run(main, &["no-such-ref", "HEAD"], &ctx).0,
            1
        );
    }
}
