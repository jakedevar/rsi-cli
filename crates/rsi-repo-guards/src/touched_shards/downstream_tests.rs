//! Shared reverse-dependency compile plus exact runtime selection (#3408).
//! Code identifiers of changed public items are searched in mounted dependent
//! sources, imports and qualified paths included. A reference in a production
//! module selects that module's descendant tests; a reference inside an inline
//! test module stays there. The match over-approximates resolution and bounds
//! common names; the compile plan always remains the baseline coverage.
use super::{
    downstream::{self, Snapshot},
    fixture,
    git::Repository,
    identity::SourceScopes,
    slow, source,
    source::SourceTree,
    tree::lex_tokens,
};
use crate::re::{Re, end, start};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

pub const GENERIC_ITEMS: &[&str] = &[
    "new", "default", "from", "into", "get", "set", "len", "is_empty", "clone", "fmt",
];
pub const REFERENCE_FILE_CAP: usize = 40;

static PUBLIC_DEFINITION: LazyLock<Re> = LazyLock::new(|| {
    Re::new(
        r#"\bpub\s+(?:(?:async|unsafe|const|extern\s*(?:"[^"\n]*")?)\s+)*(?P<kind>fn|const|static|struct|enum|type)\s+(?:mut\s+)?(?P<name>[A-Za-z_]\w*)\b"#,
    )
});
static CFG_TEST: LazyLock<Re> = LazyLock::new(|| Re::new(r"#\[cfg\(\s*test\s*\)\]"));
static TEST_ATTR: LazyLock<Re> = LazyLock::new(|| Re::new(r"#\[(?:[a-z_]+::)?test\b"));
static IGNORE_ATTR: LazyLock<Re> = LazyLock::new(|| Re::new(r"#\[ignore\b"));

type Definitions = BTreeMap<String, Vec<Vec<String>>>;

/// Public definition names and executable tokens, bodies included. Delimiters
/// are balanced over code (literals/comments blanked) so a private neighbour
/// never seeds observers; restricted visibility is not a workspace API and test
/// definitions stay excluded.
pub fn public_item_definitions(
    text: &str,
    host: &str,
) -> Result<Vec<(String, Vec<String>)>, String> {
    let code = source::code_text(text)?;
    let scopes = SourceScopes::new("", text)?;
    let rows: Vec<_> = text.lines().collect();
    let bytes = code.as_bytes();
    let mut found = Vec::new();
    for m in PUBLIC_DEFINITION.find_all(&code) {
        let first = code[..start(&m)].matches('\n').count() + 1;
        let module = scopes
            .lines
            .get(first - 1)
            .map_or(String::new(), |s| s.module.clone());
        if module.split("::").any(source::is_test_module)
            || slow::only_test_lines(text, &BTreeSet::from([first]), host)
        {
            continue;
        }
        let attr = source::attrs_above(&rows, first - 1).join("\n");
        if CFG_TEST.is_match(&attr) {
            continue;
        }
        let (mut depth, mut opened, mut finish) = (0i32, false, code.len());
        for cursor in end(&m)..bytes.len() {
            match bytes[cursor] {
                b'(' | b'[' | b'{' => {
                    depth += 1;
                    if bytes[cursor] == b'{' && depth == 1 {
                        opened = true;
                    }
                }
                b')' | b']' | b'}' => {
                    depth -= 1;
                    if bytes[cursor] == b'}' && depth == 0 && opened {
                        finish = cursor + 1;
                        break;
                    }
                }
                b';' if depth == 0 => {
                    finish = cursor + 1;
                    break;
                }
                _ => (),
            }
        }
        let definition = &text[start(&m)..finish];
        let mut tokens = Vec::new();
        let mut previous = 0;
        for lex in source::rust_lexemes(definition)? {
            tokens.extend(lex_tokens(&definition[previous..lex.start]));
            if lex.kind == source::LexemeKind::Literal {
                tokens.push(definition[lex.start..lex.end].to_owned());
            }
            previous = lex.end;
        }
        tokens.extend(lex_tokens(&definition[previous..]));
        found.push((m.name("name").map_or("", |n| n.as_str()).to_owned(), tokens));
    }
    Ok(found)
}

struct Target {
    kind: String,
    target: Option<String>,
    references: Vec<((String, String), BTreeSet<String>)>,
    tests: Vec<(String, String, Vec<String>)>,
}

