//! #1280: the tests an unfiltered `rsid`/`rsid-store`/`rsid-remote-read`/`rsi-graph` change can observe.
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
//! An item-level `thread_local!` (or `task_local!`, `lazy_static!`) declares exactly the
//! statics its body writes, so it is an ordinary item defining those names
//! (#2086); the reason of any other opaque macro names the macro.
//!
//! Anything the names cannot prove runs every library test of both packages
//! (`Everything`): an unmounted source (migrations), an item-level macro
//! invocation the closure reaches, an undeterminable impl self type, a
//! multiply mounted library source, or a closure that finds no test. A large
//! closure selects the nearest [`MAX_SELECTED_TESTS`] tests, always including
//! direct observers and the complete nearest distance tier (#2092). Reached
//! bin and integration test targets are ranked by their nearest affected item:
//! owned, mounted, reading and directly calling targets always run, and the
//! nearest others fill [`MAX_SELECTED_TARGETS`] (#2094).
//! Found nothing never means run nothing.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

/// The packages whose sources the closure reads. `rsid` and `rsid-store` share
/// one library test build; [`LEAF_PACKAGES`] run theirs in their own package run.
pub(crate) const LIB_PACKAGES: [&str; 5] = [
    "rsid",
    "rsid-store",
    "rsid-remote-read",
    "rsid-harness",
    "rsi-graph",
];

/// Leaf crates split out of rsid (#2410). A name-closure edge runs from a leaf
/// item to the `rsid` items that name it, never the reverse (a leaf depends on
/// `rsi-common` only). The gate runs every library test of a changed leaf in
/// its own package run (`cargo test -p <leaf> --lib`), so the shared rsid lib
/// build is given only the daemon-side observers. `rsi-graph` (#2416) joins
/// them for its `planning` module (#2415, #2308 cut 2), whose pure planners the
/// daemon's topology executor and graph runner call through rsid facades: an
/// edit there selects the rsid tests that name the changed items, not just
/// `rsi-graph`'s own tests. It too depends on `rsi-common` only.
pub(crate) const LEAF_PACKAGES: [&str; 3] = ["rsid-remote-read", "rsid-harness", "rsi-graph"];

/// Nearest-first gate budget (#2092). Direct observers and the complete nearest
/// distance tier are mandatory, even when they exceed this soft bound. The
/// queue runs the full libraries asynchronously after a bounded landing.
pub(crate) const MAX_SELECTED_TESTS: usize = 400;

/// Nearest-first budget of the bin and integration test targets the closure
/// reaches only through other code (#2094). Targets at distance 0 or 1 (owned,
/// mounted, read or direct callers) are mandatory beyond it; the queue's async
/// full run covers the omitted targets.
pub(crate) const MAX_SELECTED_TARGETS: usize = 5;

/// Item-level macros of the standard library and its well-known companions
/// whose expansion is exactly the declarations written in their body
/// (`thread_local! { static NAME: T = ..; }`), so the names a body declares
/// are the names the invocation defines (#2086). Any other macro that this
/// crate does not define stays opaque.
const STD_ITEM_MACROS: &[&str] = &["thread_local", "task_local", "lazy_static"];

const SCANNED_TOKENS: &str = "scripts/scanned-tokens.json";

#[derive(serde::Deserialize)]
struct ScannedTokens {
    version: u32,
    tokens: BTreeMap<String, TokenScanner>,
}

#[derive(serde::Deserialize)]
struct TokenScanner {
    packages: Vec<String>,
    tests: Vec<ScannerTest>,
}

#[derive(serde::Deserialize)]
struct ScannerTest {
    package: String,
    name: String,
}

/// Compiler built-in macros: they expand to no item and name no item of this
/// crate, so an invocation that merely uses them is not reached by name.
const BUILTIN_MACROS: &[&str] = &[
    "include",
    "concat",
    "env",
    "option_env",
    "stringify",
    "file",
    "line",
    "column",
    "module_path",
    "cfg",
];

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
    /// Number of library tests reached before applying the gate budget.
    pub(crate) reached_tests: usize,
    /// `(package, "bin:NAME" | "test:NAME")`: the binary and integration test
    /// targets the gate runs: every target that owns, mounts or reads changed
    /// code or names it directly, and the nearest others within
    /// [`MAX_SELECTED_TARGETS`] (#2094).
    pub(crate) targets: BTreeSet<(String, String)>,
    /// Number of targets reached before applying the target budget.
    pub(crate) reached_targets: usize,
    /// The names in `lib` that only a [`LEAF_PACKAGES`] crate defines: the leaf's
    /// own package run covers them, so the shared rsid lib build leaves them out.
    pub(crate) leaf_tests: BTreeSet<String>,
}

/// The distances behind a selection, for the read-only `--explain-selection`
/// report. Empty when the selector chose `everything` or reached nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Explanation {
    /// The soft test and target bounds the selector ran under.
    pub(crate) test_bound: usize,
    pub(crate) target_bound: usize,
    /// Every reached library test (full name) with its call distance.
    pub(crate) test_distances: BTreeMap<String, usize>,
    /// Every reached target with its distance (0 = owns, mounts or reads
    /// changed code).
    pub(crate) target_distances: BTreeMap<(String, String), usize>,
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

/// How many of the reached library tests and targets a selection keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Budget {
    tests: usize,
    targets: usize,
}

impl Budget {
    /// The gate's nearest-first budget (#2092, #2094).
    pub(crate) const GATE: Self = Self {
        tests: MAX_SELECTED_TESTS,
        targets: MAX_SELECTED_TARGETS,
    };
    /// Every reached test and target: what a change can observe, not what a
    /// gate can afford to run (#1957).
    pub(crate) const WHOLE: Self = Self {
        tests: usize::MAX,
        targets: usize::MAX,
    };
}

/// The selection for `candidate` against `base`, read from `repo`. `None`
/// means every shard: a crate root, manifest or build script changed, or the
/// diff or a tree could not be read.
pub(crate) fn select(repo: &Path, base: &str, candidate: &str) -> Option<UnfilteredSelection> {
    select_within(repo, base, candidate, Budget::GATE)
}

/// [`select`] under an explicit [`Budget`].
pub(crate) fn select_within(
    repo: &Path,
    base: &str,
    candidate: &str,
    budget: Budget,
) -> Option<UnfilteredSelection> {
    select_explained(repo, base, candidate, budget).map(|(selection, _)| selection)
}

/// [`select_within`] plus the distances it ranked (read-only; same selection).
pub(crate) fn select_explained(
    repo: &Path,
    base: &str,
    candidate: &str,
    budget: Budget,
) -> Option<(UnfilteredSelection, Explanation)> {
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
    args.extend([
        SCANNED_TOKENS,
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain",
        "rust-toolchain.toml",
    ]);
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
        return Some((
            UnfilteredSelection {
                lib: LibTests::Tests(BTreeSet::new()),
                reached_tests: 0,
                targets: BTreeSet::new(),
                reached_targets: 0,
                leaf_tests: BTreeSet::new(),
            },
            Explanation::default(),
        ));
    }
    let candidate_tree = load_tree(repo, candidate).ok()?;
    let base_tree = load_tree(repo, base).ok()?;
    select_from_explained(&diffs, &base_tree, &candidate_tree, budget)
}

/// The selection for parsed diffs over the two trees (see [`select`]).
#[cfg(test)]
pub(crate) fn select_from(
    diffs: &[FileDiff],
    base: &Tree,
    candidate: &Tree,
) -> Option<UnfilteredSelection> {
    select_from_within(diffs, base, candidate, Budget::GATE)
}

/// [`select_from`] under an explicit [`Budget`].
#[cfg(test)]
pub(crate) fn select_from_within(
    diffs: &[FileDiff],
    base: &Tree,
    candidate: &Tree,
    budget: Budget,
) -> Option<UnfilteredSelection> {
    select_from_explained(diffs, base, candidate, budget).map(|(selection, _)| selection)
}

/// [`select_from_within`] plus the ranked distances; the one implementation
/// of the selector, so the explanation cannot drift from the real choice.
pub(crate) fn select_from_explained(
    diffs: &[FileDiff],
    base: &Tree,
    candidate: &Tree,
    budget: Budget,
) -> Option<(UnfilteredSelection, Explanation)> {
    let mut explanation = Explanation {
        test_bound: budget.tests,
        target_bound: budget.targets,
        ..Explanation::default()
    };
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
            if matches!(
                path,
                "Cargo.toml" | "Cargo.lock" | "rust-toolchain" | "rust-toolchain.toml"
            ) {
                return None;
            }
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
    let mut changed_texts: BTreeSet<String> = BTreeSet::new();
    // Source reads hidden behind workspace-walking helpers have no ordinary
    // name edge. The same candidate registry feeds scoped-test/queue filters.
    match candidate_program.scanned_token_seeds(diffs, base, candidate, &base_program) {
        Ok(registered) => {
            lib_changed |= !registered.is_empty();
            seeds.extend(registered);
        }
        Err(reason) => {
            lib_changed = true;
            everything = Some(reason);
        }
    }
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
        let (_, rest) = split_package(path).expect("filtered above");
        // A source scanner observes every change under either crate's src/.
        if rest.starts_with("src/") && !scanned {
            scanned = true;
            if !candidate_program.source_readers.is_empty() {
                lib_changed = true;
                seeds.extend(candidate_program.source_readers.iter().copied());
            }
        }
        // Binary and integration tests can scan source trees too. Keep them
        // as direct observers so both the scoped-test and lander selectors
        // choose the same target (#2165).
        if rest.starts_with("src/") {
            for (package, filter, crates) in &candidate_program.target_source_readers {
                if crates.contains(split_package(path).expect("filtered above").0) {
                    targets.insert((package.clone(), filter.clone()));
                }
            }
        }
        // A binary or integration test target that owns the file runs its
        // own tests.
        for owner in program.owners.get(path).into_iter().flatten() {
            if let Some(target) = program.runnable_targets.get(owner) {
                targets.insert((owner.package().to_owned(), target.clone()));
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
                            &mut changed_texts,
                        );
                    }
                    Reader::Target(owner) => {
                        if let Some(target) = program.runnable_targets.get(&owner) {
                            targets.insert((owner.package().to_owned(), target.clone()));
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
                        &mut changed_texts,
                    );
                }
                Reader::Target(owner) => {
                    if let Some(target) = program.runnable_targets.get(&owner) {
                        targets.insert((owner.package().to_owned(), target.clone()));
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
                        &mut changed_texts,
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
                    &mut changed_texts,
                );
            }
        }
    }
    // #1952: a test that asserts prose a changed production item writes
    // observes the edit without naming the item (a launch brief, a refusal
    // message): it is a direct observer.
    let observers = candidate_program.text_observers(&changed_texts);
    if !observers.is_empty() {
        lib_changed = true;
        seeds.extend(observers);
    }
    // A path-only library index cannot prove reachability through every
    // module/owner of a reused source (including its descendants). Check
    // both trees: a base-side key otherwise carries only the retained mount.
    if lib_changed {
        everything = everything
            .or_else(|| base_program.mount_error.clone())
            .or_else(|| candidate_program.mount_error.clone());
    }
    let mut reached_tests = 0;
    let mut reached_targets = targets.len();
    let mut leaf_tests = BTreeSet::new();
    let lib = if let Some(reason) = everything {
        LibTests::Everything(reason)
    } else if !lib_changed {
        LibTests::Tests(BTreeSet::new())
    } else {
        match candidate_program.closure(&seeds, seed_keys) {
            Err(reason) => LibTests::Everything(reason),
            Ok(reached) => {
                // #1306: a bin or integration test target that names the
                // changed code runs with it; one reached only through
                // daemon hubs competes for the target budget (#2094).
                let mut ranked: BTreeMap<(String, String), usize> =
                    targets.iter().map(|target| (target.clone(), 0)).collect();
                for (target, distance) in &reached.targets {
                    ranked.entry(target.clone()).or_insert(*distance);
                }
                reached_targets = ranked.len();
                explanation.target_distances = ranked.clone();
                explanation.test_distances = reached.tests.clone();
                targets = nearest_targets(&ranked, budget.targets);
                reached_tests = reached.tests.len();
                let tests = nearest_tests(&reached.tests, budget.tests);
                leaf_tests = tests
                    .iter()
                    .filter(|name| reached.leaf_only(name))
                    .cloned()
                    .collect();
                if tests.is_empty() && targets.is_empty() {
                    LibTests::Everything(
                        "the changed library code names no test that reaches it".into(),
                    )
                } else {
                    LibTests::Tests(tests)
                }
            }
        }
    };
    Some((
        UnfilteredSelection {
            lib,
            reached_tests,
            targets,
            reached_targets,
            leaf_tests,
        },
        explanation,
    ))
}

/// Render the read-only `--explain-selection` report: the library and target
/// distances, which entries are mandatory, the bounds and the cut.
pub(crate) fn render_explanation(selection: Option<&(UnfilteredSelection, Explanation)>) -> String {
    let Some((selection, explanation)) = selection else {
        return "selection=all_shards reason=crate root, manifest, build script or unreadable diff\n"
            .to_string();
    };
    let mut out = format!("{}\n", selection.evidence());
    out.push_str(&format!(
        "max_selected_tests={} max_selected_targets={}\n",
        explanation.test_bound, explanation.target_bound
    ));
    let minimum = explanation.test_distances.values().min().copied();
    let mut tests: Vec<_> = explanation.test_distances.iter().collect();
    tests.sort_by(|a, b| a.1.cmp(b.1).then_with(|| a.0.cmp(b.0)));
    let selected_tests = match &selection.lib {
        LibTests::Tests(tests) => Some(tests),
        LibTests::Everything(_) => None,
    };
    for (name, distance) in tests {
        let mandatory = *distance <= 1 || Some(*distance) == minimum;
        let selected = selected_tests.is_some_and(|set| set.contains(name));
        out.push_str(&format!(
            "test distance={distance} mandatory={mandatory} selected={selected} {name}\n"
        ));
    }
    let mut targets: Vec<_> = explanation.target_distances.iter().collect();
    targets.sort_by(|a, b| a.1.cmp(b.1).then_with(|| a.0.cmp(b.0)));
    for ((package, target), distance) in targets {
        let mandatory = *distance <= 1;
        let selected = selection
            .targets
            .contains(&(package.clone(), target.clone()));
        out.push_str(&format!(
            "target distance={distance} mandatory={mandatory} selected={selected} {package} {target}\n"
        ));
    }
    out
}

impl UnfilteredSelection {
    /// Stable fields carried by both the lander log and the queue outcome.
    /// A bounded target set appends `targets=bounded selected=N reached=M`,
    /// which also obliges the queue's async full run (#2094).
    pub(crate) fn evidence(&self) -> String {
        let library = match &self.lib {
            LibTests::Everything(reason) => format!("selection=everything reason={reason}"),
            LibTests::Tests(tests) => format!(
                "selection={} selected={} reached={}",
                if tests.len() < self.reached_tests {
                    "bounded"
                } else {
                    "closure"
                },
                tests.len(),
                self.reached_tests,
            ),
        };
        if self.targets_bounded() {
            format!(
                "{library} targets=bounded selected={} reached={}",
                self.targets.len(),
                self.reached_targets
            )
        } else {
            library
        }
    }

    /// The gate omitted reached targets, leaving them to the async full run.
    pub(crate) fn targets_bounded(&self) -> bool {
        self.targets.len() < self.reached_targets
    }
}

/// Distance first, `(package, target)` second. Targets at distance 0 or 1 (a
/// changed or reading target, a direct caller) always run; the budget only
/// trims targets reached through other code (#2094).
fn nearest_targets(
    targets: &BTreeMap<(String, String), usize>,
    bound: usize,
) -> BTreeSet<(String, String)> {
    let mut ranked: Vec<_> = targets.iter().collect();
    ranked.sort_by(|(a_target, a_distance), (b_target, b_distance)| {
        a_distance
            .cmp(b_distance)
            .then_with(|| a_target.cmp(b_target))
    });
    let mandatory = ranked
        .iter()
        .filter(|(_, distance)| **distance <= 1)
        .count();
    ranked
        .into_iter()
        .take(bound.max(mandatory))
        .map(|(target, _)| target.clone())
        .collect()
}

