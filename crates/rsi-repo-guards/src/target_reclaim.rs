//! Literal target writers and conservative local fixture provenance (#362, #424).
// guard-regressions: stdout
use crate::{
    Ctx,
    cli::{self, Opt},
    re::{self, Re},
};
use std::sync::{Arc, LazyLock, Mutex};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::Path,
};

pub const POLICY: &str = "crates/rsid-store/src/sandbox/target_reclaim.rs";
pub const SHARED: &str = "crates/rsi-common/src/sandbox_storage.rs";
pub const RETAINED: &str = "scripts/target-reclaim-retained.json";
pub const NEXTEST: &str = ".config/nextest.toml";
const NAME: &str = r"[A-Za-z0-9_.-]+";
const PATH_STEP: &str = r#"(?:\s*\.(?:path|canonicalize|unwrap|to_path_buf|clone|as_path)\(\s*\)|\s*\.expect\(\s*"[^"\n]*"\s*\)|\s*\.name|\s*\.join\(\s*["'][^"'\n]*["']\s*\)|\s*/\s*["'][^"'\n]*["']|\?)"#;
const TEMP_ROOT: &str = r#"(?:tempfile::tempdir\(\s*\)|(?:tempfile::)?TempDir::new\(\s*\)|(?:crate::test_support::)?disk_backed_tempdir\(\s*"[^"\n]*"\s*\)|tempfile\.TemporaryDirectory\(\s*\))"#;
const ASSIGN: &str = r"\b(?:let\s+(?:mut\s+)?)?([A-Za-z_]\w*)(?:\s*:\s*[^=;\n]+)?\s*=\s*([^;\n]*(?:\n\s*\.[^;\n]*)*)";
const TARGET_ROOT: &str = r#"(?:\.join\(\s*["'](?:\.\./)*target["']\s*\)|(?:Path|PathBuf)(?:::\w+)?\(\s*["'](?:\.\./)*target["']\s*\)|/\s*["']target["']|["']CARGO_TARGET_DIR["']|(?:^|["/])target["']?\}?["']?$)"#;
static FIXTURE: LazyLock<Re> = LazyLock::new(|| {
    Re::new(&format!(
        r"^(?:{TEMP_ROOT}|\b[A-Za-z_]\w*)(?:{PATH_STEP})*$"
    ))
});
static TEMPORARY: LazyLock<Re> = LazyLock::new(|| Re::new(&format!("^{TEMP_ROOT}")));
static BASE: LazyLock<Re> = LazyLock::new(|| Re::new(r"^\w+"));
static COMPONENTS: LazyLock<Re> =
    LazyLock::new(|| Re::new(r#"(?:\.join\(\s*|/\s*)["']([^"']*)["']"#));
static DRIVE: LazyLock<Re> = LazyLock::new(|| Re::new(r"^[A-Za-z]:"));
static STAMPED: LazyLock<Re> =
    LazyLock::new(|| Re::new(r"^stamped_repository\(\s*&\s*([A-Za-z_]\w*)\s*\)$"));

// #447: the look-around assertions the writer patterns used to carry made
// fancy-regex run them in its backtracking VM at every offset of every file
// (about two minutes in a dev build). They are removed from the pattern text,
// so the linear-time engine runs the pattern, and re-checked on each candidate
// below. Each pattern has a single viable parse per start offset (the
// component is a maximal run of characters the look-ahead set excludes), so a
// rejected candidate is exactly a failed original match at that start.
static ASSIGN_RE: LazyLock<Re> = LazyLock::new(|| Re::new(ASSIGN));
static TARGET_ROOT_RE: LazyLock<Re> = LazyLock::new(|| Re::new(TARGET_ROOT));
static WORD_OR_DASH: LazyLock<Re> = LazyLock::new(|| Re::new(r"^[\w-]$"));
static COMPONENT_END: LazyLock<Re> = LazyLock::new(|| Re::new(r#"^[/\s"');,\\}\]]$"#));

/// A literal every `TEMP_ROOT` alternative contains.
const TEMP_HINTS: [&str; 4] = [
    "tempfile::tempdir(",
    "TempDir::new(",
    "disk_backed_tempdir(",
    "tempfile.TemporaryDirectory(",
];
static MASK_CANDIDATES: LazyLock<Re> =
    LazyLock::new(|| Re::new(&format!(r"(?:{TEMP_ROOT}|\b[A-Za-z_]\w*)(?:{PATH_STEP})*")));
static HIT_LITERAL: LazyLock<Re> = LazyLock::new(|| {
    Re::new(&format!(
        r"(?:target/|\$(?:CARGO_TARGET_DIR|\{{CARGO_TARGET_DIR\}})/)({NAME})"
    ))
});
static HIT_ROOT_JOIN: LazyLock<Re> =
    LazyLock::new(|| Re::new(&format!(r#"^\s*\.join\(\s*["\x27]({NAME})"#)));
static HIT_OSENV: LazyLock<Re> = LazyLock::new(|| {
    Re::new(&format!(
        r#"os\.path\.join\(\s*(?:os\.environ|env)\[\s*["']CARGO_TARGET_DIR["']\s*\]\s*,\s*["']({NAME})"#
    ))
});
static HIT_ASSIGN_JOIN: LazyLock<Re> = LazyLock::new(|| {
    Re::new(&format!(
        r#"\.join\(\s*["\x27]({NAME})|/\s*["\x27]({NAME})"#
    ))
});
/// The alias writer patterns with a literal every match must contain (a cheap
/// exact pre-test: a text without it has no match, so the scan is skipped).
static HIT_ALIASES: LazyLock<[(Re, fn(&str) -> bool); 5]> = LazyLock::new(|| {
    let alias = r"(?P<alias>[A-Za-z_]\w*)";
    [
        (
            format!(r#"\b{alias}\s*\.join\(\s*["\x27]({NAME})"#),
            (|t| t.contains(".join(")) as fn(&str) -> bool,
        ),
        (
            format!(r#"\b{alias}\s*/\s*["\x27]({NAME})"#),
            (|t| t.contains('/')) as fn(&str) -> bool,
        ),
        (
            format!(r#"\b{alias}\s*\+\s*["\x27]/({NAME})"#),
            (|t| t.contains("\"/") || t.contains("'/")) as fn(&str) -> bool,
        ),
        (
            format!(r#"os\.path\.join\(\s*{alias}\s*,\s*["\x27]({NAME})"#),
            (|t| t.contains("os.path.join(")) as fn(&str) -> bool,
        ),
        (
            format!(r"\$\{{?{alias}\}}?/({NAME})"),
            (|t| t.contains('$')) as fn(&str) -> bool,
        ),
    ]
    .map(|(pattern, needs)| (Re::new(&pattern), needs))
});

/// `(?=/|[\s"');,\\}\]]|$)` at `end`.
fn component_ends(text: &str, end: usize) -> bool {
    text[end..].chars().next().is_none_or(|c| {
        let mut buf = [0_u8; 4];
        COMPONENT_END.is_match(c.encode_utf8(&mut buf))
    })
}
/// `(?<![\w-])` at `start`.
fn no_word_before(text: &str, start: usize) -> bool {
    text[..start].chars().next_back().is_none_or(|c| {
        let mut buf = [0_u8; 4];
        !WORD_OR_DASH.is_match(c.encode_utf8(&mut buf))
    })
}
/// End of the component group `group` of `caps` satisfies the look-ahead.
fn component_group_ends(text: &str, caps: &re::Captures<'_>, group: usize) -> bool {
    caps.get(group)
        .is_some_and(|m| component_ends(text, m.end()))
}
/// Every assignment of `code`: `ASSIGN` without its `=(?!=)` look-ahead, which
/// is re-checked here (the first character after the `=` is not another `=`).
fn assign_matches(code: &str) -> Vec<re::Captures<'_>> {
    ASSIGN_RE.find_all_checked(code, &|caps| {
        let expr_start = caps.get(2).map_or(0, |m| m.start());
        !code[code[..expr_start].trim_end().len()..].starts_with('=')
    })
}

// The tree scanner revisits the same lexical patterns for every binding/file.
// Keep compilation bounded and shared; do not spend the scan budget compiling
// an identical pattern thousands of times. Policy-derived patterns are bounded
// too: once the cache is full, compile without retaining new entries.
fn rx(pattern: &str) -> Arc<Re> {
    static CACHE: LazyLock<Mutex<BTreeMap<String, Arc<Re>>>> =
        LazyLock::new(|| Mutex::new(BTreeMap::new()));
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(pattern) = cache.get(pattern) {
        return Arc::clone(pattern);
    }
    let compiled = Arc::new(Re::new(pattern));
    if cache.len() < 256 {
        cache.insert(pattern.to_string(), Arc::clone(&compiled));
    }
    compiled
}

fn compact(text: &str) -> String {
    rx(r"\s+")
        .sub(&rx(r"//[^\n]*").sub(text, ""), "")
        .into_owned()
}
fn function(source: &str, name: &str) -> Result<String, String> {
    rx(&format!(r"(?s)fn {name}\([^\n]*\) -> bool \{{(.*?)\n\}}"))
        .search(source)
        .map(|c| compact(re::group(&c, 1)))
        .ok_or_else(|| format!("cannot read {name} in {POLICY}"))
}
#[derive(Debug)]
pub struct Policy {
    pub directories: BTreeSet<String>,
    pub files: BTreeSet<String>,
    pub fixture_suffix: String,
    pub idle_prefix: String,
    pub idle_suffix: String,
}
impl Policy {
    pub fn allows(&self, name: &str) -> bool {
        let pid = name
            .strip_prefix(&self.idle_prefix)
            .and_then(|s| s.strip_suffix(&self.idle_suffix));
        self.directories.contains(name)
            || self.files.contains(name)
            || (name.ends_with(&self.fixture_suffix) && name.len() > self.fixture_suffix.len())
            || pid.is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
    }
}
pub fn read_policy(root: &Path) -> Result<Policy, String> {
    let source = fs::read_to_string(root.join(POLICY)).map_err(|e| e.to_string())?;
    let body = function(&source, "is_reclaimable_target_entry")?;
    let byte = format!(r#"b"({NAME})""#);
    let atom = format!(r#"(?:b"{NAME}"|[A-Z][A-Z0-9_]*)"#);
    let arms = format!(r"({atom}(?:\|{atom})*)");
    let pattern = format!(
        r"^constFIXTURE_SUFFIX:&\[u8\]={byte};ifkind==SFlag::S_IFDIR\{{matches!\(name,{arms}\)\|\|name==E2E_ARTIFACTS_DIR_NAME.as_bytes\(\)\|\|\(name.len\(\)>FIXTURE_SUFFIX.len\(\)&&name.ends_with\(FIXTURE_SUFFIX\)\)\}}elseifkind==SFlag::S_IFREG\{{is_idle_cost_scratch\(name\)\|\|is_python_discover_scratch\(name\)\|\|matches!\(name,{arms}\)\}}else\{{false\}}$"
    );
    let matcher = rx(&pattern);
    let m = matcher.search(&body).ok_or_else(|| format!("unsupported reclaim predicate in {POLICY}; update read_policy in scripts/repo-guard target-reclaim"))?;
    let constants = compact(&format!(
        "{source}\n{}",
        fs::read_to_string(root.join(SHARED)).map_err(|e| e.to_string())?
    ));
    let names = |arms: &str| -> Result<BTreeSet<String>, String> {
        arms.split('|')
            .map(|arm| {
                if arm.starts_with("b\"") {
                    Ok(arm[2..arm.len() - 1].into())
                } else {
                    rx(&format!(r#"const{arm}:(?:&\[u8\]|&str)=b?"({NAME})";"#))
                        .search(&constants)
                        .map(|c| re::group(&c, 1).into())
                        .ok_or_else(|| format!("unresolved reclaim constant {arm} in {POLICY}"))
                }
            })
            .collect()
    };
    let directories = names(&format!("{}|E2E_ARTIFACTS_DIR_NAME", re::group(&m, 2)))?;
    let mut files = names(re::group(&m, 3))?;
    let discover = function(&source, "is_python_discover_scratch")?;
    let dm = rx(&format!(r"^matches!\(name,{arms}\)$"))
        .search(&discover)
        .ok_or_else(|| format!("unsupported is_python_discover_scratch in {POLICY}"))?;
    files.extend(names(re::group(&dm, 1))?);
    let idle = function(&source, "is_idle_cost_scratch")?;
    let im = rx(&format!(r"^letSome\(pid\)=name.strip_prefix\({byte}\).and_then\(\|rest\|rest.strip_suffix\({byte}\)\)else\{{returnfalse;\}};!pid.is_empty\(\)&&pid.iter\(\).all\(u8::is_ascii_digit\)$")).search(&idle).ok_or_else(|| format!("unsupported is_idle_cost_scratch in {POLICY}"))?;
    Ok(Policy {
        directories,
        files,
        fixture_suffix: re::group(&m, 1).into(),
        idle_prefix: re::group(&im, 1).into(),
        idle_suffix: re::group(&im, 2).into(),
    })
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Target,
    Fixture,
    Unknown,
}
#[derive(Default)]
struct Scopes {
    offsets: Vec<usize>,
    scopes: Vec<Vec<usize>>,
    parameters: Vec<(usize, String)>,
}

// Scope declarations inside Python source strings are fixture data, not real
// functions. The old AST reader made that distinction; retain it without an
// interpreter. Mask both quote styles, triple quotes, escapes and comments.
fn python_scope_code(source: &str) -> String {
    let mut masked = source.as_bytes().to_vec();
    let bytes = source.as_bytes();
    let mut at = 0;
    let mut continuation_tails = Vec::new();
    while at < bytes.len() {
        let start = at;
        if bytes[at] == b'#' {
            while at < bytes.len() && bytes[at] != b'\n' {
                at += 1;
            }
        } else if matches!(bytes[at], b'\'' | b'"') {
            let quote = bytes[at];
            let triple = bytes.get(at..at + 3) == Some(&[quote, quote, quote]);
            let width = if triple { 3 } else { 1 };
            at += width;
            while at < bytes.len() {
                if bytes[at] == b'\\' {
                    at = (at + 2).min(bytes.len());
                } else if bytes
                    .get(at..at + width)
                    .is_some_and(|s| s.iter().all(|b| *b == quote))
                {
                    at += width;
                    break;
                } else if !triple && bytes[at] == b'\n' {
                    break;
                } else {
                    at += 1;
                }
            }
            // The closing line of a multiline literal is still part of the
            // statement that opened it. Its suffix (for example `.replace`)
            // cannot dedent a Python function, even when the fixture text's
            // closing delimiter starts at column zero. Only this scope view
            // is masked; the writer scan continues to inspect the literal.
            if bytes[start..at].contains(&b'\n') {
                let end = bytes[at..]
                    .iter()
                    .position(|b| *b == b'\n')
                    .map_or(bytes.len(), |n| at + n);
                continuation_tails.push((at, end));
            }
        } else {
            at += 1;
            continue;
        }
        for b in &mut masked[start..at] {
            if *b != b'\n' {
                *b = b' ';
            }
        }
    }
    // Scan every token before hiding continuation tails: another multiline
    // literal can begin on the same closing line (a tuple of source fixtures).
    for (start, end) in continuation_tails {
        masked[start..end].fill(b' ');
    }
    String::from_utf8(masked).unwrap_or_else(|_| source.to_string())
}
impl Scopes {
    fn at(&self, offset: usize) -> &[usize] {
        &self.scopes[self
            .offsets
            .partition_point(|n| *n <= offset)
            .saturating_sub(1)]
    }
    fn parse(code: &str) -> Self {
        let mut events: Vec<(usize, bool)> = Vec::new();
        let mut parameters = Vec::new();
        // Python source remains data during the transition. Track def/class scopes
        // by indentation without invoking an interpreter. Control blocks do not
        // bind locals. Rust and shell scopes use braces with quoted text masked.
        let python_code = python_scope_code(code);
        let python = rx(r"(?m)^\s*(?:async\s+)?(?:def|class)\s+\w+").is_match(&python_code)
            && !rx(r"(?m)^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+\w+")
                .is_match(&python_code);
        if python {
            let definition =
                rx(r"^(?:async\s+)?(def|class)\s+\w+\s*(?:\((.*)\))?[^\n]*:\s*(?:#.*)?$");
            let mut stack: Vec<(usize, usize)> = Vec::new();
            let mut offset = 0;
            let lines: Vec<&str> = python_code.split_inclusive('\n').collect();
            for (i, line) in lines.iter().enumerate() {
                let stripped = line.trim();
                let indent = line.len() - line.trim_start().len();
                if !stripped.is_empty() && !stripped.starts_with('#') {
                    while stack.last().is_some_and(|(n, _)| indent <= *n) {
                        stack.pop();
                        events.push((offset, false));
                    }
                }
                if let Some(m) = definition.search(stripped) {
                    let body = lines
                        .iter()
                        .skip(i + 1)
                        .find(|s| !s.trim().is_empty())
                        .map_or(0, |s| s.len() - s.trim_start().len());
                    let at = offset + line.len() + body;
                    events.push((at, true));
                    stack.push((indent, at));
                    if re::group(&m, 1) == "def" {
                        for arg in re::group(&m, 2).split(',') {
                            let arg = arg
                                .trim()
                                .trim_start_matches('*')
                                .split([':', '='])
                                .next()
                                .unwrap_or("")
                                .trim();
                            if rx(r"^\w+$").is_match(arg) {
                                parameters.push((at, arg.to_string()));
                            }
                        }
                    }
                }
                offset += line.len();
            }
            for _ in stack {
                events.push((code.len(), false));
            }
        } else {
            let tokens =
                rx(r#"(?s)"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\\n])*'|//[^\n]*|/\*.*?\*/|\#[^\n]*|[{}]"#);
            for m in tokens.find_all(code) {
                match re::group(&m, 0) {
                    "{" => events.push((re::end(&m), true)),
                    "}" => events.push((re::end(&m), false)),
                    _ => {}
                }
            }
            for header in rx(r"\bfn\s+\w+[^;{]*\{").find_all(code) {
                for name in rx(r"\b(\w+)\s*:\s*[^:]").findall1(re::group(&header, 0)) {
                    parameters.push((re::end(&header), name));
                }
            }
        }
        events.sort_by_key(|(at, _)| *at);
        let mut result = Self {
            offsets: vec![0],
            scopes: vec![Vec::new()],
            parameters,
        };
        let mut stack = Vec::new();
        for (id, (at, opening)) in events.into_iter().enumerate() {
            if opening {
                stack.push(id);
            } else {
                stack.pop();
            }
            result.offsets.push(at);
            result.scopes.push(stack.clone());
        }
        result
    }
}
struct Paths {
    scopes: Scopes,
    bindings: BTreeMap<String, Vec<(usize, Vec<usize>, Kind)>>,
    stamped: bool,
}
impl Paths {
    fn lookup(&self, var: &str, at: usize) -> Kind {
        let scope = self.scopes.at(at);
        self.bindings
            .get(var)
            .into_iter()
            .flatten()
            .rev()
            .find(|(end, s, _)| *end <= at && scope.starts_with(s))
            .map(|(_, _, k)| *k)
            .unwrap_or(if var == "CARGO_TARGET_DIR" {
                Kind::Target
            } else {
                Kind::Unknown
            })
    }
    fn fixture(&self, expr: &str, at: usize) -> bool {
        let mut expr = expr.trim().strip_prefix('&').unwrap_or(expr.trim()).trim();
        if expr.starts_with("Path(") && expr.ends_with(')') {
            expr = &expr[5..expr.len() - 1];
        }
        // #424: only a locally verified immutable path-returning helper may
        // transfer fixture provenance; its name alone never exempts a writer.
        if self.stamped {
            if let Some(m) = STAMPED.search(expr) {
                return self.lookup(re::group(&m, 1), at) == Kind::Fixture;
            }
        }
        // Cheap provenance first (a typical identifier is no fixture variable
        // and no temporary-directory constructor); the pure checks commute.
        if !TEMPORARY.is_match(expr) {
            let Some(base) = BASE.find_range_at(expr, 0) else {
                return false;
            };
            if self.lookup(&expr[base], at) != Kind::Fixture {
                return false;
            }
        }
        if !FIXTURE.is_match(expr) {
            return false;
        }
        COMPONENTS.findall1(expr).iter().all(|p| safe_child(p))
    }
    fn bind(&mut self, var: &str, end: usize, at: usize, kind: Kind) {
        self.bindings.entry(var.to_string()).or_default().push((
            end,
            self.scopes.at(at).to_vec(),
            kind,
        ));
    }
}
fn safe_child(path: &str) -> bool {
    !path.starts_with(['/', '\\'])
        && !path.replace('\\', "/").split('/').any(|p| p == "..")
        && !DRIVE.is_match(path)
}
fn stamped_helper(code: &str) -> bool {
    if !code.contains("fn stamped_repository") {
        return false;
    }
    let Ok((masked, hidden)) = crate::store_boundary::production(code, "fixture.rs") else {
        return false;
    };
    let _ = masked;
    let Ok(raw) = crate::rust_source::mask(code, "fixture.rs") else {
        return false;
    };
    let definitions = rx(r"\bfn\s+stamped_repository\b").find_all(&raw);
    if definitions.len() != 1 {
        return false;
    }
    let start = re::start(&definitions[0]);
    if !hidden.iter().any(|(a, b)| *a <= start && start < *b) {
        return false;
    }
    let Some(header) = rx(
        r"fn\s+stamped_repository\(\s*(\w+)\s*:\s*&\s*tempfile::TempDir\s*\)\s*->\s*PathBuf\s*\{",
    )
    .search(&raw[start..]) else {
        return false;
    };
    let at = start + re::end(&header) - 1;
    let Ok(end) = crate::rust_source::closing(&raw, at) else {
        return false;
    };
    let body = &code[at + 1..end - 1];
    let param = re::group(&header, 1);
    let pattern = format!(
        r#"(?s)^\s*let\s+(\w+)\s*=\s*{param}\.path\(\)\.join\(\s*"([^"\n]*)"\s*\);(.*)\b\1\s*$"#
    );
    let Some(m) = rx(&pattern).search(body) else {
        return false;
    };
    if !safe_child(re::group(&m, 2)) {
        return false;
    }
    let var = re::group(&m, 1);
    // No shadow/reassignment of either path or input, and no early return.
    let middle = re::group(&m, 3);
    let Ok(middle) = crate::rust_source::mask(middle, "stamped_repository") else {
        return false;
    };
    !rx(&format!(
        r"\b(?:{var}|{param})\s*=(?!=)|\blet\s+(?:mut\s+)?(?:{var}|{param})\b|\breturn\b"
    ))
    .is_match(&middle)
}
fn root_tail<'a>(expr: &'a str, root: &re::Captures<'_>) -> &'a str {
    let tail = &expr[re::end(root)..];
    if matches!(
        re::group(root, 0),
        "\"CARGO_TARGET_DIR\"" | "'CARGO_TARGET_DIR'"
    ) {
        let mut depth = 0_usize;
        for m in rx(r#""(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|[\(\)\[\]]"#).find_all(tail) {
            match re::group(&m, 0) {
                "(" | "[" => depth += 1,
                ")" | "]" if depth == 0 => return &tail[re::end(&m)..],
                ")" | "]" => depth -= 1,
                _ => {}
            }
        }
    }
    tail
}
fn local_paths(code: &str, stamped: bool) -> (String, Paths) {
    local_paths_version(code, stamped, None)
}
/// Frozen detector semantics selected only after authenticating a historical
/// guard blob. These do not inherit new fixture helpers or reclaim policy.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HistoricalVersion {
    Global,
    Scoped,
    Conservative,
}
fn local_paths_version(
    code: &str,
    stamped: bool,
    version: Option<HistoricalVersion>,
) -> (String, Paths) {
    if version == Some(HistoricalVersion::Global) {
        let mut aliases = BTreeSet::from(["CARGO_TARGET_DIR".to_string()]);
        let assignments = assign_matches(code);
        for m in &assignments {
            let expr = re::group(m, 2);
            if let Some(root) = rx(TARGET_ROOT).find_all(expr).last() {
                if !rx(r#"\.join\(|/\s*["']"#).is_match(root_tail(expr, root)) {
                    aliases.insert(re::group(m, 1).to_string());
                }
            }
        }
        for _ in 0..assignments.len() {
            let before = aliases.len();
            for m in &assignments {
                if aliases.contains(re::group(m, 2).trim_matches([' ', '"', '\'', '{', '}', '$'])) {
                    aliases.insert(re::group(m, 1).to_string());
                }
            }
            if aliases.len() == before {
                break;
            }
        }
        let mut paths = Paths {
            scopes: Scopes {
                offsets: vec![0],
                scopes: vec![Vec::new()],
                parameters: Vec::new(),
            },
            bindings: BTreeMap::new(),
            stamped: false,
        };
        for alias in aliases {
            paths.bind(&alias, 0, 0, Kind::Target);
        }
        return (code.to_string(), paths);
    }
    let sticky = version != Some(HistoricalVersion::Scoped);
    let mut paths = Paths {
        scopes: Scopes::parse(code),
        bindings: BTreeMap::new(),
        stamped,
    };
    let assignments = assign_matches(code);
    let mut events: Vec<(usize, Option<usize>, String)> = assignments
        .iter()
        .enumerate()
        .map(|(i, m)| (re::start(m), Some(i), String::new()))
        .collect();
    events.extend(
        paths
            .scopes
            .parameters
            .iter()
            .map(|(at, n)| (*at, None, n.clone())),
    );
    events.sort_by_key(|(at, m, _)| (*at, m.is_some()));
    let roots = rx(TARGET_ROOT);
    for (at, assignment, parameter) in events {
        let Some(i) = assignment else {
            let kind = if sticky
                && (parameter == "target"
                    || parameter == "CARGO_TARGET_DIR"
                    || paths.lookup(&parameter, at) == Kind::Target)
            {
                Kind::Target
            } else {
                Kind::Unknown
            };
            paths.bind(&parameter, at, at, kind);
            continue;
        };
        let m = &assignments[i];
        let var = re::group(m, 1);
        let expr = re::group(m, 2);
        let mut kind = Kind::Unknown;
        if paths.fixture(expr, at) {
            kind = Kind::Fixture;
        } else {
            let rs = roots.find_all(expr);
            if let Some(root) = rs.last() {
                if !rx(r#"\.join\(|/\s*["']"#).is_match(root_tail(expr, root)) {
                    kind = Kind::Target;
                }
            } else if paths.lookup(expr.trim_matches([' ', '"', '\'', '{', '}', '$']), at)
                == Kind::Target
            {
                kind = Kind::Target;
            }
            if sticky && paths.lookup(var, at) == Kind::Target {
                kind = Kind::Target;
            } else if sticky && kind == Kind::Unknown && rs.is_empty() {
                if let Some(reference) =
                    rx(r"^\s*(?:[A-Za-z_]\w*\(\s*|Path\([^\n]*\)\s*/\s*)([A-Za-z_]\w*)\s*\)?\s*$")
                        .search(expr)
                {
                    if paths.lookup(re::group(&reference, 1), at) == Kind::Target {
                        kind = Kind::Target;
                    }
                }
            }
        }
        paths.bind(var, re::end(m), at, kind);
    }
    // `fixture` holds only for a temporary-directory constructor or a variable
    // bound to one, so a text with neither masks nothing: skip the pass.
    let fixtures_possible = TEMP_HINTS.iter().any(|hint| code.contains(hint))
        || paths
            .bindings
            .values()
            .flatten()
            .any(|(_, _, kind)| *kind == Kind::Fixture);
    if !fixtures_possible {
        return (code.to_string(), paths);
    }
    let mut masked = code.as_bytes().to_vec();
    let all = MASK_CANDIDATES.find_ranges(code);
    for range in all {
        if paths.fixture(&code[range.clone()], range.start) {
            for b in &mut masked[range] {
                if *b != b'\n' {
                    *b = b' ';
                }
            }
        }
    }
    (String::from_utf8(masked).unwrap_or_default(), paths)
}

pub fn entries(text: &str) -> Vec<(usize, String)> {
    entries_version(text, None)
}
pub fn historical_entries(text: &str, version: HistoricalVersion) -> Vec<(usize, String)> {
    entries_version(text, Some(version))
}
fn entries_version(text: &str, version: Option<HistoricalVersion>) -> Vec<(usize, String)> {
    if !text.contains("target") && !text.contains("CARGO_TARGET_DIR") {
        return Vec::new();
    }
    let code = text
        .lines()
        .map(|s| {
            if s.trim_start().starts_with(['#', '*']) || s.trim_start().starts_with("//") {
                ""
            } else {
                s
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let (code, paths) = if version.is_none() {
        local_paths(&code, stamped_helper(text))
    } else {
        local_paths_version(&code, false, version)
    };
    let mut hits = BTreeSet::new();
    if code.contains("target/") || code.contains("CARGO_TARGET_DIR") {
        for m in HIT_LITERAL.find_all_checked(&code, &|c| {
            no_word_before(&code, re::start(c)) && component_group_ends(&code, c, 1)
        }) {
            hits.insert((re::start(&m), re::group(&m, 1).to_string()));
        }
    }
    for root in TARGET_ROOT_RE.find_all(&code) {
        let tail = &code[re::end(&root)..];
        if let Some(m) = HIT_ROOT_JOIN.search(tail) {
            if component_group_ends(tail, &m, 1) {
                hits.insert((re::end(&root) + re::start(&m), re::group(&m, 1).to_string()));
            }
        }
    }
    if code.contains("os.path.join(") {
        for m in HIT_OSENV.find_all_checked(&code, &|c| component_group_ends(&code, c, 1)) {
            hits.insert((re::start(&m), re::group(&m, 1).to_string()));
        }
    }
    for m in assign_matches(&code) {
        let expr = re::group(&m, 2);
        if let Some(root) = TARGET_ROOT_RE.find_all(expr).last() {
            let tail = root_tail(expr, root);
            if let Some(join) = HIT_ASSIGN_JOIN.search_checked(tail, &|c| {
                component_group_ends(tail, c, if c.get(1).is_some() { 1 } else { 2 })
            }) {
                let name = if re::group(&join, 1).is_empty() {
                    re::group(&join, 2)
                } else {
                    re::group(&join, 1)
                };
                if !matches!(name, ".." | "target") {
                    hits.insert((
                        m.get(2).map_or(0, |c| c.start()) + re::start(root),
                        name.to_string(),
                    ));
                }
            }
        }
    }
    for (pattern, needs) in HIT_ALIASES.iter() {
        if !needs(&code) {
            continue;
        }
        for m in pattern.find_all_checked(&code, &|c| component_group_ends(&code, c, 2)) {
            if paths.lookup(re::group(&m, 1), re::start(&m)) == Kind::Target {
                hits.insert((re::start(&m), re::group(&m, 2).to_string()));
            }
        }
    }
    hits.into_iter()
        .filter(|(_, n)| !matches!(n.as_str(), "." | ".."))
        .map(|(at, n)| (crate::rust_source::line(&code, at), n))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}
pub fn nextest_entries(text: &str) -> Result<Vec<(usize, String)>, String> {
    let config: toml::Value = toml::from_str(text).map_err(|e| e.to_string())?;
    let directory = config.get("store").and_then(|s| s.get("dir"));
    let directory = match directory {
        None => "target/nextest",
        Some(v) => v
            .as_str()
            .ok_or_else(|| format!("nextest store directory in {NEXTEST} must be a string"))?,
    };
    Ok(entries(directory))
}
pub fn check(root: &Path) -> Result<Vec<String>, String> {
    let policy = read_policy(root)?;
    let retained: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join(RETAINED)).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let retained = retained
        .as_object()
        .ok_or_else(|| format!("{RETAINED} must be an object"))?;
    for (path, entry) in retained {
        if !entry["reason"]
            .as_str()
            .is_some_and(|r| !r.trim().is_empty())
            || !entry["entries"].as_array().is_some_and(|a| {
                a.iter().all(|n| {
                    n.as_str()
                        .is_some_and(|n| rx(&format!("^{NAME}$")).is_match(n))
                })
            })
        {
            return Err(format!(
                "{RETAINED}: {path} needs explicit entry names and a retention reason"
            ));
        }
    }
    fn visit(root: &Path, files: &mut Vec<std::path::PathBuf>) -> Result<(), String> {
        if !root.exists() {
            return Ok(());
        }
        let mut items = fs::read_dir(root)
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        items.sort_by_key(|e| e.path());
        // os.walk visits parent files before child directories.
        for item in &items {
            let ft = item.file_type().map_err(|e| e.to_string())?;
            if !ft.is_dir() && !ft.is_symlink() {
                files.push(item.path());
            }
        }
        for item in items {
            if item.file_type().map_err(|e| e.to_string())?.is_dir()
                && !matches!(
                    item.file_name().to_str(),
                    Some("target" | "node_modules" | ".git" | "__pycache__")
                )
            {
                visit(&item.path(), files)?;
            }
        }
        Ok(())
    }
    let mut findings = Vec::new();
    for directory in ["crates", "scripts", "desktop/scripts", ".config"] {
        let mut files = Vec::new();
        visit(&root.join(directory), &mut files)?;
        for path in files {
            let relative =
                crate::os_escape::decode_path(path.strip_prefix(root).map_err(|e| e.to_string())?)
                    .replace('\\', "/");
            if relative != NEXTEST
                && !matches!(
                    path.extension()
                        .map_or(Some(""), |s| s.to_str())
                        .unwrap_or("\u{0}"),
                    "" | "rs" | "sh" | "py" | "mjs" | "js" | "cjs" | "ts"
                )
            {
                continue;
            }
            let text = fs::read_to_string(path).map_err(|e| e.to_string())?;
            let writer_entries = if relative == NEXTEST {
                nextest_entries(&text)?
            } else {
                entries(&text)
            };
            for (line, name) in writer_entries {
                let allowed = retained
                    .get(&relative)
                    .and_then(|r| r["entries"].as_array())
                    .is_some_and(|a| a.iter().any(|n| n.as_str() == Some(&name)));
                if !policy.allows(&name) && !allowed {
                    findings.push(format!("{relative}:{line}: target/{name} is not reclaimable; edit is_reclaimable_target_entry in {POLICY}, or document retention in {RETAINED} (pins the whole target tree)"));
                }
            }
        }
    }
    Ok(findings)
}
pub fn main(args: &[String], ctx: &Ctx, _out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let p = match cli::parse(
        "check-target-reclaim",
        "[--root ROOT]",
        args,
        &[Opt {
            name: "--root",
            value: true,
        }],
        &[],
        Some(0),
        err,
    ) {
        Ok(p) => p,
        Err(e) => return e.code(),
    };
    match check(&ctx.path(p.value("--root").unwrap_or("."))) {
        Ok(findings) => {
            for f in &findings {
                let _ = writeln!(err, "{}", crate::os_escape::display(f));
            }
            i32::from(!findings.is_empty())
        }
        Err(e) => {
            let _ = writeln!(err, "target-reclaim guard: {e}");
            2
        }
    }
}
#[cfg(test)]
#[path = "target_reclaim_tests.rs"]
mod tests;