/// The receipt object: compile plan plus items, selections, filters and notes.
pub fn downstream_test_plan(
    repo: &Repository,
    base: &str,
    head: &str,
    sources: &SourceTree,
) -> Result<Value, String> {
    let host = &sources.host;
    let diff = repo
        .text(&[
            "diff",
            "--name-only",
            "--no-renames",
            &format!("{base}..{head}"),
        ])
        .map_err(|e| e.to_string())?;
    let changed: Vec<String> = diff.lines().map(str::to_owned).collect();
    let has_rust = changed.iter().any(|p| p.ends_with(".rs"));
    let snapshots = if has_rust {
        Some([
            downstream::snapshot(repo, base)?,
            downstream::snapshot(repo, head)?,
        ])
    } else {
        None
    };
    let empty = |files: BTreeSet<String>| Snapshot {
        files,
        texts: Default::default(),
        packages: Default::default(),
    };
    let plan = match &snapshots {
        Some(pair) => downstream::compile_plan_with(repo, pair, base, head, &changed, host)?,
        None => downstream::compile_plan_with(
            repo,
            &[empty(BTreeSet::new()), empty(BTreeSet::new())],
            base,
            head,
            &changed,
            host,
        )?,
    };
    let result = serde_json::to_value(&plan).map_err(|e| e.to_string())?;
    let (mut items_out, mut selections, mut filters_out, mut notes) =
        (Vec::new(), Vec::new(), Vec::<String>::new(), Vec::new());
    let finish = |mut result: Value,
                  items: Vec<Value>,
                  selections: Vec<Value>,
                  filters: Vec<String>,
                  notes: Vec<Value>| {
        let object = result.as_object_mut().expect("plan object");
        object.insert("items".into(), Value::Array(items));
        object.insert("selections".into(), Value::Array(selections));
        let filters: BTreeSet<_> = filters.into_iter().collect();
        object.insert("filters".into(), json!(filters));
        object.insert("notes".into(), Value::Array(notes));
        result
    };
    let Some(snapshots) = snapshots else {
        return Ok(finish(result, items_out, selections, filters_out, notes));
    };
    if plan.changed_libraries.is_empty() {
        return Ok(finish(result, items_out, selections, filters_out, notes));
    }
    let mut edges: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for snap in &snapshots {
        for (name, package) in &snap.packages {
            edges
                .entry(name.clone())
                .or_default()
                .extend(package.dependencies.clone());
        }
    }
    let mut items: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for changed_library in &plan.changed_libraries {
        let mut definitions: Vec<BTreeMap<String, Definitions>> = Vec::new();
        for snap in &snapshots {
            let mut found: BTreeMap<String, Definitions> = BTreeMap::new();
            if let Some(package) = snap.packages.get(&changed_library.package) {
                if let Some(library) = &package.library {
                    let mounted = source::mount_closure(&snap.texts, library, host).modules;
                    for path in &changed_library.paths {
                        let Some(module) = mounted.get(path) else {
                            continue;
                        };
                        if module.split("::").any(source::is_test_module) {
                            continue;
                        }
                        let entry = found.entry(path.clone()).or_default();
                        let text = snap.texts.get(path).map_or("", String::as_str);
                        for (item, tokens) in public_item_definitions(text, host)? {
                            entry.entry(item).or_default().push(tokens);
                        }
                    }
                }
            }
            definitions.push(found);
        }
        for path in &changed_library.paths {
            let empty = Definitions::new();
            let before = definitions[0].get(path).unwrap_or(&empty);
            let after = definitions[1].get(path).unwrap_or(&empty);
            let names: BTreeSet<_> = before.keys().chain(after.keys()).collect();
            for item in names {
                if before.get(item) != after.get(item) {
                    items
                        .entry((changed_library.package.clone(), item.clone()))
                        .or_default()
                        .insert(path.clone());
                }
            }
        }
    }
    for ((library, item), paths) in &items {
        items_out.push(json!({"package": library, "item": item, "paths": paths}));
        if GENERIC_ITEMS.contains(&item.as_str()) || item.chars().count() < 4 {
            notes.push(json!({"package": library, "item": item, "reason": "generic identifier; compile coverage retained"}));
        }
    }
    items.retain(|(_, item), _| {
        !GENERIC_ITEMS.contains(&item.as_str()) && item.chars().count() >= 4
    });
    if items.is_empty() {
        return Ok(finish(result, items_out, selections, filters_out, notes));
    }
    let head_snapshot = &snapshots[1];
    let targets = sources.targets();
    // Inventory real targets, mounts, inline module identities and runnable
    // tests once per dependent, independently of the number of changed items.
    let mut inventory: BTreeMap<String, Vec<Target>> = BTreeMap::new();
    for entry in &plan.packages {
        let name = &entry.package;
        let package = &head_snapshot.packages[name];
        let mut roots: Vec<(String, Option<String>, String)> = Vec::new();
        if let Some(library) = &package.library {
            roots.push(("lib".into(), None, library.clone()));
        }
        roots.extend(
            targets
                .iter()
                .filter(|t| &t.package == name)
                .map(|t| (t.kind.clone(), Some(t.name.clone()), t.root.clone())),
        );
        let sharded = name == "rsid" || name == "rsid-store";
        let mut list = Vec::new();
        for (kind, target, root) in roots {
            let mounts = source::mount_closure(&sources.texts, &root, host);
            let mut references: Vec<((String, String), BTreeSet<String>)> = Vec::new();
            let mut positions: BTreeMap<(String, String), usize> = BTreeMap::new();
            let mut tests = Vec::new();
            for (path, module) in &mounts.modules {
                let text = sources.texts.get(path).map_or("", String::as_str);
                if mounts.gated_off.contains(path) || source::file_gated_off(text, host) {
                    continue;
                }
                let code = source::code_text(text)?;
                let scopes = SourceScopes::new(module, text)?;
                for (line, row) in code.lines().enumerate() {
                    let owner = scopes
                        .lines
                        .get(line)
                        .map_or(String::new(), |s| s.module.clone());
                    let key = (path.clone(), owner);
                    let at = *positions.entry(key.clone()).or_insert_with(|| {
                        references.push((key, BTreeSet::new()));
                        references.len() - 1
                    });
                    references[at].1.extend(fixture::words(row));
                }
                for f in source::rust_functions(text, host) {
                    if !TEST_ATTR.is_match(&f.attrs) || IGNORE_ATTR.is_match(&f.attrs) {
                        continue;
                    }
                    let owner = scopes
                        .lines
                        .get(f.start)
                        .map_or(String::new(), |s| s.module.clone());
                    let full = [owner.as_str(), f.name.as_str()]
                        .into_iter()
                        .filter(|p| !p.is_empty())
                        .collect::<Vec<_>>()
                        .join("::");
                    let shards: Vec<String> = if kind == "lib" && sharded {
                        source::code_shards(&f.attrs)?.into_iter().collect()
                    } else {
                        Vec::new()
                    };
                    // Sharded library tests must retain their own gate; do not
                    // invent a shard for a test the gate cannot build.
                    if kind == "lib" && sharded && shards.is_empty() {
                        continue;
                    }
                    tests.push((owner, full, shards));
                }
            }
            list.push(Target {
                kind,
                target,
                references,
                tests,
            });
        }
        inventory.insert(name.clone(), list);
    }
    for (library, item) in items.keys() {
        let dependents = downstream::reverse_closure(&edges, &BTreeSet::from([library.clone()]));
        let mut matches = Vec::new();
        for name in dependents.iter().filter(|n| inventory.contains_key(*n)) {
            for target in &inventory[name] {
                for ((path, module), words) in &target.references {
                    if words.contains(item) {
                        matches.push((name, target, path, module));
                    }
                }
            }
        }
        let files: BTreeSet<_> = matches.iter().map(|m| m.2).collect();
        if files.len() > REFERENCE_FILE_CAP {
            notes.push(json!({"package": library, "item": item,
                "reason": "generic identifier: reference file cap exceeded; compile coverage retained",
                "cap": REFERENCE_FILE_CAP}));
            continue;
        }
        for (name, target, path, module) in matches {
            let selected: Vec<_> = target
                .tests
                .iter()
                .filter(|(owner, _, _)| {
                    module.is_empty()
                        || owner == module
                        || owner.starts_with(&format!("{module}::"))
                })
                .collect();
            let mut filters = Vec::new();
            for (_, full, shards) in &selected {
                let prefixes: Vec<String> = if shards.is_empty() {
                    vec![if target.kind == "lib" {
                        format!("{name}=")
                    } else {
                        format!(
                            "{name}={}:{}:",
                            target.kind,
                            target.target.as_deref().unwrap_or("")
                        )
                    }]
                } else {
                    shards.iter().map(|s| format!("rsid=shard:{s}:")).collect()
                };
                filters.extend(prefixes.into_iter().map(|p| format!("{p}test(={full})")));
            }
            let tests: BTreeSet<_> = selected.iter().map(|(_, full, _)| full.clone()).collect();
            let unique: BTreeSet<_> = filters.iter().cloned().collect();
            selections.push(json!({
                "library": library, "item": item, "package": name,
                "path": path, "module": module, "kind": target.kind,
                "target": target.target, "tests": tests, "filters": unique,
            }));
            filters_out.extend(filters);
        }
    }
    Ok(finish(result, items_out, selections, filters_out, notes))
}