/// Call-graph distance first, full name second. Keeping every direct observer
/// (distance 0 or 1) and the entire nearest tier takes precedence over the
/// budget: truncating that tier would drop equally strong evidence.
fn nearest_tests(tests: &BTreeMap<String, usize>, bound: usize) -> BTreeSet<String> {
    let Some(minimum) = tests.values().min().copied() else {
        return BTreeSet::new();
    };
    let mut ranked: Vec<_> = tests.iter().collect();
    ranked.sort_by(|(a_name, a_distance), (b_name, b_distance)| {
        a_distance.cmp(b_distance).then_with(|| a_name.cmp(b_name))
    });
    let mandatory = ranked
        .iter()
        .filter(|(_, distance)| **distance <= 1 || **distance == minimum)
        .count();
    ranked
        .into_iter()
        .take(bound.max(mandatory))
        .map(|(name, _)| name.clone())
        .collect()
}

/// `git` run in `repo` with the repository-selecting environment removed,
/// like the lander's other git calls: an inherited `GIT_DIR` must not turn a
/// diff into `--no-index` mode.
fn repo_git(repo: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(repo)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE");
    command
}

/// What an unfiltered change to a package outside the rsid family runs beyond
/// its library tests (#1937, #1650).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PackageTargets {
    /// The package has a library target (`cargo test --lib` exists).
    pub(crate) has_lib: bool,
    /// `bin:NAME` and `test:NAME` targets that declare tests and whose own
    /// sources, mounted sources or read data changed, or every such target
    /// when the manifest or build script changed.
    pub(crate) targets: BTreeSet<String>,
    /// Production library code (or a build input) of the package changed and
    /// the package has a bin or integration target: every target must still
    /// compile, whether or not it declares a test (#1952).
    pub(crate) check: bool,
}