#[test]
fn a_huge_star_member_pattern_neither_overflows_the_stack_nor_changes_the_match() {
    // Run on a small stack so unbounded recursion fails here, not on a lucky host.
    let outcome = std::thread::Builder::new()
        .stack_size(512 * 1024)
        .spawn(|| {
            let stars = "*".repeat(100_000);
            let texts = BTreeMap::from([
                (
                    "Cargo.toml".to_owned(),
                    format!("[workspace]\nmembers = [\"{stars}/x\"]\n"),
                ),
                (
                    "crates/x/Cargo.toml".to_owned(),
                    "[package]\nname = \"x\"\nversion = \"0.1.0\"\n".to_owned(),
                ),
                (
                    "crates/y/Cargo.toml".to_owned(),
                    "[package]\nname = \"y\"\nversion = \"0.1.0\"\n".to_owned(),
                ),
            ]);
            downstream::workspace_test_graph(&texts, &BTreeSet::new())
        })
        .unwrap()
        .join()
        .expect("the glob matcher must not overflow the stack");
    let graph = outcome.expect("a collapsible star run is a valid pattern");
    assert_eq!(graph.keys().collect::<Vec<_>>(), ["x"]);
}

#[test]
fn a_pattern_beyond_the_matcher_budget_is_refused_not_matched_recursively() {
    let stars = "*?".repeat(2_000_000);
    let texts = BTreeMap::from([
        (
            "Cargo.toml".to_owned(),
            format!("[workspace]\nmembers = [\"{stars}\"]\n"),
        ),
        (
            "crates/x/Cargo.toml".to_owned(),
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\n".to_owned(),
        ),
    ]);
    let refusal = downstream::workspace_test_graph(&texts, &BTreeSet::new())
        .expect_err("an oversized member pattern must be refused");
    assert!(refusal.contains("pattern"), "{refusal}");
}