/// The targets to run for each of `packages` (outside the rsid family) that
/// `candidate` changes against `base`, read from `repo`. An unreadable diff or
/// tree is an error, never "nothing to run".
pub(crate) fn other_package_targets(
    repo: &Path,
    base: &str,
    candidate: &str,
    packages: &[String],
) -> Result<BTreeMap<String, PackageTargets>, String> {
    if packages.is_empty() {
        return Ok(BTreeMap::new());
    }
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
    let pathspecs: Vec<String> = packages
        .iter()
        .map(|package| format!("crates/{package}"))
        .collect();
    args.extend(pathspecs.iter().map(String::as_str));
    let output = repo_git(repo)
        .args(&args)
        .output()
        .map_err(|error| format!("git diff: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git diff {base} {candidate}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let diffs = parse_diff(&String::from_utf8_lossy(&output.stdout))
        .ok_or("cannot read the diff of the changed packages")?;
    let candidate_tree = load_tree_of(repo, candidate, packages)?;
    let base_tree = load_tree_of(repo, base, packages)?;
    Ok(other_targets_from(
        &diffs,
        &base_tree,
        &candidate_tree,
        packages,
    ))
}

/// Whether `package` declares a library target at this tree.
fn has_lib_target(tree: &Tree, package: &str) -> bool {
    let prefix = format!("crates/{package}/");
    tree.contains_key(&format!("{prefix}src/lib.rs"))
        || tree
            .get(&format!("{prefix}Cargo.toml"))
            .and_then(|text| toml::from_str::<toml::Table>(text).ok())
            .and_then(|manifest| {
                manifest
                    .get("lib")?
                    .get("path")?
                    .as_str()
                    .map(str::to_owned)
            })
            .is_some_and(|path| tree.contains_key(&normalize(&format!("{prefix}{path}"))))
}

/// [`other_package_targets`] over parsed diffs and the two trees.
///
/// A target runs when it owns, mounts or reads changed code, and (#1952) when
/// the name closure of a changed library item reaches one of its tests: the
/// same closure the rsid family uses, built over the touched packages only
/// (`targets` is consumed; the package's `--lib` run covers its library
/// tests). Anything the names cannot bound runs every runnable target of the
/// package. A package whose production library code changed and that has a
/// bin or integration target also owes a compile check of all its targets,
/// whether or not they declare a test.
pub(crate) fn other_targets_from(
    diffs: &[FileDiff],
    base: &Tree,
    candidate: &Tree,
    packages: &[String],
) -> BTreeMap<String, PackageTargets> {
    let mut found: BTreeMap<String, PackageTargets> = packages
        .iter()
        .map(|package| {
            (
                package.clone(),
                PackageTargets {
                    has_lib: has_lib_target(candidate, package),
                    ..PackageTargets::default()
                },
            )
        })
        .collect();
    let interned: Vec<&'static str> = packages.iter().map(|package| intern(package)).collect();
    let program = Program::build_for(candidate, &interned);
    let base_program = Program::build_for(base, &interned);
    let mut seeds: Vec<usize> = Vec::new();
    let mut seed_keys: Vec<Key> = Vec::new();
    let mut changed_texts: BTreeSet<String> = BTreeSet::new();
    let mut unbounded: Option<String> = None;
    // Packages whose production library code (or build inputs) changed.
    let mut production: BTreeSet<String> = BTreeSet::new();
    // Packages the names cannot bound: every runnable target runs.
    let mut whole: BTreeSet<String> = BTreeSet::new();
    for diff in diffs {
        for (path, side, lines) in [
            (diff.old_path.as_deref(), Side::Base, &diff.old_lines),
            (diff.new_path.as_deref(), Side::Candidate, &diff.new_lines),
        ] {
            let Some(path) = path else { continue };
            let Some((package, rest)) = packages.iter().find_map(|package| {
                path.strip_prefix(&format!("crates/{package}/"))
                    .map(|rest| (package, rest))
            }) else {
                continue;
            };
            // A manifest or build script can change what every target compiles.
            if matches!(rest, "Cargo.toml" | "build.rs") {
                production.insert(package.clone());
                whole.insert(package.clone());
                continue;
            }
            let (tree, side_program) = match side {
                Side::Base => (base, &base_program),
                Side::Candidate => (candidate, &program),
            };
            // A Rust change that is only comments or whitespace changes no test.
            if path.ends_with(".rs") && !diff.binary && !side_has_tokens(tree, path, lines) {
                continue;
            }
            let readers = target_readers(&program.target_texts, &program.owners, path);
            for owner in program
                .owners
                .get(path)
                .into_iter()
                .flatten()
                .chain(&readers)
            {
                if let Some(filter) = program.runnable_targets.get(owner)
                    && let Some(owner_entry) = found.get_mut(owner.package())
                {
                    owner_entry.targets.insert(filter.clone());
                }
            }
            // A binary or integration test that scans this package's sources
            // observes any change under its `src/`.
            if rest.starts_with("src/") {
                for (target_package, filter, crates) in &program.target_source_readers {
                    if crates.contains(package.as_str())
                        && let Some(target_entry) = found.get_mut(target_package)
                    {
                        target_entry.targets.insert(filter.clone());
                    }
                }
            }
            if diff.binary {
                if rest.starts_with("src/") {
                    production.insert(package.clone());
                    whole.insert(package.clone());
                }
                continue;
            }
            let mut seed = |index: usize| {
                if !side_program.items[index].in_test && side_program.items[index].target.is_none()
                {
                    production.insert(side_program.items[index].crate_name.to_owned());
                }
                push_seed(
                    side_program,
                    side,
                    index,
                    &mut seeds,
                    &mut seed_keys,
                    &mut unbounded,
                    &mut changed_texts,
                );
            };
            for reader in side_program.literal_readers(path) {
                if let Reader::Item(index) = reader {
                    seed(index);
                }
            }
            if !path.ends_with(".rs") {
                // A source-tree data file nothing reads is not bounded by names.
                if rest.starts_with("src/") && side_program.literal_readers(path).is_empty() {
                    production.insert(package.clone());
                    whole.insert(package.clone());
                }
                continue;
            }
            let Some(file) = side_program.lib_files.get(path) else {
                // A source no target mounts and no library mounts.
                if rest.starts_with("src/") && !side_program.owners.contains_key(path) {
                    production.insert(package.clone());
                    whole.insert(package.clone());
                }
                continue;
            };
            for line in lines
                .iter()
                .copied()
                .filter(|line| file.parsed.token_lines.contains(line))
            {
                let containing = side_program.items_at(path, line);
                if containing.is_empty() {
                    // Outside every item: the whole module subtree it sits in.
                    let module = file.module_at(line);
                    for index in side_program.subtree_items(file.crate_name, &module) {
                        seed(index);
                    }
                } else {
                    for index in containing {
                        seed(index);
                    }
                }
            }
        }
    }
    // A test that asserts prose a changed item writes observes the edit.
    let observers = program.text_observers(&changed_texts);
    seeds.extend(observers);
    let reason = unbounded
        .or_else(|| base_program.mount_error.clone())
        .or_else(|| program.mount_error.clone());
    if !seeds.is_empty() || !seed_keys.is_empty() {
        match reason
            .map_or_else(|| program.closure(&seeds, seed_keys), Err)
            .map(|reached| reached.targets)
        {
            Ok(reached) => {
                for (target_package, filter) in reached.into_keys() {
                    if let Some(entry) = found.get_mut(&target_package) {
                        entry.targets.insert(filter);
                    }
                }
            }
            // The names cannot bound the change: every package that changed
            // library code runs all its targets.
            Err(_) => whole.extend(production.iter().cloned()),
        }
    }
    for package in &whole {
        let Some(entry) = found.get_mut(package) else {
            continue;
        };
        entry.targets.extend(
            program
                .runnable_targets
                .iter()
                .filter(|(owner, _)| owner.package() == package.as_str())
                .map(|(_, filter)| filter.clone()),
        );
    }
    for package in &production {
        let Some(entry) = found.get_mut(package) else {
            continue;
        };
        entry.check = !package_targets(candidate, package).is_empty();
    }
    found
}

/// A `'static` package name: [`Program`] keys library items by crate name, and
/// the names of packages outside the rsid family are known only at run time.
/// Leaks one allocation per distinct name for the life of the process.
fn intern(name: &str) -> &'static str {
    static NAMES: std::sync::Mutex<BTreeSet<&'static str>> = std::sync::Mutex::new(BTreeSet::new());
    let mut names = NAMES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(known) = names.get(name) {
        return known;
    }
    let leaked: &'static str = Box::leak(name.to_owned().into_boxed_str());
    names.insert(leaked);
    leaked
}

fn push_seed(
    program: &Program,
    side: Side,
    index: usize,
    seeds: &mut Vec<usize>,
    seed_keys: &mut Vec<Key>,
    everything: &mut Option<String>,
    changed_texts: &mut BTreeSet<String>,
) {
    // The prose a production item writes is observed by the tests that
    // assert it (#1952); a changed test's own prose pins nothing else.
    if !program.items[index].in_test {
        changed_texts.extend(program.items[index].item.texts.iter().cloned());
    }
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
    load_tree_of(repo, rev, &LIB_PACKAGES)
}

/// [`load_tree`] for any packages (the targets of packages outside the rsid
/// family, #1937).
pub(crate) fn load_tree_of<S: AsRef<str>>(
    repo: &Path,
    rev: &str,
    packages: &[S],
) -> Result<Tree, String> {
    let mut args = vec!["ls-tree", "-r", "-z", "--name-only", rev, "--"];
    let pathspecs: Vec<String> = packages
        .iter()
        .map(|package| format!("crates/{}", package.as_ref()))
        .collect();
    args.extend(pathspecs.iter().map(String::as_str));
    args.push(SCANNED_TOKENS);
    let listing = repo_git(repo)
        .args(&args)
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
        .filter(|path| {
            path.ends_with(".rs") || path.ends_with("/Cargo.toml") || *path == SCANNED_TOKENS
        })
        .map(str::to_owned)
        .collect();
    let request: String = paths.iter().map(|path| format!("{rev}:{path}\n")).collect();
    let mut command = repo_git(repo);
    let mut child = command
        .args(["cat-file", "--batch"])
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

/// The shortest prose fragment that pins a text (#1952): shorter strings are
/// names, keys and punctuation that many unrelated items share.
const MIN_PROSE: usize = 16;

/// A string literal's value with the common escapes resolved, as the text a
/// test asserts and a producer writes (`\"`, `\n`, a line-continuing `\`).
fn unescape_literal(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => {}
            Some('"') => out.push('"'),
            Some('\'') => out.push('\''),
            Some('\\') => out.push('\\'),
            Some('\n') => {
                while chars.peek().is_some_and(|c| c.is_whitespace()) {
                    chars.next();
                }
            }
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// The prose fragments of a string literal: the runs of its unescaped value
/// between newlines and format braces that are at least [`MIN_PROSE`] long and
/// hold a space. A producer and a test that asserts it share a fragment.
fn prose_fragments(value: &str, into: &mut Vec<String>) {
    if value.len() < MIN_PROSE {
        return;
    }
    for fragment in unescape_literal(value).split(['\n', '{', '}']) {
        let fragment = fragment.trim();
        if fragment.len() >= MIN_PROSE
            && fragment.contains(' ')
            && !into.iter().any(|known| known == fragment)
        {
            into.push(fragment.to_owned());
        }
    }
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
    /// Prose string literals (a message or brief the item produces), as
    /// fragments between format placeholders and newlines. A test that
    /// asserts such text observes an edit to the item that writes it (#1952).
    texts: Vec<String>,
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
        texts: &mut Vec<String>,
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
                    prose_fragments(value, texts);
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
                            texts: Vec::new(),
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
                    let mut texts = Vec::new();
                    self.mentions(
                        index,
                        open.saturating_sub(1).max(index),
                        &mut mentions,
                        &mut literals,
                        &mut texts,
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
                        texts,
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
                    let mut texts = Vec::new();
                    self.mentions(index, stop, &mut mentions, &mut literals, &mut texts);
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
                        texts,
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
                    let mut texts = Vec::new();
                    self.mentions(index, stop, &mut mentions, &mut literals, &mut texts);
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
                        texts,
                        glob: false,
                        expands_to: if STD_ITEM_MACROS.contains(&path.as_str()) {
                            self.expansion_names(open, close)
                                .filter(|names| !names.is_empty())
                        } else {
                            None
                        },
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
        let mut texts = Vec::new();
        self.mentions(index, stop, &mut mentions, &mut literals, &mut texts);
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
            texts,
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
        let mut texts = Vec::new();
        self.mentions(index, stop, &mut mentions, &mut literals, &mut texts);
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
            texts,
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
    Bin(String, String),
    Test(String, String),
}

impl Owner {
    /// The package that owns the target. A target is run with its own
    /// package, which is not always the changed file's package: a bin of one
    /// crate can read a data file that lives in another.
    fn package(&self) -> &str {
        match self {
            Owner::Lib(package) => package,
            Owner::Bin(package, _) | Owner::Test(package, _) => package.as_str(),
        }
    }
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
    /// `(package, "bin:NAME" | "test:NAME")` for an item of a bin or
    /// integration test target that declares tests; `None` for a library item.
    target: Option<(String, String)>,
}

/// The crate name of every bin and integration test target item. A target's
/// private names are scoped by its own module prefix (`bin:NAME::`), never by
/// a library's.
const TARGET_CRATE: &str = "<target>";

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

/// What the name closure reaches: library tests (full libtest names) and
/// bin or integration test targets that declare an affected test, each at
/// its call-graph distance from the changed items. A target's distance is
/// that of its nearest affected item, so a target whose helper calls changed
/// code directly is a direct caller even when its tests reach it through
/// that helper (#2094).
#[derive(Default)]
struct Reached {
    tests: BTreeMap<String, usize>,
    targets: BTreeMap<(String, String), usize>,
    /// Test names defined in a [`LEAF_PACKAGES`] crate, and in any other crate.
    leaf_named: BTreeSet<String>,
    family_named: BTreeSet<String>,
}

impl Reached {
    /// Whether only a leaf crate defines the reached test `name`.
    fn leaf_only(&self, name: &str) -> bool {
        self.leaf_named.contains(name) && !self.family_named.contains(name)
    }
}

/// The items of each module of each crate, ordered so that a module's whole
/// subtree is one range (#2086): a glob key reaches every item of its module
/// subtree, and scanning every item of the program for each one made a large
/// closure take minutes.
struct ModuleIndex<'a> {
    modules: BTreeMap<(&'a str, &'a str), Vec<usize>>,
}

impl<'a> ModuleIndex<'a> {
    fn new(items: &'a [FlatItem]) -> Self {
        let mut modules: BTreeMap<(&str, &str), Vec<usize>> = BTreeMap::new();
        for (index, flat) in items.iter().enumerate() {
            modules
                .entry((flat.crate_name, flat.module.as_str()))
                .or_default()
                .push(index);
        }
        Self { modules }
    }

    /// The items of `module` and its descendants (all of the crate for the
    /// root module `""`), in item order.
    fn subtree(&self, crate_name: &str, module: &str) -> Vec<usize> {
        let mut found: Vec<usize> = Vec::new();
        if module.is_empty() {
            for (_, indexes) in self
                .modules
                .range((crate_name, "")..)
                .take_while(|((name, _), _)| *name == crate_name)
            {
                found.extend(indexes);
            }
        } else {
            if let Some(indexes) = self.modules.get(&(crate_name, module)) {
                found.extend(indexes);
            }
            // Every descendant starts with `module::`, which sorts inside
            // `["module::", "module:;")`.
            let from = format!("{module}::");
            let to = format!("{module}:;");
            for (_, indexes) in self
                .modules
                .range((crate_name, from.as_str())..(crate_name, to.as_str()))
            {
                found.extend(indexes);
            }
        }
        found.sort_unstable();
        found
    }
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
    target_source_readers: Vec<(String, String, BTreeSet<String>)>,
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
fn package_targets(tree: &Tree, package: &str) -> Vec<(Owner, String, bool)> {
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
                Owner::Bin(package.to_owned(), name.to_owned())
            } else {
                Owner::Test(package.to_owned(), name.to_owned())
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
            ["src", "main.rs"] => Some(Owner::Bin(package.to_owned(), package.to_owned())),
            ["src", "bin", file] if file.ends_with(".rs") => Some(Owner::Bin(
                package.to_owned(),
                file.trim_end_matches(".rs").to_owned(),
            )),
            ["src", "bin", dir, "main.rs"] => {
                Some(Owner::Bin(package.to_owned(), (*dir).to_owned()))
            }
            ["tests", file] if file.ends_with(".rs") => Some(Owner::Test(
                package.to_owned(),
                file.trim_end_matches(".rs").to_owned(),
            )),
            ["tests", dir, "main.rs"] => Some(Owner::Test(package.to_owned(), (*dir).to_owned())),
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

/// Whether a literal read by `from` names `path`: a Rust source by an
/// `include_str!`/`include_bytes!` that resolves to it (a path literal
/// elsewhere is test data), a data file by any literal that is its file name
/// or a path ending in it, since it is read at run time.
fn literal_names_path(path: &str, from: &str, literal: &str) -> bool {
    if path.ends_with(".rs") {
        literal
            .strip_prefix(INCLUDED)
            .is_some_and(|included| normalize(&format!("{}/{included}", dirname(from))) == path)
    } else {
        let file_name = path.rsplit('/').next().unwrap_or(path);
        literal == file_name || literal.ends_with(&format!("/{file_name}"))
    }
}

/// The binary and integration test targets whose files read `path` by a
/// literal (see [`literal_names_path`]).
fn target_readers(
    texts: &BTreeMap<String, String>,
    owners: &BTreeMap<String, BTreeSet<Owner>>,
    path: &str,
) -> Vec<Owner> {
    let mut readers = Vec::new();
    for (target_path, text) in texts {
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
                literal_names_path(path, target_path, &format!("{INCLUDED}{value}"))
            } else {
                literal_names_path(path, target_path, value)
            }
        });
        if mentions {
            for owner in owners.get(target_path).into_iter().flatten() {
                if !matches!(owner, Owner::Lib(_)) {
                    readers.push(owner.clone());
                }
            }
        }
    }
    readers
}

/// One file mounted into a runnable target.
struct TargetFile {
    package: String,
    filter: String,
    path: String,
    mounted: Mounted,
}

/// Indexes the binary and integration test targets of one package: the
/// targets that own every file they mount, the targets that declare tests and
/// need no feature (`bin:NAME`/`test:NAME`), and the text of each mounted file.
/// Returns the mounted files of every runnable target.
fn index_targets(
    tree: &Tree,
    package: &str,
    owners: &mut BTreeMap<String, BTreeSet<Owner>>,
    runnable: &mut BTreeMap<Owner, String>,
    texts: &mut BTreeMap<String, String>,
) -> Vec<TargetFile> {
    let mut target_files = Vec::new();
    for (owner, root, gated) in package_targets(tree, package) {
        let mut declares_tests = false;
        let (files, _) = mount(tree, &root);
        let mut mounted_files = Vec::new();
        for (path, mounted) in files {
            declares_tests |= mounted.parsed.items.iter().any(|item| item.test);
            if let Some(text) = tree.get(&path) {
                texts.insert(path.clone(), text.clone());
            }
            owners
                .entry(path.clone())
                .or_default()
                .insert(owner.clone());
            mounted_files.push((path, mounted));
        }
        if declares_tests && !gated {
            let filter = match &owner {
                Owner::Bin(_, name) => format!("bin:{name}"),
                Owner::Test(_, name) => format!("test:{name}"),
                Owner::Lib(_) => continue,
            };
            target_files.extend(mounted_files.into_iter().map(|(path, mounted)| TargetFile {
                package: package.to_owned(),
                filter: filter.clone(),
                path,
                mounted,
            }));
            runnable.insert(owner, filter);
        }
    }
    target_files
}

impl Program {
    /// File-level token pins (#2495), including edits that leave the SQL
    /// literal unchanged and removals visible only in the base. Registered
    /// observers are direct seeds, so the nearest-test budget retains them.
    fn scanned_token_seeds(
        &self,
        diffs: &[FileDiff],
        base: &Tree,
        candidate: &Tree,
        base_program: &Program,
    ) -> Result<Vec<usize>, String> {
        let Some(raw) = candidate.get(SCANNED_TOKENS) else {
            return Ok(Vec::new());
        };
        let registry: ScannedTokens = serde_json::from_str(raw)
            .map_err(|error| format!("invalid scanned-token registry: {error}"))?;
        if registry.version != 1 {
            return Err("unsupported scanned-token registry version".into());
        }
        let registry_changed = diffs.iter().any(|diff| {
            diff.old_path.as_deref() == Some(SCANNED_TOKENS)
                || diff.new_path.as_deref() == Some(SCANNED_TOKENS)
        });
        let mut triggered = BTreeSet::new();
        for diff in diffs {
            for (path, lines, tree, program) in [
                (
                    diff.old_path.as_deref(),
                    &diff.old_lines,
                    base,
                    base_program,
                ),
                (diff.new_path.as_deref(), &diff.new_lines, candidate, self),
            ] {
                let Some(path) = path else { continue };
                let Some((package, rest)) = split_package(path) else {
                    continue;
                };
                if !rest.starts_with("src/") || !path.ends_with(".rs") {
                    continue;
                }
                if program.lib_files.get(path).is_some_and(|file| file.test) {
                    continue;
                }
                let Some(text) = tree.get(path) else { continue };
                let parsed = parse_file(text);
                if !lines
                    .iter()
                    .any(|line| parsed.token_lines.contains(line) && !parsed.in_test(*line))
                {
                    continue;
                }
                let tokens = tokenize(text);
                for (token, rule) in &registry.tokens {
                    if rule.packages.iter().any(|name| name == package)
                        && tokens
                            .iter()
                            .any(|lexeme| lexeme.text.contains(token.as_str()))
                    {
                        triggered.insert(token.clone());
                    }
                }
            }
        }
        if registry_changed {
            triggered.extend(registry.tokens.keys().cloned());
        }
        let mut seeds = BTreeSet::new();
        for token in triggered {
            for test in &registry.tokens[&token].tests {
                let found: Vec<usize> = self
                    .items
                    .iter()
                    .enumerate()
                    .filter_map(|(index, flat)| {
                        (flat.target.is_none()
                            && flat.crate_name == test.package
                            && flat.item.test
                            && !flat.item.ignored
                            && flat.item.names.first().is_some_and(|name| {
                                join_module(&flat.module, std::slice::from_ref(name)) == test.name
                            }))
                        .then_some(index)
                    })
                    .collect();
                if found.is_empty() {
                    return Err(format!(
                        "scanned-token test does not resolve: {}::{}",
                        test.package, test.name
                    ));
                }
                seeds.extend(found);
            }
        }
        Ok(seeds.into_iter().collect())
    }

    fn build(tree: &Tree) -> Self {
        Self::build_for(tree, &LIB_PACKAGES)
    }

    /// The program of `packages`: the rsid family for the closure of its
    /// selection, the touched packages outside it for [`other_targets_from`].
    fn build_for(tree: &Tree, packages: &[&'static str]) -> Self {
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
            target_source_readers: Vec::new(),
        };
        let mut target_files: Vec<TargetFile> = Vec::new();
        for &package in packages {
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
            target_files.extend(index_targets(
                tree,
                package,
                &mut program.owners,
                &mut program.runnable_targets,
                &mut program.target_texts,
            ));
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
                    target: None,
                });
            }
        }
        // #1306: the items of every bin and integration test target that
        // declares tests join the closure, so a target whose code names an
        // affected library item (or reads a changed source) is affected.
        for file in target_files {
            let is_test_target = file.filter.starts_with("test:");
            let prefix = if file.mounted.module.is_empty() {
                file.filter.clone()
            } else {
                format!("{}::{}", file.filter, file.mounted.module)
            };
            for item in &file.mounted.parsed.items {
                program.items.push(FlatItem {
                    path: file.path.clone(),
                    crate_name: TARGET_CRATE,
                    module: join_module(&prefix, &item.inline),
                    in_test: is_test_target
                        || file.mounted.test
                        || file.mounted.parsed.in_test(item.start),
                    item: item.clone(),
                    target: Some((file.package.clone(), file.filter.clone())),
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
        // Test code that resolves the crate root (`rsi_common::manifest_dir!()`,
        // `rsi_common::manifest_dir!()` (#1741), or a test helper that does)
        // and names a source tree reads sources.
        let resolves_root = |item: &Item| {
            item.literals
                .iter()
                .any(|literal| literal == "CARGO_MANIFEST_DIR")
                || item.mentions.iter().any(|name| name == "manifest_dir")
        };
        let root_helpers: HashSet<&str> = program
            .items
            .iter()
            .filter(|flat| flat.in_test && resolves_root(&flat.item))
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
                    && (resolves_root(&flat.item)
                        || flat
                            .item
                            .mentions
                            .iter()
                            .any(|name| root_helpers.contains(name.as_str())))
            })
            .map(|(index, _)| index)
            .collect();
        // Target tests may copy and inspect production source trees. Track
        // those observers by the source crates named in their test/helper
        // literals, including helpers that resolve CARGO_MANIFEST_DIR.
        let target_root_helpers: BTreeMap<(String, String), HashSet<String>> = program
            .items
            .iter()
            .filter(|flat| flat.target.is_some() && !flat.in_test && resolves_root(&flat.item))
            .filter_map(|flat| {
                flat.target
                    .as_ref()
                    .map(|target| (target.clone(), flat.item.names.iter().cloned().collect()))
            })
            .collect();
        for flat in &program.items {
            let Some((package, filter)) = &flat.target else {
                continue;
            };
            if !flat.in_test {
                continue;
            }
            let helpers = target_root_helpers
                .get(&(package.clone(), filter.clone()))
                .cloned()
                .unwrap_or_default();
            let uses_root_helper = flat.item.mentions.iter().any(|name| helpers.contains(name));
            if !resolves_root(&flat.item) && !uses_root_helper {
                continue;
            }
            let mut literals = flat.item.literals.clone();
            if uses_root_helper {
                for helper in &program.items {
                    if helper.target.as_ref() == Some(&(package.clone(), filter.clone()))
                        && helper.item.names.iter().any(|name| helpers.contains(name))
                    {
                        literals.extend(helper.item.literals.iter().cloned());
                    }
                }
            }
            let mut crates = BTreeSet::new();
            for literal in literals {
                let pieces: Vec<_> = literal.split('/').collect();
                for window in pieces.windows(3) {
                    if window[0] == "crates" && window[2] == "src" {
                        crates.insert(window[1].to_owned());
                    }
                }
                if source_tree_path(&literal) {
                    crates.insert(package.clone());
                }
                if literal.contains("../") {
                    for part in pieces.windows(2) {
                        if part[0] == ".." && part[1].contains("rsid") {
                            crates.insert(part[1].to_owned());
                        }
                    }
                }
            }
            if !crates.is_empty() {
                program
                    .target_source_readers
                    .push((package.clone(), filter.clone(), crates));
            }
        }
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
            let Some(invoked) = item.invokes.clone() else {
                continue;
            };
            let invoked = &invoked;
            // The name of a compiler built-in macro in the invocation
            // (`include!(concat!(env!("OUT_DIR"), ..))`) names no item of
            // this crate: matching it by name made the invocation affected
            // by any function called `env` (#2086). A `macro_rules!` of this
            // crate with the same name still counts.
            item.mentions.retain(|name| {
                !BUILTIN_MACROS.contains(&name.as_str())
                    || defined.contains_key(&(flat.crate_name, name.clone()))
            });
            // A `macro_rules!` of this crate decides what it expands to; only
            // a macro it does not define can be a well-known external one
            // whose body is its own declarations (`thread_local!`, #2086).
            let names = match defined.get(&(flat.crate_name, invoked.clone())) {
                Some(Some(names)) => names,
                Some(None) => continue,
                None => match item
                    .expands_to
                    .as_ref()
                    .filter(|_| STD_ITEM_MACROS.contains(&invoked.as_str()))
                {
                    Some(names) => names,
                    None => continue,
                },
            };
            let names = names.clone();
            item.names = names;
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
        let mut readers: Vec<Reader> = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, flat)| {
                flat.target.is_none()
                    && flat
                        .item
                        .literals
                        .iter()
                        .any(|literal| literal_names_path(path, &flat.path, literal))
            })
            .map(|(index, _)| Reader::Item(index))
            .collect();
        readers.extend(
            target_readers(&self.target_texts, &self.owners, path)
                .into_iter()
                .map(Reader::Target),
        );
        readers
    }

    /// The test-only items (tests and their helpers) whose prose shares a
    /// fragment with `changed` (one holds the other): they assert text a
    /// changed production item writes, so they observe the edit although no
    /// name connects them (#1952).
    fn text_observers(&self, changed: &BTreeSet<String>) -> Vec<usize> {
        if changed.is_empty() {
            return Vec::new();
        }
        let mut observers = Vec::new();
        for (index, flat) in self.items.iter().enumerate() {
            if flat.in_test
                && flat.item.texts.iter().any(|text| {
                    changed.iter().any(|edited| {
                        edited.contains(text.as_str()) || text.contains(edited.as_str())
                    })
                })
            {
                observers.push(index);
            }
        }
        observers
    }

    /// The keys an affected item propagates, or why the names cannot.
    fn keys(&self, index: usize) -> Result<Vec<Key>, String> {
        let flat = &self.items[index];
        let item = &flat.item;
        let place = || format!("{} line {}", flat.path, item.start);
        if item.kind == ItemKind::Opaque {
            let what = item
                .invokes
                .as_ref()
                .map_or_else(String::new, |name| format!(" (`{name}!`)"));
            return Err(format!(
                "an item-level macro or unreadable impl{what} at {} is affected",
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
        // Nothing outside a bin or integration test target can name its items,
        // public or not: they are scoped to the target, never global.
        let target_scope = flat
            .target
            .as_ref()
            .map(|(_, filter)| (flat.crate_name, filter.clone()));
        let scope = match &target_scope {
            Some(scope) => Some(scope.clone()),
            None => (!global).then(|| (flat.crate_name, flat.module.clone())),
        };
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
                    scope: target_scope.clone(),
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

    /// The names a qualified key accepts: its qualifier, `Self` and every
    /// alias chain from them. Alias edges are deliberately scope-independent:
    /// extra matches are safe, but dropping a chained qualifier can hide a
    /// regression. A visited set closes chains even when reused names form
    /// cycles. Computed once per key, not per candidate (#2086).
    fn accepted_names<'a>(&'a self, qualifier: &'a str) -> HashSet<&'a str> {
        let mut accepted = HashSet::new();
        let mut pending = vec![qualifier, "Self"];
        while let Some(name) = pending.pop() {
            if !accepted.insert(name) {
                continue;
            }
            if let Some(aliases) = self.aliases.get(name) {
                pending.extend(aliases.iter().map(String::as_str));
            }
        }
        accepted
    }

    /// Whether `key` reaches item `index`; `accepted` is
    /// [`Self::accepted_names`] of the key's qualifier, when it has one.
    fn matches(&self, key: &Key, index: usize, accepted: Option<&HashSet<&str>>) -> bool {
        let flat = &self.items[index];
        if let Some((crate_name, module)) = &key.scope
            && (flat.crate_name != *crate_name || !Self::in_subtree(&flat.module, module))
        {
            return false;
        }
        let Some(accepted) = accepted else {
            return true;
        };
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
    fn closure(&self, seeds: &[usize], seed_keys: Vec<Key>) -> Result<Reached, String> {
        let mut affected = vec![false; self.items.len()];
        // FIFO keys carry the distance of their observers. All changed items
        // are distance zero; base-only names start at one. First visits are
        // shortest, including cycles and aliases, without rewalking the graph.
        let mut queue: VecDeque<(Key, usize)> = seed_keys.into_iter().map(|key| (key, 1)).collect();
        let mut seen_keys: HashSet<Key> = HashSet::new();
        let mut reached = Reached::default();
        // Every target with an affected item, at its nearest one; only those
        // that also declare an affected test are reached.
        let mut target_items: BTreeMap<&(String, String), usize> = BTreeMap::new();
        let mut tested_targets: HashSet<&(String, String)> = HashSet::new();
        let by_module = ModuleIndex::new(&self.items);
        let mut mark = |index: usize,
                        distance: usize,
                        queue: &mut VecDeque<(Key, usize)>,
                        reached: &mut Reached|
         -> Result<(), String> {
            if affected[index] {
                return Ok(());
            }
            affected[index] = true;
            let flat = &self.items[index];
            if let Some(target) = &flat.target {
                target_items
                    .entry(target)
                    .and_modify(|old| *old = (*old).min(distance))
                    .or_insert(distance);
            }
            if flat.item.test && !flat.item.ignored {
                if let Some(target) = &flat.target {
                    tested_targets.insert(target);
                } else if let Some(name) = flat.item.names.first() {
                    let full = join_module(&flat.module, std::slice::from_ref(name));
                    if LEAF_PACKAGES.contains(&flat.crate_name) {
                        reached.leaf_named.insert(full.clone());
                    } else {
                        reached.family_named.insert(full.clone());
                    }
                    reached
                        .tests
                        .entry(full)
                        .and_modify(|old| *old = (*old).min(distance))
                        .or_insert(distance);
                }
            }
            queue.extend(self.keys(index)?.into_iter().map(|key| (key, distance + 1)));
            Ok(())
        };
        for seed in seeds {
            mark(*seed, 0, &mut queue, &mut reached)?;
        }
        while let Some((key, distance)) = queue.pop_front() {
            if !seen_keys.insert(key.clone()) {
                continue;
            }
            let candidates: Vec<usize> = if key.name.is_empty() {
                let (crate_name, module) = key.scope.clone().unwrap_or(("", String::new()));
                by_module.subtree(crate_name, &module)
            } else {
                self.by_mention.get(&key.name).cloned().unwrap_or_default()
            };
            let accepted = key
                .qualifier
                .as_deref()
                .map(|qualifier| self.accepted_names(qualifier));
            for index in candidates {
                if self.matches(&key, index, accepted.as_ref()) {
                    mark(index, distance, &mut queue, &mut reached)?;
                }
            }
        }
        reached.targets = target_items
            .into_iter()
            .filter(|(target, _)| tested_targets.contains(target))
            .map(|(target, distance)| (target.clone(), distance))
            .collect();
        Ok(reached)
    }
}

// ---------------------------------------------------------------------------
// Proven test-only diffs (#2512)

/// A compilation target whose test build compiles a test-only edit.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum TestTarget {
    /// The package's library test build.
    Lib(String),
    /// `cargo test -p PACKAGE --bin NAME`.
    Bin(String, String),
    /// `cargo test -p PACKAGE --test NAME`.
    Test(String, String),
}

/// A whole diff proven to leave every production (non-test) build unchanged,
/// and the test targets that compile the test code it does change.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TestOnlyDiff {
    pub(crate) packages: BTreeSet<String>,
    pub(crate) targets: BTreeSet<TestTarget>,
}

/// The [`TestOnlyDiff`] of `candidate` against `base`, read from `repo`. `None`
/// when any part of the diff is not provably test-only, or cannot be read: the
/// gate then keeps every compile check.
pub(crate) fn test_only_diff(repo: &Path, base: &str, candidate: &str) -> Option<TestOnlyDiff> {
    let output = repo_git(repo)
        .args([
            "diff",
            "--name-status",
            "-z",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            base,
            candidate,
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut fields = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty());
    let mut paths = Vec::new();
    while let Some(status) = fields.next() {
        // A type change, copy or unmerged entry is never provable.
        if !matches!(status, b"M" | b"A" | b"D") {
            return None;
        }
        paths.push(std::str::from_utf8(fields.next()?).ok()?.to_owned());
    }
    // The cheap path checks run before the two trees (tens of MB of Rust
    // each) are loaded and parsed.
    if paths.is_empty() || !paths.iter().all(|path| test_only_path_candidate(path)) {
        return None;
    }
    let candidate_tree = load_tree(repo, candidate).ok()?;
    let base_tree = load_tree(repo, base).ok()?;
    test_only_from(&paths, &base_tree, &candidate_tree)
}

/// A path that could be part of a test-only proof: a Rust source of an
/// rsid-family package other than its build script.
fn test_only_path_candidate(path: &str) -> bool {
    split_package(path).is_some_and(|(_, rest)| path.ends_with(".rs") && rest != "build.rs")
}

/// Whether `path` is the root file of a binary target Cargo discovers from
/// the file existing (`src/main.rs`, `src/bin/<name>.rs`,
/// `src/bin/<name>/main.rs`).
fn is_bin_target_root(rest: &str) -> bool {
    if rest == "src/main.rs" {
        return true;
    }
    let Some(under) = rest.strip_prefix("src/bin/") else {
        return false;
    };
    match under.split_once('/') {
        None => under.ends_with(".rs"),
        Some((_, tail)) => tail == "main.rs",
    }
}

/// The binary targets a program builds.
fn bin_owners(program: &Program) -> BTreeSet<&Owner> {
    program
        .owners
        .values()
        .flatten()
        .filter(|owner| matches!(owner, Owner::Bin(..)))
        .collect()
}

/// [`test_only_diff`] over the changed `paths` and the two trees. Every path
/// of the whole diff must be a mounted Rust source of an rsid-family package
/// whose production tokens are identical on both sides: test regions
/// (`#[cfg(test)]` and `#[cfg(all(test, ..))]` items, modules and impls,
/// `#[test]` functions, files mounted only under `#[cfg(test)]`, integration
/// test targets) are ignored, everything else (cfg boundaries, `cfg(any(test,
/// feature = ..))` seams, doc comments, signatures) is compared. A source that
/// nothing mounts, that is read as text, or that a build script may collect is
/// not provable.
pub(crate) fn test_only_from(
    paths: &[String],
    base: &Tree,
    candidate: &Tree,
) -> Option<TestOnlyDiff> {
    if paths.is_empty() {
        return None;
    }
    // Cheap checks before either tree is parsed.
    for path in paths {
        let (_, rest) = split_package(path)?;
        if !test_only_path_candidate(path)
            || !(base.contains_key(path) || candidate.contains_key(path))
        {
            return None;
        }
        // Cargo creates a binary target from its root file existing, so an
        // added or deleted root changes what builds even when it has no
        // production token (a missing `main` is E0601 in a production build).
        if is_bin_target_root(rest) && base.contains_key(path) != candidate.contains_key(path) {
            return None;
        }
    }
    let base_program = Program::build(base);
    let candidate_program = Program::build(candidate);
    if base_program.mount_error.is_some() || candidate_program.mount_error.is_some() {
        return None;
    }
    if bin_owners(&base_program) != bin_owners(&candidate_program) {
        return None;
    }
    let mut proof = TestOnlyDiff::default();
    for path in paths {
        let (package, _) = split_package(path)?;
        let mut fingerprints = Vec::new();
        for (tree, program) in [(base, &base_program), (candidate, &candidate_program)] {
            // Code compiled in from this source as text.
            if !program.literal_readers(path).is_empty() {
                return None;
            }
            let Some(text) = tree.get(path) else {
                fingerprints.push(Vec::new());
                continue;
            };
            let owners = program
                .owners
                .get(path)
                .filter(|owners| !owners.is_empty())?;
            let mut whole_file_test = true;
            for owner in owners {
                match owner {
                    Owner::Lib(name) => {
                        whole_file_test &= program.lib_files.get(path)?.test;
                        proof.targets.insert(TestTarget::Lib((*name).to_owned()));
                    }
                    Owner::Bin(name, target) => {
                        whole_file_test = false;
                        program.runnable_targets.get(owner)?;
                        proof
                            .targets
                            .insert(TestTarget::Bin(name.clone(), target.clone()));
                    }
                    Owner::Test(name, target) => {
                        program.runnable_targets.get(owner)?;
                        proof
                            .targets
                            .insert(TestTarget::Test(name.clone(), target.clone()));
                    }
                }
            }
            fingerprints.push(production_fingerprint(text, whole_file_test)?);
        }
        if fingerprints[0] != fingerprints[1] {
            return None;
        }
        proof.packages.insert(package.to_owned());
    }
    Some(proof)
}

/// The production tokens of one source, in order, with the doc comments the
/// tokenizer drops put back at their lines (a lint may deny missing docs).
/// `None` for a source with block doc comments, which this does not read.
fn production_fingerprint(text: &str, whole_file_test: bool) -> Option<Vec<String>> {
    if whole_file_test {
        return Some(Vec::new());
    }
    if text.contains("/**") || text.contains("/*!") {
        return None;
    }
    let parsed = parse_file(text);
    let mut entries: Vec<(u32, String)> = tokenize(text)
        .iter()
        .filter(|token| !(parsed.in_test(token.line) && parsed.in_test(token.end_line)))
        .map(|token| (token.line, format!("{:?}:{}", token.kind, token.text)))
        .collect();
    for (index, line) in text.lines().enumerate() {
        let number = u32::try_from(index).unwrap_or(u32::MAX).saturating_add(1);
        let trimmed = line.trim_start();
        let doc = (trimmed.starts_with("///") && !trimmed.starts_with("////"))
            || trimmed.starts_with("//!");
        if doc && !parsed.in_test(number) {
            entries.push((number, format!("doc:{}", trimmed.trim_end())));
        }
    }
    // Stable: a line's doc entry follows its tokens, which keep their order.
    entries.sort_by_key(|(line, _)| *line);
    Some(entries.into_iter().map(|(_, entry)| entry).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_first_ranking_keeps_direct_tests_and_breaks_ties_by_name() {
        let tests = BTreeMap::from([
            ("far".into(), 8),
            ("b".into(), 3),
            ("a".into(), 3),
            ("direct".into(), 1),
            ("changed".into(), 0),
        ]);
        assert_eq!(
            nearest_tests(&tests, 3),
            BTreeSet::from(["a".into(), "direct".into(), "changed".into(),])
        );
        assert_eq!(
            nearest_tests(&tests, 1),
            BTreeSet::from(["direct".into(), "changed".into()])
        );
    }

    #[test]
    fn every_test_at_the_minimum_distance_survives_a_smaller_budget() {
        let tests = BTreeMap::from([("a".into(), 4), ("b".into(), 4), ("c".into(), 5)]);
        assert_eq!(
            nearest_tests(&tests, 1),
            BTreeSet::from(["a".into(), "b".into()])
        );
        assert!(nearest_tests(&BTreeMap::new(), 400).is_empty());
    }

    // ea12bae9a against ce1c4b7af: reclaim callers reach daemon hubs, which
    // reach hundreds of unrelated RPC tests. This fixture is independent of
    // retained git history and exercises select_from, not just a sorter.
    fn daemon_hub_tree() -> Tree {
        let mut source = String::from(
            "pub fn ensure_idle_cargo_target() {}\n\
             pub fn reclaim_terminal_targets() { ensure_idle_cargo_target(); }\n\
             pub fn graph_runner() { reclaim_terminal_targets(); }\n\
             pub fn run_daemon() { graph_runner(); }\n\
             pub fn handle_request_inner() { run_daemon(); }\n\
             #[test]\nfn direct_reclaim() { ensure_idle_cargo_target(); }\n\
             #[test]\nfn direct_reclaim_second() { ensure_idle_cargo_target(); }\n\
             #[test]\nfn nearer_than_rpc() { reclaim_terminal_targets(); }\n",
        );
        for i in 0..(MAX_SELECTED_TESTS + 300) {
            source.push_str(&format!(
                "#[test]\nfn rpc_{i:04}() {{ handle_request_inner(); }}\n"
            ));
        }
        tree(&[
            ("crates/rsid/src/lib.rs", "mod target_reclaim;"),
            ("crates/rsid/src/target_reclaim.rs", &source),
        ])
    }

    #[test]
    fn ea12bae9a_daemon_hub_shape_selects_a_bounded_closure() {
        let tree = daemon_hub_tree();
        let selection = select_from(
            &[changed("crates/rsid/src/target_reclaim.rs", &[1])],
            &tree,
            &tree,
        )
        .unwrap();
        let tests = selected(&selection);
        assert_eq!(tests.len(), MAX_SELECTED_TESTS);
        assert_eq!(selection.reached_tests, MAX_SELECTED_TESTS + 303);
        assert!(tests.contains("target_reclaim::direct_reclaim"));
        assert!(tests.contains("target_reclaim::direct_reclaim_second"));
        assert!(tests.contains("target_reclaim::nearer_than_rpc"));
        assert!(tests.contains("target_reclaim::rpc_0396"));
        assert_eq!(
            selection.evidence(),
            "selection=bounded selected=400 reached=703"
        );
        // #1957: the whole budget keeps every reached test, what a stale
        // advance can observe rather than what a gate can afford.
        let whole = select_from_within(
            &[changed("crates/rsid/src/target_reclaim.rs", &[1])],
            &tree,
            &tree,
            Budget::WHOLE,
        )
        .unwrap();
        assert_eq!(selected(&whole).len(), MAX_SELECTED_TESTS + 303);
        assert!(selected(&whole).contains("target_reclaim::rpc_0699"));
        assert_eq!(
            whole.evidence(),
            "selection=closure selected=703 reached=703"
        );
    }

    #[test]
    fn explanation_reports_the_real_selectors_distances_and_cut() {
        let tree = daemon_hub_tree();
        let diffs = [changed("crates/rsid/src/target_reclaim.rs", &[1])];
        let real = select_from(&diffs, &tree, &tree).unwrap();
        let (selection, explanation) =
            select_from_explained(&diffs, &tree, &tree, Budget::GATE).unwrap();
        // Explaining never changes what the selector chooses.
        assert_eq!(selection, real);
        assert_eq!(explanation.test_distances.len(), selection.reached_tests);
        let report = render_explanation(Some(&(selection, explanation)));
        assert!(report.starts_with("selection=bounded selected=400 reached=703\n"));
        assert!(report.contains("max_selected_tests=400 max_selected_targets=5\n"));
        let line = |needle: &str| report.lines().find(|l| l.ends_with(needle)).unwrap();
        assert!(line("target_reclaim::direct_reclaim").contains("mandatory=true selected=true"));
        assert!(line("target_reclaim::rpc_0396").contains("selected=true"));
        assert!(line("target_reclaim::rpc_0699").contains("mandatory=false selected=false"));
    }

    #[test]
    fn explanation_of_an_all_shards_selection_says_so() {
        assert!(render_explanation(None).starts_with("selection=all_shards"));
    }

    fn target(name: &str) -> (String, String) {
        ("rsid".to_owned(), name.to_owned())
    }

    #[test]
    fn nearest_targets_keep_direct_targets_beyond_the_bound_and_break_ties_by_name() {
        let targets = BTreeMap::from([
            (target("test:far"), 9),
            (target("test:b_hub"), 5),
            (target("test:a_hub"), 5),
            (target("test:near"), 3),
            (target("test:caller"), 1),
            (target("bin:reader"), 0),
        ]);
        assert_eq!(
            nearest_targets(&targets, 4),
            BTreeSet::from([
                target("bin:reader"),
                target("test:caller"),
                target("test:near"),
                target("test:a_hub"),
            ])
        );
        // Direct targets are never cut, even below the budget.
        assert_eq!(
            nearest_targets(&targets, 1),
            BTreeSet::from([target("bin:reader"), target("test:caller")])
        );
        let direct: BTreeMap<_, _> = (0..(MAX_SELECTED_TARGETS + 2))
            .map(|i| (target(&format!("test:direct_{i}")), 1))
            .collect();
        assert_eq!(
            nearest_targets(&direct, MAX_SELECTED_TARGETS).len(),
            MAX_SELECTED_TARGETS + 2
        );
        assert!(nearest_targets(&BTreeMap::new(), MAX_SELECTED_TARGETS).is_empty());
    }

    /// #2094, ea12bae9a against ce1c4b7af: the reclaim change reached 40
    /// rsid targets, most only through daemon hubs. A target that reads the
    /// changed source or calls changed code (itself or through its own
    /// helper) always runs; the nearest others fill the budget; the rest are
    /// left to the queue's async full run.
    #[test]
    fn ea12bae9a_hub_targets_select_direct_and_nearest_within_the_target_budget() {
        let mut fixture = daemon_hub_tree();
        for (path, text) in [
            (
                "crates/rsid/src/bin/reader.rs".to_owned(),
                "fn main() {}\n#[test]\nfn reads() { let _ = include_str!(\"../target_reclaim.rs\"); }\n"
                    .to_owned(),
            ),
            (
                "crates/rsid/tests/direct_caller.rs".to_owned(),
                "#[test]\nfn it() { rsid::target_reclaim::ensure_idle_cargo_target(); }\n"
                    .to_owned(),
            ),
            (
                "crates/rsid/tests/through_helper.rs".to_owned(),
                "fn reclaim_helper() { rsid::target_reclaim::ensure_idle_cargo_target(); }\n\
                 #[test]\nfn it() { reclaim_helper(); }\n"
                    .to_owned(),
            ),
            (
                "crates/rsid/tests/near_runner.rs".to_owned(),
                "#[test]\nfn it() { rsid::target_reclaim::graph_runner(); }\n".to_owned(),
            ),
        ] {
            fixture.insert(path, text);
        }
        for i in 0..36 {
            fixture.insert(
                format!("crates/rsid/tests/rpc_{i:02}.rs"),
                "#[test]\nfn it() { rsid::target_reclaim::handle_request_inner(); }\n".to_owned(),
            );
        }
        let selection = select_from(
            &[changed("crates/rsid/src/target_reclaim.rs", &[1])],
            &fixture,
            &fixture,
        )
        .unwrap();
        assert_eq!(selection.reached_targets, 40);
        assert_eq!(
            selection.targets,
            BTreeSet::from([
                target("bin:reader"),
                target("test:direct_caller"),
                target("test:through_helper"),
                target("test:near_runner"),
                target("test:rpc_00"),
            ])
        );
        assert!(selection.targets_bounded());
        assert_eq!(
            selection.evidence(),
            "selection=bounded selected=400 reached=703 targets=bounded selected=5 reached=40"
        );
        // Within the budget nothing is omitted and the evidence is unchanged.
        let few: Tree = fixture
            .into_iter()
            .filter(|(path, _)| !path.contains("/tests/rpc_"))
            .collect();
        let selection = select_from(
            &[changed("crates/rsid/src/target_reclaim.rs", &[1])],
            &few,
            &few,
        )
        .unwrap();
        assert_eq!(selection.targets.len(), 4);
        assert!(!selection.targets_bounded());
        assert_eq!(
            selection.evidence(),
            "selection=bounded selected=400 reached=703"
        );
    }

    #[test]
    fn closure_distances_are_shortest_across_cycles_and_base_only_names() {
        let fixture = daemon_hub_tree();
        let program = Program::build(&fixture);
        let seed = program
            .items
            .iter()
            .position(|i| i.item.names.iter().any(|n| n == "ensure_idle_cargo_target"))
            .unwrap();
        let candidate = program.closure(&[seed], vec![]).unwrap();
        let removed = program.closure(&[], program.keys(seed).unwrap()).unwrap();
        assert_eq!(candidate.tests, removed.tests);
        assert_eq!(candidate.tests["target_reclaim::direct_reclaim"], 1);
        assert_eq!(candidate.tests["target_reclaim::nearer_than_rpc"], 2);
        assert_eq!(candidate.tests["target_reclaim::rpc_0000"], 5);
        let cyclic = tree(&[
            ("crates/rsid/src/lib.rs", "mod m;"),
            (
                "crates/rsid/src/m.rs",
                "pub fn a() { b(); }\npub fn b() { a(); }\n#[test]\nfn direct() { a(); b(); }\n",
            ),
        ]);
        let program = Program::build(&cyclic);
        let seed = program
            .items
            .iter()
            .position(|i| i.item.names.iter().any(|n| n == "a"))
            .unwrap();
        assert_eq!(
            program.closure(&[seed], vec![]).unwrap().tests["m::direct"],
            1
        );
    }

    #[test]
    fn crate_root_manifest_build_script_and_toolchain_keep_everything() {
        let fixture = daemon_hub_tree();
        for path in [
            "crates/rsid/src/lib.rs",
            "crates/rsid-store/src/lib.rs",
            "crates/rsid/Cargo.toml",
            "crates/rsid-store/build.rs",
            "Cargo.toml",
            "Cargo.lock",
            "rust-toolchain",
            "rust-toolchain.toml",
        ] {
            assert_eq!(
                select_from(&[changed(path, &[1])], &fixture, &fixture),
                None,
                "{path}"
            );
        }
    }

    #[test]
    fn changed_opaque_impl_keeps_everything_when_distance_cannot_be_computed() {
        let fixture = tree(&[
            ("crates/rsid/src/lib.rs", "mod m;"),
            (
                "crates/rsid/src/m.rs",
                "impl Trait for <T as Other>::Assoc {\nfn foo() {}\n}\n",
            ),
        ]);
        let selection =
            select_from(&[changed("crates/rsid/src/m.rs", &[1])], &fixture, &fixture).unwrap();
        assert!(matches!(selection.lib, LibTests::Everything(_)));
    }

    #[test]
    fn a_large_nearest_tier_stays_selected_instead_of_falling_back_to_everything() {
        let tests: BTreeMap<_, _> = (0..(MAX_SELECTED_TESTS + 1))
            .map(|i| (format!("test_{i}"), 3))
            .collect();
        assert_eq!(
            nearest_tests(&tests, MAX_SELECTED_TESTS).len(),
            MAX_SELECTED_TESTS + 1
        );
    }

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

    fn package_targets_of(
        files: &[(&str, &str)],
        changed_files: &[(&str, &str)],
        packages: &[&str],
    ) -> BTreeMap<String, PackageTargets> {
        let base = tree(files);
        let mut candidate = base.clone();
        let mut diffs = Vec::new();
        for (path, text) in changed_files {
            candidate.insert((*path).to_owned(), (*text).to_owned());
            let lines = u32::try_from(text.lines().count().max(1)).unwrap();
            diffs.push(changed(path, &(1..=lines).collect::<Vec<_>>()));
        }
        let packages: Vec<String> = packages
            .iter()
            .map(|package| (*package).to_owned())
            .collect();
        other_targets_from(&diffs, &base, &candidate, &packages)
    }

    /// The targets of `package` after replacing `path` with `text`, where only
    /// `lines` of the new file changed.
    fn targets_after(
        files: &[(&str, &str)],
        path: &str,
        text: &str,
        lines: &[u32],
        package: &str,
    ) -> PackageTargets {
        let base = tree(files);
        let mut candidate = base.clone();
        candidate.insert(path.to_owned(), text.to_owned());
        other_targets_from(
            &[changed(path, lines)],
            &base,
            &candidate,
            &[package.to_owned()],
        )
        .remove(package)
        .expect("seeded")
    }

    const TEST: &str = "#[test]\nfn t() {}\n";

    /// #1937, #1650: a changed integration test or bin target of a package
    /// outside the rsid family runs its own tests, including through a module
    /// it mounts; a library-only change runs no target.
    #[test]
    fn changed_bin_and_integration_targets_of_other_packages_are_selected() {
        let files = [
            ("crates/rsi-remote/src/lib.rs", "pub fn l() {}\n"),
            (
                "crates/rsi-remote/src/main.rs",
                "mod gateway;\nfn main() {}\n#[test]\nfn m() {}\n",
            ),
            ("crates/rsi-remote/src/gateway.rs", "pub fn g() {}\n"),
            ("crates/rsi-remote/tests/api.rs", TEST),
            ("crates/rsi-remote/tests/other.rs", TEST),
            ("crates/rsi-remote/tests/untested.rs", "fn helper() {}\n"),
        ];
        let selected = |changed_files: &[(&str, &str)]| {
            package_targets_of(&files, changed_files, &["rsi-remote"])["rsi-remote"].clone()
        };
        let main = selected(&[(
            "crates/rsi-remote/src/main.rs",
            "mod gateway;\nfn main() { 1; }\n#[test]\nfn m() {}\n",
        )]);
        assert!(main.has_lib);
        assert_eq!(main.targets, BTreeSet::from(["bin:rsi-remote".to_owned()]));
        let module = selected(&[("crates/rsi-remote/src/gateway.rs", "pub fn g() { 1; }\n")]);
        assert_eq!(
            module.targets,
            BTreeSet::from(["bin:rsi-remote".to_owned()])
        );
        let integration =
            selected(&[("crates/rsi-remote/tests/api.rs", "#[test]\nfn t() { 1; }\n")]);
        assert_eq!(integration.targets, BTreeSet::from(["test:api".to_owned()]));
        // A target that declares no test has nothing to run.
        let untested = selected(&[(
            "crates/rsi-remote/tests/untested.rs",
            "fn helper() { 1; }\n",
        )]);
        assert!(untested.targets.is_empty(), "{untested:?}");
        // A library-only change leaves the targets to the lib run and the compile check.
        let lib = selected(&[("crates/rsi-remote/src/lib.rs", "pub fn l() { 1; }\n")]);
        assert!(lib.has_lib && lib.targets.is_empty(), "{lib:?}");
        // ... which owes a compile check of the package's other targets (#1952).
        assert!(lib.check, "{lib:?}");
        // A manifest change can change what every target compiles.
        let manifest = selected(&[(
            "crates/rsi-remote/Cargo.toml",
            "[package]\nname = \"rsi-remote\"\n",
        )]);
        assert_eq!(
            manifest.targets,
            BTreeSet::from([
                "bin:rsi-remote".to_owned(),
                "test:api".to_owned(),
                "test:other".to_owned()
            ])
        );
    }

    /// Comment-only edits of a target change no test, and a package without a
    /// library target has none to run.
    #[test]
    fn comment_only_target_edits_select_nothing_and_a_bin_only_package_has_no_lib() {
        let base = tree(&[(
            "crates/rsi-diag/src/main.rs",
            "fn main() {}\n#[test]\nfn t() {}\n",
        )]);
        let mut candidate = base.clone();
        candidate.insert(
            "crates/rsi-diag/src/main.rs".to_owned(),
            "// note\nfn main() {}\n#[test]\nfn t() {}\n".to_owned(),
        );
        let packages = ["rsi-diag".to_owned()];
        // Only line 1 (the comment) is new; the rest merely shifted.
        let comment = other_targets_from(
            &[changed("crates/rsi-diag/src/main.rs", &[1])],
            &base,
            &candidate,
            &packages,
        );
        assert!(comment["rsi-diag"].targets.is_empty(), "{comment:?}");
        assert!(!comment["rsi-diag"].has_lib);
        candidate.insert(
            "crates/rsi-diag/src/main.rs".to_owned(),
            "fn main() { 1; }\n#[test]\nfn t() {}\n".to_owned(),
        );
        let code = other_targets_from(
            &[changed("crates/rsi-diag/src/main.rs", &[1])],
            &base,
            &candidate,
            &packages,
        );
        assert_eq!(
            code["rsi-diag"].targets,
            BTreeSet::from(["bin:rsi-diag".to_owned()])
        );
    }

    /// A target that reads a changed data file by a literal runs; an unrelated one does not.
    #[test]
    fn a_target_that_reads_a_changed_data_file_is_selected() {
        let files = [
            ("crates/rsi-common/src/lib.rs", "pub fn l() {}\n"),
            (
                "crates/rsi-common/tests/corpus.rs",
                "#[test]\nfn t() { let _ = dir.join(\"a.json\"); }\n",
            ),
            ("crates/rsi-common/tests/other.rs", TEST),
        ];
        let base = tree(&files);
        let diff = FileDiff {
            old_path: Some("crates/rsi-common/tests/fixtures/a.json".into()),
            new_path: Some("crates/rsi-common/tests/fixtures/a.json".into()),
            old_lines: vec![1],
            new_lines: vec![1],
            binary: false,
        };
        let found = other_targets_from(&[diff], &base, &base, &["rsi-common".to_owned()]);
        assert_eq!(
            found["rsi-common"].targets,
            BTreeSet::from(["test:corpus".to_owned()])
        );
    }

    /// #1952: a library API change reaches the unchanged bin and integration
    /// targets that name it, and owes a compile check of every target of the
    /// package, including the ones that declare no test.
    #[test]
    fn a_library_change_selects_the_unchanged_targets_that_link_it() {
        let lib = "pub fn api() -> u32 { 1 }\npub fn unrelated() -> u32 { 2 }\n";
        let files = [
            ("crates/rsi-common/src/lib.rs", lib),
            (
                "crates/rsi-common/src/bin/tool.rs",
                "fn main() {}\n#[test]\nfn uses() { let _ = rsi_common::api(); }\n",
            ),
            (
                "crates/rsi-common/tests/corpus.rs",
                "#[test]\nfn t() { let _ = rsi_common::api(); }\n",
            ),
            (
                "crates/rsi-common/tests/helper_only.rs",
                "fn helper() { let _ = rsi_common::api(); }\n",
            ),
            (
                "crates/rsi-common/tests/other.rs",
                "#[test]\nfn t() { let _ = rsi_common::unrelated(); }\n",
            ),
        ];
        let found = targets_after(
            &files,
            "crates/rsi-common/src/lib.rs",
            "pub fn api() -> u32 { 3 }\npub fn unrelated() -> u32 { 2 }\n",
            &[1],
            "rsi-common",
        );
        let found = &found;
        assert!(found.has_lib && found.check, "{found:?}");
        assert_eq!(
            found.targets,
            BTreeSet::from(["bin:tool".to_owned(), "test:corpus".to_owned()]),
            "{found:?}"
        );
        // The change of an item no target names selects no target, but the
        // package's targets must still compile.
        let found = targets_after(
            &files,
            "crates/rsi-common/src/lib.rs",
            "pub fn api() -> u32 { 1 }\npub fn unrelated() -> u32 { 4 }\n",
            &[2],
            "rsi-common",
        );
        assert_eq!(
            found.targets,
            BTreeSet::from(["test:other".to_owned()]),
            "{found:?}"
        );
        assert!(found.check);
    }

    /// #1952: no compile check without a bin or integration target to break,
    /// and none for an edit that only changes test code.
    #[test]
    fn the_target_compile_check_needs_a_production_change_and_a_target() {
        let lib_only = [("crates/rsi-signal/src/lib.rs", "pub fn f() -> u32 { 1 }\n")];
        let changed = [("crates/rsi-signal/src/lib.rs", "pub fn f() -> u32 { 2 }\n")];
        let found = package_targets_of(&lib_only, &changed, &["rsi-signal"]);
        assert!(!found["rsi-signal"].check, "{found:?}");
        let with_tests = [
            (
                "crates/rsi-signal/src/lib.rs",
                "pub fn f() -> u32 { 1 }\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() { assert_eq!(super::f(), 1); }\n}\n",
            ),
            ("crates/rsi-signal/tests/api.rs", TEST),
        ];
        let found = targets_after(
            &with_tests,
            "crates/rsi-signal/src/lib.rs",
            "pub fn f() -> u32 { 1 }\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() { assert_eq!(super::f(), 2); }\n}\n",
            &[5],
            "rsi-signal",
        );
        assert!(!found.check, "{found:?}");
        let found = targets_after(
            &with_tests,
            "crates/rsi-signal/src/lib.rs",
            "pub fn f() -> u32 { 2 }\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() { assert_eq!(super::f(), 1); }\n}\n",
            &[1],
            "rsi-signal",
        );
        assert!(found.check, "{found:?}");
        // A manifest change is a build input of every target.
        let manifest = [(
            "crates/rsi-signal/Cargo.toml",
            "[package]\nname = \"rsi-signal\"\n",
        )];
        let found = package_targets_of(&with_tests, &manifest, &["rsi-signal"]);
        assert!(found["rsi-signal"].check, "{found:?}");
    }

    /// #1952: library code the names cannot place runs every runnable target
    /// of the package rather than none ("found nothing never means run
    /// nothing").
    #[test]
    fn unplaceable_library_code_runs_every_target_of_the_package() {
        let files = [
            ("crates/rsi-common/src/lib.rs", "pub fn api() {}\n"),
            ("crates/rsi-common/src/orphan.rs", "pub fn gone() {}\n"),
            ("crates/rsi-common/tests/a.rs", TEST),
            ("crates/rsi-common/tests/b.rs", TEST),
            ("crates/rsi-common/tests/helper_only.rs", "fn helper() {}\n"),
        ];
        let found = package_targets_of(
            &files,
            &[("crates/rsi-common/src/orphan.rs", "pub fn gone() { 1; }\n")],
            &["rsi-common"],
        );
        assert_eq!(
            found["rsi-common"].targets,
            BTreeSet::from(["test:a".to_owned(), "test:b".to_owned()]),
            "{found:?}"
        );
        assert!(found["rsi-common"].check);
    }

    /// #1952: a closure that crosses two touched packages selects the target
    /// of the package that names the other's changed item.
    #[test]
    fn a_closure_selects_the_targets_of_every_touched_package() {
        let files = [
            ("crates/rsi-common/src/lib.rs", "pub fn shared_api() {}\n"),
            ("crates/rsi-common/tests/own.rs", TEST),
            (
                "crates/rsi-diag/src/main.rs",
                "fn main() {}\n#[test]\nfn t() { rsi_common::shared_api(); }\n",
            ),
        ];
        let found = package_targets_of(
            &files,
            &[(
                "crates/rsi-common/src/lib.rs",
                "pub fn shared_api() { 1; }\n",
            )],
            &["rsi-common", "rsi-diag"],
        );
        assert_eq!(
            found["rsi-diag"].targets,
            BTreeSet::from(["bin:rsi-diag".to_owned()]),
            "{found:?}"
        );
        assert!(found["rsi-common"].targets.is_empty(), "{found:?}");
    }

    /// #1952: a test that asserts the prose a changed store item writes (a
    /// launch brief) observes the edit without naming the item.
    #[test]
    fn a_test_that_asserts_text_a_changed_item_writes_is_selected() {
        const BRIEF: &str = "pub fn bound_brief() -> String {\n    format!(\"{}\\n\\nPublish your complete handoff with the `handoff` field.\", 1)\n}\n";
        const PINNED: &str = "#[test]\nfn bound_worker_brief_is_published() {\n    let query = String::new();\n    assert!(query.contains(\"Publish your complete handoff with the `handoff` field.\"));\n}\n#[test]\nfn quoted_unrelated() {\n    assert!(String::new().contains(\"nothing about the brief at all here\"));\n}\n";
        let base = tree(&[
            ("crates/rsid-store/src/lib.rs", "pub mod brief;\n"),
            ("crates/rsid-store/src/brief.rs", BRIEF),
            ("crates/rsid/src/lib.rs", "pub mod pinned;\n"),
            ("crates/rsid/src/pinned.rs", PINNED),
        ]);
        let mut candidate = base.clone();
        candidate.insert(
            "crates/rsid-store/src/brief.rs".into(),
            BRIEF.replace("Publish your", "Send your"),
        );
        let line = line_of(BRIEF, "format!");
        let diff = FileDiff {
            old_path: Some("crates/rsid-store/src/brief.rs".into()),
            new_path: Some("crates/rsid-store/src/brief.rs".into()),
            old_lines: vec![line],
            new_lines: vec![line],
            binary: false,
        };
        let selection = select_from(&[diff], &base, &candidate).unwrap();
        let names = selected(&selection);
        // The base side held the asserted text, so the test now fails.
        assert!(
            names.contains("pinned::bound_worker_brief_is_published"),
            "{names:?}"
        );
        assert!(!names.contains("pinned::quoted_unrelated"), "{names:?}");
    }

    #[test]
    fn prose_fragments_unescape_and_split_on_placeholders() {
        let mut found = Vec::new();
        prose_fragments(
            "{}\\n\\nThe daemon copied the \\\"Issues\\\" below{x:?} as they stood on the ground.\\nshort",
            &mut found,
        );
        assert_eq!(
            found,
            vec![
                "The daemon copied the \"Issues\" below".to_owned(),
                "as they stood on the ground.".to_owned(),
            ]
        );
        let mut none = Vec::new();
        prose_fragments("launch_refused_no_spaces_here_at_all", &mut none);
        prose_fragments("too short", &mut none);
        assert!(none.is_empty(), "{none:?}");
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

    const LEAF_LIB: &str = "mod projection;\npub use projection::project_pending;\n";
    const LEAF_PROJECTION: &str = "\
pub fn project_pending() -> u32 {
    1
}

pub fn unrelated_projection() -> u32 {
    9
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_one_pending_item() {
        assert_eq!(project_pending(), 1);
    }

    #[test]
    fn projects_something_else() {
        assert_eq!(unrelated_projection(), 9);
    }
}
";
    const REMOTE_LIB: &str = "pub mod remote_read;\npub mod session;\n";
    const REMOTE_FACADE: &str = "mod owner;\npub use rsid_remote_read::*;\n";
    const REMOTE_OWNER: &str = "\
pub fn spawn_info_read() -> u32 {
    project_pending()
}

pub fn unrelated_owner() -> u32 {
    2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_reads_the_projection() {
        assert_eq!(spawn_info_read(), 1);
    }

    #[test]
    fn owner_something_else() {
        assert_eq!(unrelated_owner(), 2);
    }
}
";
    const SESSION_QUERIES: &str = "\
pub fn session_probe() -> u32 {
    project_pending()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_probe_sees_the_projection() {
        assert_eq!(session_probe(), 1);
    }

    #[test]
    fn unrelated_session_test() {
        assert_eq!(1, 1);
    }
}
";

    /// The Remote shape of #2410: a leaf crate (`rsid-remote-read`) whose
    /// callers live in rsid behind a facade module (`remote_read`) and in the
    /// session code.
    fn remote_workspace() -> Tree {
        tree(&[
            ("crates/rsid-remote-read/src/lib.rs", LEAF_LIB),
            ("crates/rsid-remote-read/src/projection.rs", LEAF_PROJECTION),
            ("crates/rsid/src/lib.rs", REMOTE_LIB),
            ("crates/rsid/src/remote_read/mod.rs", REMOTE_FACADE),
            ("crates/rsid/src/remote_read/owner.rs", REMOTE_OWNER),
            ("crates/rsid/src/session/mod.rs", "mod queries;\n"),
            ("crates/rsid/src/session/queries.rs", SESSION_QUERIES),
        ])
    }

    fn edited_remote_selection(
        path: &str,
        text: &str,
        old: &str,
        new: &str,
    ) -> UnfilteredSelection {
        let base = remote_workspace();
        let mut candidate = remote_workspace();
        candidate.insert(path.into(), text.replace(old, new));
        // The first line of the edit, indentation kept, locates the changed line.
        let line = line_of(text, old.lines().next().expect("a non-empty edit"));
        select_from(&[changed(path, &[line])], &base, &candidate).unwrap()
    }

    /// #2410: a production edit of the leaf selects its own tests AND the rsid
    /// tests that observe it (the facade owner and the session code): the leaf
    /// filter alone would miss them, so the leaf crate is part of the closure.
    #[test]
    fn a_leaf_production_edit_selects_its_tests_and_the_daemon_observers() {
        let selection = edited_remote_selection(
            "crates/rsid-remote-read/src/projection.rs",
            LEAF_PROJECTION,
            "    1\n}",
            "    2\n}",
        );
        assert_eq!(
            selected(&selection),
            BTreeSet::from([
                "projection::tests::projects_one_pending_item".to_owned(),
                "remote_read::owner::tests::owner_reads_the_projection".to_owned(),
                "session::queries::tests::session_probe_sees_the_projection".to_owned(),
            ])
        );
        // Only the leaf's own test is covered by the leaf's package run; the
        // shared rsid lib build keeps the daemon observers.
        assert_eq!(
            selection.leaf_tests,
            BTreeSet::from(["projection::tests::projects_one_pending_item".to_owned()])
        );
    }

    #[test]
    fn harness_leaf_selects_its_tests_and_retained_daemon_observers() {
        let mut base: Tree = remote_workspace()
            .into_iter()
            .map(|(path, text)| {
                (
                    path.replace("rsid-remote-read", "rsid-harness")
                        .replace("rsid/src/remote_read", "rsid/src/session/harness"),
                    text.replace("rsid_remote_read", "rsid_harness"),
                )
            })
            .collect();
        base.insert("crates/rsid/src/lib.rs".into(), "pub mod session;".into());
        base.insert(
            "crates/rsid/src/session/mod.rs".into(),
            "mod harness; mod queries;".into(),
        );
        let path = "crates/rsid-harness/src/projection.rs";
        let mut candidate = base.clone();
        candidate.insert(path.into(), LEAF_PROJECTION.replace("    1\n}", "    2\n}"));
        let selection = select_from(
            &[changed(path, &[line_of(LEAF_PROJECTION, "    1")])],
            &base,
            &candidate,
        )
        .unwrap();
        assert_eq!(
            selection.leaf_tests,
            BTreeSet::from(["projection::tests::projects_one_pending_item".to_owned()])
        );
        assert!(
            selected(&selection)
                .contains("session::harness::owner::tests::owner_reads_the_projection")
        );
    }

    /// #2616: the harness tools live in a nested leaf module (`tools::<tool>`) whose
    /// daemon observers keep the old `session::harness::tools` paths.
    #[test]
    fn nested_harness_tool_edit_selects_the_leaf_module_and_daemon_observers() {
        let mut base: Tree = remote_workspace()
            .into_iter()
            .filter(|(path, _)| !path.contains("rsid-remote-read"))
            .map(|(path, text)| {
                (
                    path.replace("rsid/src/remote_read", "rsid/src/session/harness"),
                    text.replace("rsid_remote_read", "rsid_harness::tools"),
                )
            })
            .collect();
        base.insert(
            "crates/rsid-harness/src/lib.rs".into(),
            "pub mod tools;".into(),
        );
        base.insert(
            "crates/rsid-harness/src/tools/mod.rs".into(),
            LEAF_LIB.into(),
        );
        base.insert(
            "crates/rsid-harness/src/tools/projection.rs".into(),
            LEAF_PROJECTION.into(),
        );
        base.insert("crates/rsid/src/lib.rs".into(), "pub mod session;".into());
        base.insert(
            "crates/rsid/src/session/mod.rs".into(),
            "mod harness; mod queries;".into(),
        );
        let path = "crates/rsid-harness/src/tools/projection.rs";
        let mut candidate = base.clone();
        candidate.insert(path.into(), LEAF_PROJECTION.replace("    1\n}", "    2\n}"));
        let selection = select_from(
            &[changed(path, &[line_of(LEAF_PROJECTION, "    1")])],
            &base,
            &candidate,
        )
        .unwrap();
        assert_eq!(
            selection.leaf_tests,
            BTreeSet::from(["tools::projection::tests::projects_one_pending_item".to_owned()])
        );
        assert!(
            selected(&selection)
                .contains("session::harness::owner::tests::owner_reads_the_projection")
        );
    }

    /// #2410: an edit of the leaf's test code selects that test and nothing in
    /// the daemon: no production symbol changed.
    #[test]
    fn a_leaf_test_only_edit_selects_only_the_leaf_test() {
        let selection = edited_remote_selection(
            "crates/rsid-remote-read/src/projection.rs",
            LEAF_PROJECTION,
            "assert_eq!(project_pending(), 1);",
            "assert_eq!(project_pending(), 1, \"one\");",
        );
        let only = BTreeSet::from(["projection::tests::projects_one_pending_item".to_owned()]);
        assert_eq!(selected(&selection), only);
        assert_eq!(selection.leaf_tests, only);
    }

    /// #2410: an edit of the retained owner selects the owner tests that name
    /// it and no leaf test: the leaf cannot depend on rsid.
    #[test]
    fn a_retained_owner_edit_selects_the_owner_tests_and_no_leaf_test() {
        let selection = edited_remote_selection(
            "crates/rsid/src/remote_read/owner.rs",
            REMOTE_OWNER,
            "    project_pending()\n",
            "    project_pending() + 0\n",
        );
        assert_eq!(
            selected(&selection),
            BTreeSet::from(["remote_read::owner::tests::owner_reads_the_projection".to_owned()])
        );
        assert!(selection.leaf_tests.is_empty());
    }

    /// #2410: the leaf's root and manifest keep every shard, like any rsid-family root.
    #[test]
    fn the_leaf_crate_root_and_manifest_keep_everything() {
        let fixture = remote_workspace();
        for path in [
            "crates/rsid-remote-read/src/lib.rs",
            "crates/rsid-remote-read/Cargo.toml",
        ] {
            assert_eq!(
                select_from(&[changed(path, &[1])], &fixture, &fixture),
                None,
                "{path}"
            );
        }
    }

    const PLANNING_LIB: &str = "pub mod planning;\n";
    const PLANNING_MOD: &str = "pub mod graph;\n";
    const PLANNING_GRAPH: &str = "\
pub fn plan_edges() -> u32 {
    1
}

pub fn unrelated_plan() -> u32 {
    9
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_one_edge() {
        assert_eq!(plan_edges(), 1);
    }

    #[test]
    fn plans_something_else() {
        assert_eq!(unrelated_plan(), 9);
    }
}
";
    const PLANNING_RSID_LIB: &str = "pub mod topology;\npub mod session;\n";
    const PLANNING_FACADE: &str = "\
mod executor;
pub(crate) use rsi_graph::planning::graph::*;
";
    const PLANNING_EXECUTOR: &str = "\
pub fn run_plan() -> u32 {
    plan_edges()
}

pub fn unrelated_run() -> u32 {
    2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executor_runs_the_planned_edges() {
        assert_eq!(run_plan(), 1);
    }

    #[test]
    fn executor_something_else() {
        assert_eq!(unrelated_run(), 2);
    }
}
";
    const PLANNING_RUNNER: &str = "\
pub fn runner_edges() -> u32 {
    plan_edges()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_runner_uses_the_planned_edges() {
        assert_eq!(runner_edges(), 1);
    }

    #[test]
    fn unrelated_runner_test() {
        assert_eq!(1, 1);
    }
}
";

    /// The planning shape of #2416: `rsi-graph::planning` (#2415) whose callers
    /// live in rsid behind the `topology` facade and in the session graph runner.
    fn planning_workspace() -> Tree {
        tree(&[
            ("crates/rsi-graph/src/lib.rs", PLANNING_LIB),
            ("crates/rsi-graph/src/planning/mod.rs", PLANNING_MOD),
            ("crates/rsi-graph/src/planning/graph.rs", PLANNING_GRAPH),
            ("crates/rsid/src/lib.rs", PLANNING_RSID_LIB),
            ("crates/rsid/src/topology/mod.rs", PLANNING_FACADE),
            ("crates/rsid/src/topology/executor.rs", PLANNING_EXECUTOR),
            ("crates/rsid/src/session/mod.rs", "mod graph_runner;\n"),
            ("crates/rsid/src/session/graph_runner.rs", PLANNING_RUNNER),
        ])
    }

    fn edited_planning_selection(
        path: &str,
        text: &str,
        old: &str,
        new: &str,
    ) -> UnfilteredSelection {
        let base = planning_workspace();
        let mut candidate = planning_workspace();
        candidate.insert(path.into(), text.replace(old, new));
        let line = line_of(text, old.lines().next().expect("a non-empty edit"));
        select_from(&[changed(path, &[line])], &base, &candidate).unwrap()
    }

    /// #2416: a moved production edit under `rsi-graph::planning` selects the
    /// planning test AND the rsid topology executor and graph runner tests that
    /// exercise the planner; the planning filter alone would miss them.
    #[test]
    fn a_planning_production_edit_selects_the_topology_and_graph_runner_observers() {
        let selection = edited_planning_selection(
            "crates/rsi-graph/src/planning/graph.rs",
            PLANNING_GRAPH,
            "    1\n}",
            "    2\n}",
        );
        assert_eq!(
            selected(&selection),
            BTreeSet::from([
                "planning::graph::tests::plans_one_edge".to_owned(),
                "session::graph_runner::tests::graph_runner_uses_the_planned_edges".to_owned(),
                "topology::executor::tests::executor_runs_the_planned_edges".to_owned(),
            ])
        );
        // Only the planning test is covered by the rsi-graph package run.
        assert_eq!(
            selection.leaf_tests,
            BTreeSet::from(["planning::graph::tests::plans_one_edge".to_owned()])
        );
    }

    /// #2416: a moved test-only edit selects that planning test and nothing in
    /// the daemon: no production symbol changed.
    #[test]
    fn a_planning_test_only_edit_selects_only_the_planning_test() {
        let selection = edited_planning_selection(
            "crates/rsi-graph/src/planning/graph.rs",
            PLANNING_GRAPH,
            "assert_eq!(plan_edges(), 1);",
            "assert_eq!(plan_edges(), 1, \"one\");",
        );
        let only = BTreeSet::from(["planning::graph::tests::plans_one_edge".to_owned()]);
        assert_eq!(selected(&selection), only);
        assert_eq!(selection.leaf_tests, only);
    }

    /// #2416: a retained daemon-owner edit selects the executor tests that name
    /// it and no planning test: rsi-graph cannot depend on rsid.
    #[test]
    fn a_retained_daemon_owner_edit_selects_the_owner_tests_and_no_planning_test() {
        let selection = edited_planning_selection(
            "crates/rsid/src/topology/executor.rs",
            PLANNING_EXECUTOR,
            "    plan_edges()\n",
            "    plan_edges() + 0\n",
        );
        assert_eq!(
            selected(&selection),
            BTreeSet::from([
                "topology::executor::tests::executor_runs_the_planned_edges".to_owned()
            ])
        );
        assert!(selection.leaf_tests.is_empty());
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

    const THREAD_LOCAL_SOURCE: &str = "\
use std::cell::RefCell;

thread_local! {
    static STATE: RefCell<u32> = const { RefCell::new(0) };
    pub static SHARED: RefCell<u32> = RefCell::new(1);
}

pub fn read_state() -> u32 {
    STATE.with(|state| *state.borrow())
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_the_state() {
        assert_eq!(super::read_state(), 0);
    }
}
";

    /// #2086: a `thread_local!` invocation declares exactly the statics in its
    /// body, so a change to it selects the tests naming them, not every test.
    #[test]
    fn a_thread_local_change_selects_only_the_tests_naming_its_statics() {
        let base = macro_workspace(THREAD_LOCAL_SOURCE);
        let candidate_text =
            THREAD_LOCAL_SOURCE.replace("const { RefCell::new(0) }", "const { RefCell::new(5) }");
        let candidate = macro_workspace(&candidate_text);
        let line = line_of(&candidate_text, "static STATE");
        let selection = select_from(
            &[changed("crates/rsid/src/fields.rs", &[line])],
            &base,
            &candidate,
        )
        .unwrap();
        assert_eq!(
            selected(&selection),
            BTreeSet::from(["fields::tests::reads_the_state".to_owned()])
        );
    }

    /// #2086: `tokio::task_local!` declares the statics of its body the same
    /// way, and a built-in macro such as `env!` in an `include!` is not a
    /// reference to a function of the same name.
    #[test]
    fn a_task_local_change_selects_the_tests_naming_its_static() {
        let text = "\
tokio::task_local! {\n    static STEP: u32;\n}\n\
pub fn note() -> u32 {\n    STEP.try_with(|step| *step).unwrap_or(0)\n}\n\
#[cfg(test)]\nmod tests {\n    #[test]\n    fn notes_the_step() {\n        assert_eq!(super::note(), 0);\n    }\n}\n";
        let base = macro_workspace(text);
        let candidate_text = text.replace("static STEP: u32;", "static STEP: u64;");
        let candidate = macro_workspace(&candidate_text);
        let line = line_of(&candidate_text, "static STEP");
        let selection = select_from(
            &[changed("crates/rsid/src/fields.rs", &[line])],
            &base,
            &candidate,
        )
        .unwrap();
        assert_eq!(
            selected(&selection),
            BTreeSet::from(["fields::tests::notes_the_step".to_owned()])
        );
    }

    #[test]
    fn a_builtin_macro_in_an_include_is_not_a_reference_to_a_same_named_function() {
        let mut base = macro_workspace(
            "pub fn env() -> u32 { 1 }\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn reads_env() {\n        assert_eq!(super::env(), 1);\n    }\n}\n",
        );
        base.insert(
            "crates/rsid/src/other.rs".into(),
            "include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));\n".into(),
        );
        let mut candidate = base.clone();
        let changed_text = "pub fn env() -> u32 { 2 }\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn reads_env() {\n        assert_eq!(super::env(), 1);\n    }\n}\n";
        candidate.insert("crates/rsid/src/fields.rs".into(), changed_text.into());
        let selection = select_from(
            &[changed("crates/rsid/src/fields.rs", &[1])],
            &base,
            &candidate,
        )
        .unwrap();
        assert_eq!(
            selected(&selection),
            BTreeSet::from(["fields::tests::reads_env".to_owned()])
        );
    }

    /// #2086: a name a `thread_local!` declares is global when it is `pub`:
    /// another module's test that names it is selected.
    #[test]
    fn a_public_thread_local_static_reaches_tests_in_other_modules() {
        let mut base = macro_workspace(THREAD_LOCAL_SOURCE);
        let user = "#[cfg(test)]\nmod tests {\n    #[test]\n    fn uses_shared() {\n        crate::fields::SHARED.with(|_| ());\n    }\n}\n";
        base.insert("crates/rsid/src/other.rs".into(), user.into());
        let mut candidate = base.clone();
        let candidate_text = THREAD_LOCAL_SOURCE.replace("RefCell::new(1)", "RefCell::new(2)");
        let line = line_of(&candidate_text, "pub static SHARED");
        candidate.insert("crates/rsid/src/fields.rs".into(), candidate_text);
        let selection = select_from(
            &[changed("crates/rsid/src/fields.rs", &[line])],
            &base,
            &candidate,
        )
        .unwrap();
        // The invocation is one item: both its statics are affected.
        assert_eq!(
            selected(&selection),
            BTreeSet::from([
                "fields::tests::reads_the_state".to_owned(),
                "other::tests::uses_shared".to_owned()
            ])
        );
    }

    /// #2086: a crate-local macro named like a well-known one decides its own
    /// expansion, and a body whose names cannot be read stays opaque.
    #[test]
    fn a_shadowed_or_unreadable_thread_local_still_runs_everything() {
        let variants = [
            (
                "macro_rules! thread_local {\n    ($name:ident) => {\n        pub fn $name() {}\n    };\n}\nthread_local! { alpha }\n",
                "thread_local! { alpha }",
            ),
            (
                "thread_local! {\n    static STATE: u32 = generate_default!();\n}\n",
                "static STATE",
            ),
            ("thread_local! {\n    fn_like { 1 }\n}\n", "fn_like"),
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
                    assert!(reason.contains("thread_local!"), "{reason}");
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

    /// #1306: a bin or integration test target whose code names a changed
    /// library item runs with it, through a helper of its own too; a target
    /// that names nothing changed, or declares no test, does not.
    #[test]
    fn a_target_that_names_a_changed_library_item_runs_with_it() {
        let mut base = workspace();
        for (path, text) in [
            (
                "crates/rsid/src/bin/lander.rs",
                "fn main() {}\n#[test]\nfn lands() { assert_eq!(rsid::other::helper(), 1); }\n",
            ),
            (
                "crates/rsid/tests/through_helper.rs",
                "mod support;\n#[test]\nfn it() { support::call(); }\n",
            ),
            (
                "crates/rsid/tests/support/mod.rs",
                "pub fn call() { let _ = rsid::other::helper(); }\n",
            ),
            (
                "crates/rsid/tests/unrelated.rs",
                "#[test]\nfn it() { let _ = rsid::topology::elsewhere(); }\n",
            ),
            (
                "crates/rsid/src/bin/untested.rs",
                "fn main() { let _ = rsid::other::helper(); }\n",
            ),
        ] {
            base.insert(path.into(), text.into());
        }
        let mut candidate = base.clone();
        let path = "crates/rsid/src/other.rs";
        candidate.insert(path.into(), OTHER.replace("{ 1 }", "{ 2 }"));
        let selection = select_from(&[changed(path, &[1])], &base, &candidate).unwrap();
        assert_eq!(
            selected(&selection),
            BTreeSet::from(["other::tests::other_test".to_owned()])
        );
        assert_eq!(
            selection.targets,
            BTreeSet::from([
                ("rsid".to_owned(), "bin:lander".to_owned()),
                ("rsid".to_owned(), "test:through_helper".to_owned()),
            ])
        );
    }

    /// A library change that no library test names but a target does is a
    /// target-only selection, not "every library test".
    #[test]
    fn a_library_change_only_a_target_names_selects_just_that_target() {
        let mut base = workspace();
        base.insert(
            "crates/rsid/src/lib.rs".into(),
            format!("{RSID_LIB}pub mod lonely;\n"),
        );
        base.insert(
            "crates/rsid/src/lonely.rs".into(),
            "pub fn solo() -> u32 { 1 }\n".into(),
        );
        base.insert(
            "crates/rsid/tests/solo.rs".into(),
            "#[test]\nfn it() { assert_eq!(rsid::lonely::solo(), 1); }\n".into(),
        );
        let mut candidate = base.clone();
        candidate.insert(
            "crates/rsid/src/lonely.rs".into(),
            "pub fn solo() -> u32 { 2 }\n".into(),
        );
        let selection = select_from(
            &[changed("crates/rsid/src/lonely.rs", &[1])],
            &base,
            &candidate,
        )
        .unwrap();
        assert_eq!(selection.lib, LibTests::Tests(BTreeSet::new()));
        assert_eq!(
            selection.targets,
            BTreeSet::from([("rsid".to_owned(), "test:solo".to_owned())])
        );
    }

    /// An integration test that scans the crate's sources observes any source
    /// change without naming it.
    #[test]
    fn an_integration_source_scanner_runs_on_any_source_change() {
        let mut base = workspace();
        base.insert(
            "crates/rsid/tests/scan.rs".into(),
            "#[test]\nfn no_sql() {\n    let root = std::path::PathBuf::from(env!(\"CARGO_MANIFEST_DIR\"));\n    assert!(root.join(\"src\").exists());\n}\n".into(),
        );
        let mut candidate = base.clone();
        candidate.insert(
            "crates/rsid/src/other.rs".into(),
            OTHER.replace("{ 1 }", "{ 2 }"),
        );
        let selection = select_from(
            &[changed("crates/rsid/src/other.rs", &[1])],
            &base,
            &candidate,
        )
        .unwrap();
        assert!(
            selection
                .targets
                .contains(&("rsid".to_owned(), "test:scan".to_owned())),
            "{selection:?}"
        );
    }

    #[test]
    fn a_data_file_selects_a_reader_bin_with_its_own_package() {
        let mut base = workspace();
        base.insert(
            "crates/rsid/src/bin/tool.rs".into(),
            "fn main() {}\n#[test]\nfn reads() { let _ = include_str!(\"../../../rsid-store/AGENTS.md\"); }\n"
                .into(),
        );
        for changed_file in [
            "crates/rsid-store/AGENTS.md",
            "crates/rsid-store/docs/AGENTS.md",
        ] {
            let selection = select_from(&[changed(changed_file, &[1])], &base, &base).unwrap();
            assert_eq!(
                selection.targets,
                BTreeSet::from([("rsid".to_owned(), "bin:tool".to_owned())]),
                "{changed_file}"
            );
        }
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

    fn token_scanner_fixture() -> Tree {
        tree(&[
            (
                SCANNED_TOKENS,
                include_str!("../../../../../scripts/scanned-tokens.json"),
            ),
            ("crates/rsid/src/lib.rs", "pub mod reader;"),
            (
                "crates/rsid-store/src/lib.rs",
                "pub mod store; pub mod reader;",
            ),
            ("crates/rsid-remote-read/src/lib.rs", "pub mod reader;"),
            ("crates/rsi-graph/src/lib.rs", ""),
            (
                "crates/rsid-store/src/store/mod.rs",
                "pub mod cold_archive;",
            ),
            (
                "crates/rsid-store/src/store/cold_archive/mod.rs",
                "#[cfg(test)] mod read_classes; #[cfg(test)] mod read_tests;",
            ),
            (
                "crates/rsid-store/src/store/cold_archive/read_classes.rs",
                "#[test] fn cold_archive_reader_classification() {}",
            ),
            (
                "crates/rsid-store/src/store/cold_archive/read_tests.rs",
                "#[test] fn cold_archive_reader_invariance() {}",
            ),
        ])
    }

    fn token_scanner_names() -> BTreeSet<String> {
        BTreeSet::from([
            "store::cold_archive::read_classes::cold_archive_reader_classification".into(),
            "store::cold_archive::read_tests::cold_archive_reader_invariance".into(),
        ])
    }

    #[test]
    fn registered_token_scanners_observe_new_readers_in_all_three_crates() {
        for package in ["rsid", "rsid-store", "rsid-remote-read"] {
            let base = token_scanner_fixture();
            let mut candidate = base.clone();
            let path = format!("crates/{package}/src/reader.rs");
            candidate.insert(
                path.clone(),
                "pub fn read() { query(\"SELECT id FROM conversation_events\"); }".into(),
            );
            let diff = FileDiff {
                new_path: Some(path),
                new_lines: vec![1],
                ..FileDiff::default()
            };
            let (selection, explanation) = select_from_explained(
                &[diff],
                &base,
                &candidate,
                Budget {
                    tests: 1,
                    targets: 1,
                },
            )
            .unwrap();
            assert_eq!(selected(&selection), token_scanner_names());
            for name in token_scanner_names() {
                assert_eq!(explanation.test_distances[&name], 0);
            }
        }
    }

    #[test]
    fn registered_token_scanners_observe_predicate_edits_and_old_side_removals() {
        let path = "crates/rsid-store/src/reader.rs";
        let mut base = token_scanner_fixture();
        base.insert(
            path.into(),
            "pub fn read() {\n query(\"conversation_events\");\n gate(1);\n}\n".into(),
        );
        let mut candidate = base.clone();
        candidate.insert(
            path.into(),
            "pub fn read() {\n query(\"conversation_events\");\n gate(2);\n}\n".into(),
        );
        assert_eq!(
            selected(&select_from(&[changed(path, &[3])], &base, &candidate).unwrap()),
            token_scanner_names()
        );
        candidate.insert(
            path.into(),
            "pub fn read() { query(\"another_table\"); }".into(),
        );
        let diff = FileDiff {
            old_path: Some(path.into()),
            new_path: Some(path.into()),
            old_lines: vec![2],
            new_lines: vec![1],
            binary: false,
        };
        assert_eq!(
            selected(&select_from(&[diff], &base, &candidate).unwrap()),
            token_scanner_names()
        );
    }

    #[test]
    fn future_scanned_tokens_need_only_a_registry_entry() {
        let mut base = token_scanner_fixture();
        base.insert(
            SCANNED_TOKENS.into(),
            base[SCANNED_TOKENS].replace("conversation_events", "future_table"),
        );
        let mut candidate = base.clone();
        let path = "crates/rsid/src/reader.rs";
        candidate.insert(
            path.into(),
            "pub fn read() { query(\"future_table\"); }".into(),
        );
        assert_eq!(
            selected(&select_from(&[changed(path, &[1])], &base, &candidate).unwrap()),
            token_scanner_names()
        );
    }

    #[test]
    fn unrelated_edits_keep_their_ordinary_observers() {
        let mut base = token_scanner_fixture();
        let path = "crates/rsid/src/reader.rs";
        base.insert(
            path.into(),
            "pub fn read() {}\n#[test] fn observes_read() { read(); }".into(),
        );
        let mut candidate = base.clone();
        candidate.insert(
            path.into(),
            "pub fn read() { changed(); }\n#[test] fn observes_read() { read(); }".into(),
        );
        assert_eq!(
            selected(&select_from(&[changed(path, &[1])], &base, &candidate).unwrap()),
            BTreeSet::from(["reader::observes_read".into()])
        );
    }

    #[test]
    fn token_file_comment_and_test_only_edits_do_not_seed_scanners() {
        let path = "crates/rsid/src/reader.rs";
        let mut base = token_scanner_fixture();
        base.insert(path.into(), "pub fn read() { query(\"conversation_events\"); }\n// before\n#[test] fn observes_read() { read(); }".into());
        let mut candidate = base.clone();
        candidate.insert(path.into(), "pub fn read() { query(\"conversation_events\"); }\n// after\n#[test] fn observes_read() { read(); changed(); }".into());
        let program = Program::build(&candidate);
        let pins = program
            .scanned_token_seeds(
                &[changed(path, &[2, 3])],
                &base,
                &candidate,
                &Program::build(&base),
            )
            .unwrap();
        assert!(pins.is_empty());
        let selection = select_from(&[changed(path, &[3])], &base, &candidate).unwrap();
        assert_eq!(
            selected(&selection),
            BTreeSet::from(["reader::observes_read".into()])
        );
    }

    #[test]
    fn a_registry_edit_seeds_its_observers_without_a_rust_edit() {
        let base = token_scanner_fixture();
        let selection = select_from(&[changed(SCANNED_TOKENS, &[1])], &base, &base).unwrap();
        assert_eq!(selected(&selection), token_scanner_names());
    }

    #[test]
    fn unresolved_token_scanner_retains_every_library_test() {
        let base = token_scanner_fixture();
        let mut candidate = base.clone();
        candidate.insert(
            "crates/rsid-store/src/store/cold_archive/read_classes.rs".into(),
            "".into(),
        );
        let path = "crates/rsid/src/reader.rs";
        candidate.insert(
            path.into(),
            "pub fn read() { query(\"conversation_events\"); }".into(),
        );
        let selection = select_from(&[changed(path, &[1])], &base, &candidate).unwrap();
        assert!(
            matches!(selection.lib, LibTests::Everything(reason) if reason.contains("scanned-token test does not resolve"))
        );
    }

    /// A runnable target test that resolves the crate root and scans source
    /// files is selected for registry and arbitrary production source edits.
    #[test]
    fn target_source_scanners_observe_registry_and_spawn_changes() {
        // The fixture reads the crate root at compile time. The variable name
        // is spliced in so this source holds no literal `env!("...")` of it,
        // which rsi-common's `manifest_dir_guard` rejects in workspace code.
        let validator = r#"
fn fixture_root() -> std::path::PathBuf {
    let _ = "crates/rsid/src";
    let _ = "../rsid-store/src";
    std::path::PathBuf::from(env!("MANIFEST_VAR"))
}
#[test]
fn validates_registry() {
    let _ = fixture_root().join("crates/rsid-store/src/model_control/registry.rs");
}
fn main() {}
"#
        .replace("MANIFEST_VAR", "CARGO_MANIFEST_DIR");
        let fixture = tree(&[
            ("crates/rsid/src/lib.rs", "pub fn marker() {}\n"),
            ("crates/rsid-store/src/lib.rs", "pub fn store() {}\n"),
            ("crates/rsid/src/process.rs", "pub fn process() {}\n"),
            (
                "crates/rsid/src/bin/rsi-model-control-validate.rs",
                &validator,
            ),
        ]);
        for (path, source) in [
            (
                "crates/rsid-store/src/model_control/registry.rs",
                "pub fn registry() { changed(); }\n",
            ),
            (
                "crates/rsid/src/process.rs",
                "pub fn process() { std::process::Command::new(\"git\"); }\n",
            ),
        ] {
            let mut candidate = fixture.clone();
            candidate.insert(path.to_owned(), source.to_owned());
            let selection = select_from(&[changed(path, &[1])], &fixture, &candidate).unwrap();
            assert_eq!(
                selection.targets,
                BTreeSet::from([(
                    "rsid".to_owned(),
                    "bin:rsi-model-control-validate".to_owned()
                )]),
                "{path}"
            );
        }
    }

    /// #1741: `rsi_common::manifest_dir!()` resolves the crate root like
    /// `rsi_common::manifest_dir!()`, so a scanner using it still observes
    /// every source change.
    #[test]
    fn manifest_dir_macro_scanners_observe_changes_they_do_not_name() {
        let mut base = workspace();
        base.insert(
            "crates/rsid/src/lib.rs".into(),
            format!("{RSID_LIB}#[cfg(test)]\nmod scan;\n"),
        );
        base.insert(
            "crates/rsid/src/scan.rs".into(),
            "fn root() -> std::path::PathBuf {\n    std::path::PathBuf::from(rsi_common::manifest_dir!())\n}\n\n#[test]\nfn no_sql_outside_the_store() {\n    assert!(root().join(\"src\").exists());\n}\n".into(),
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
            BTreeSet::from(["scan::no_sql_outside_the_store".to_owned()])
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
        let root = Path::new(rsi_common::manifest_dir!()).join("../..");
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

    // -----------------------------------------------------------------------
    // #2512: proven test-only diffs

    const LIB: &str = "crates/rsid/src/lib.rs";
    const ALPHA: &str = "crates/rsid/src/alpha.rs";
    const CASES: &str = "crates/rsid/src/alpha/cases.rs";
    const LIB_TEXT: &str = "pub mod alpha;\n";
    const ALPHA_TEXT: &str = "pub fn live() -> u8 {\n    1\n}\n\n#[cfg(test)]\nmod cases;\n";
    const CASES_TEXT: &str =
        "use super::*;\n#[test]\nfn live_is_one() {\n    assert_eq!(live(), 1);\n}\n";

    fn proof(
        paths: &[&str],
        base: &[(&str, &str)],
        candidate: &[(&str, &str)],
    ) -> Option<TestOnlyDiff> {
        let paths: Vec<String> = paths.iter().map(|path| (*path).to_owned()).collect();
        test_only_from(&paths, &tree(base), &tree(candidate))
    }

    fn rsid_files<'a>(extra: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
        let mut files = vec![(LIB, LIB_TEXT), (ALPHA, ALPHA_TEXT), (CASES, CASES_TEXT)];
        for (path, text) in extra {
            files.retain(|(known, _)| known != path);
            files.push((path, text));
        }
        files
    }

    #[test]
    fn a_file_mounted_only_under_cfg_test_is_a_test_only_edit() {
        let edited = format!("{CASES_TEXT}#[test]\nfn more() {{}}\n");
        let proved = proof(
            &[CASES],
            &rsid_files(&[]),
            &rsid_files(&[(CASES, edited.as_str())]),
        )
        .unwrap();
        assert_eq!(proved.packages, BTreeSet::from(["rsid".to_owned()]));
        assert_eq!(
            proved.targets,
            BTreeSet::from([TestTarget::Lib("rsid".to_owned())])
        );
    }

    #[test]
    fn test_regions_of_a_production_file_and_comment_edits_are_test_only() {
        let edited = ALPHA_TEXT.replace(
            "#[cfg(test)]\nmod cases;\n",
            "// the cases\n#[cfg(test)]\nmod cases;\n#[cfg(test)]\nmod more {\n    #[test]\n    fn t() {}\n}\n#[cfg(all(test, unix))]\nfn helper() {}\n#[test]\nfn bare() {}\n",
        );
        assert!(
            proof(
                &[ALPHA],
                &rsid_files(&[]),
                &rsid_files(&[(ALPHA, edited.as_str())])
            )
            .is_some()
        );
    }

    #[test]
    fn production_cfg_boundary_mixed_and_doc_edits_keep_the_checks() {
        let base = rsid_files(&[]);
        let edit = |old: &str, new: &str| ALPHA_TEXT.replace(old, new);
        for (name, edited) in [
            ("production body", edit("    1\n", "    2\n")),
            ("cfg(test) dropped", edit("#[cfg(test)]\n", "")),
            (
                "cfg(test) added to production",
                edit("pub fn live", "#[cfg(test)]\npub fn live"),
            ),
            (
                "test seam",
                format!(
                    "{ALPHA_TEXT}#[cfg(any(test, feature = \"test-seam\"))]\npub fn seam() {{}}\n"
                ),
            ),
            ("doc comment", edit("pub fn live", "/// Live.\npub fn live")),
            (
                "new production item beside a test edit",
                format!("{ALPHA_TEXT}pub fn extra() {{}}\n"),
            ),
            (
                "cfg(not(test))",
                edit("pub fn live", "#[cfg(not(test))]\npub fn live"),
            ),
        ] {
            assert_eq!(
                proof(&[ALPHA], &base, &rsid_files(&[(ALPHA, edited.as_str())])),
                None,
                "{name}"
            );
        }
        // One production file keeps the checks even when the rest is test-only.
        let edited_cases = format!("{CASES_TEXT}#[test]\nfn more() {{}}\n");
        let edited_alpha = edit("    1\n", "    2\n");
        assert_eq!(
            proof(
                &[ALPHA, CASES],
                &base,
                &rsid_files(&[
                    (ALPHA, edited_alpha.as_str()),
                    (CASES, edited_cases.as_str())
                ])
            ),
            None
        );
    }

    #[test]
    fn external_inputs_manifests_and_unmounted_sources_keep_the_checks() {
        let edited = format!("{CASES_TEXT}#[test]\nfn more() {{}}\n");
        let candidate = rsid_files(&[(CASES, edited.as_str())]);
        // The test file is also compiled in as text.
        let reader = "pub const SRC: &str = include_str!(\"alpha/cases.rs\");\n";
        let mut base = rsid_files(&[]);
        let mut changed = candidate.clone();
        base.push(("crates/rsid/src/reader.rs", reader));
        changed.push(("crates/rsid/src/reader.rs", reader));
        let lib = "pub mod alpha;\npub mod reader;\n";
        base.retain(|(path, _)| *path != LIB);
        changed.retain(|(path, _)| *path != LIB);
        base.push((LIB, lib));
        changed.push((LIB, lib));
        assert_eq!(proof(&[CASES], &base, &changed), None);
        // A path outside every mounted source, a manifest, a build script, a
        // doc, another package, and a file nothing mounts.
        for path in [
            "crates/rsid/Cargo.toml",
            "crates/rsid/build.rs",
            "docs/agents/reference.md",
            "crates/rsi/src/main.rs",
            "crates/rsid/src/orphan.rs",
        ] {
            assert_eq!(
                proof(&[CASES, path], &rsid_files(&[]), &candidate),
                None,
                "{path}"
            );
        }
        assert_eq!(proof(&[], &rsid_files(&[]), &candidate), None);
    }

    #[test]
    fn added_and_deleted_test_files_are_test_only_when_their_mount_is() {
        let lib_with = "pub mod alpha;\n#[cfg(test)]\nmod extra_cases;\n";
        let extra = "crates/rsid/src/extra_cases.rs";
        let base = rsid_files(&[]);
        let candidate = rsid_files(&[(LIB, lib_with), (extra, "#[test]\nfn t() {}\n")]);
        let added = proof(&[LIB, extra], &base, &candidate).unwrap();
        assert!(added.targets.contains(&TestTarget::Lib("rsid".to_owned())));
        assert!(proof(&[LIB, extra], &candidate, &base).is_some());
        // The same file mounted without cfg(test) is production code.
        let plain = rsid_files(&[
            (LIB, "pub mod alpha;\nmod extra_cases;\n"),
            (extra, "#[test]\nfn t() {}\n"),
        ]);
        assert_eq!(proof(&[LIB, extra], &base, &plain), None);
    }

    #[test]
    fn integration_and_bin_targets_edit_test_only_when_they_declare_tests() {
        let it = "crates/rsid/tests/session_it.rs";
        let quiet = "crates/rsid/tests/quiet.rs";
        let bin = "crates/rsid/src/bin/tool.rs";
        let base = |extra: &[(&'static str, &'static str)]| {
            let mut files = rsid_files(&[]);
            files.extend_from_slice(extra);
            files
        };
        let before = base(&[
            (it, "#[test]\nfn a() {}\n"),
            (quiet, "fn helper() {}\n"),
            (
                bin,
                "fn main() {}\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {}\n}\n",
            ),
        ]);
        let after = base(&[
            (it, "#[test]\nfn a() { assert!(true); }\n"),
            (quiet, "fn helper() { let _ = 1; }\n"),
            (
                bin,
                "fn main() {}\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() { assert!(true); }\n}\n",
            ),
        ]);
        let integration = proof(&[it], &before, &after).unwrap();
        assert_eq!(
            integration.targets,
            BTreeSet::from([TestTarget::Test("rsid".to_owned(), "session_it".to_owned())])
        );
        let binary = proof(&[bin], &before, &after).unwrap();
        assert_eq!(
            binary.targets,
            BTreeSet::from([TestTarget::Bin("rsid".to_owned(), "tool".to_owned())])
        );
        // A target that declares no test is never run, so nothing compiles it.
        assert_eq!(proof(&[quiet], &before, &after), None);
        // A production edit of a bin keeps the checks.
        let production = base(&[
            (it, "#[test]\nfn a() {}\n"),
            (quiet, "fn helper() {}\n"),
            (
                bin,
                "fn main() { let _ = 2; }\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {}\n}\n",
            ),
        ]);
        assert_eq!(proof(&[bin], &before, &production), None);
    }

    /// #2545: a bin target exists because its root file does, so adding or
    /// deleting one is never test-only, even with no `fn main` to fingerprint.
    #[test]
    fn an_added_or_deleted_bin_target_root_is_never_test_only() {
        let base = rsid_files(&[]);
        for path in [
            "crates/rsid/src/bin/x.rs",
            "crates/rsid/src/bin/x/main.rs",
            "crates/rsid/src/main.rs",
        ] {
            let added = rsid_files(&[(
                path,
                "#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {}\n}\n",
            )]);
            assert_eq!(proof(&[path], &base, &added), None, "{path}");
            assert_eq!(proof(&[path], &added, &base), None, "{path}");
        }
        assert!(is_bin_target_root("src/bin/x.rs"));
        assert!(is_bin_target_root("src/bin/x/main.rs"));
        assert!(!is_bin_target_root("src/bin/x/helper.rs"));
        assert!(!is_bin_target_root("src/lib.rs"));
    }
}