#[cfg(test)]
fn members_workspace(pattern: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            "Cargo.toml".to_owned(),
            format!("[workspace]\nmembers = [\"{pattern}\"]\n"),
        ),
        (
            "crates/x/Cargo.toml".to_owned(),
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\n".to_owned(),
        ),
    ])
}

#[test]
fn a_long_run_of_unmatched_brackets_is_compiled_in_linear_time() {
    // A pattern of 1,000,000 unmatched `[` costs ~5e11 byte comparisons when each one
    // rescans the rest of the pattern; compiled linearly it is one pass. The match
    // itself fits the budget (1,000,000 literals x 9 cells), so the result is a plain
    // "no member matches" rather than a refusal.
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let texts = members_workspace(&"[".repeat(1_000_000));
        let _ = sender.send(downstream::workspace_test_graph(&texts, &BTreeSet::new()));
    });
    let outcome = receiver
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("compiling a long run of unmatched `[` must be linear in the pattern length");
    let graph = outcome.expect("unmatched brackets are literals, not an error");
    assert!(graph.is_empty(), "{:?}", graph.keys().collect::<Vec<_>>());
}

#[test]
fn a_matched_bracket_still_ends_at_its_first_close_and_later_ones_stay_literal() {
    let matched = |pattern: &str| {
        downstream::workspace_test_graph(&members_workspace(pattern), &BTreeSet::new())
            .map(|g| g.contains_key("x"))
    };
    assert_eq!(matched("crates/[xy]"), Ok(true));
    assert_eq!(matched("crates/[!y]"), Ok(true));
    assert_eq!(matched("crates/[a-z]"), Ok(true));
    assert_eq!(matched("crates/[!a-z]"), Ok(false));
    assert_eq!(matched("crates/[xy]["), Ok(false));
    // the second `[` is just a member of the class
    assert_eq!(matched("crates/[[x]"), Ok(true));
    assert_eq!(matched("crates/x["), Ok(false));
    assert_eq!(matched("[[[crates/x"), Ok(false));
    assert_eq!(matched("*[*"), Ok(false));
    assert_eq!(matched("crates/[x]]"), Ok(false));
}

#[test]
fn a_huge_character_class_counts_toward_the_matcher_budget() {
    // One class element, but `class_hit` reads every one of its bytes for each of the
    // path's bytes: 3,000,000 x 9 cells is far past the 16M-cell budget.
    let class = format!("[{}]", "a".repeat(3_000_000));
    let refusal = downstream::workspace_test_graph(&members_workspace(&class), &BTreeSet::new())
        .expect_err("a class whose scan alone exceeds the budget must be refused");
    assert!(refusal.contains("pattern"), "{refusal}");
}
