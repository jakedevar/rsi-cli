//! #1940: which checks of a gate a rolling advance can invalidate.
//!
//! A queue gate takes tens of minutes, and rolling moves under it whenever any
//! worker lands directly. Re-running the whole gate on every moved tip stalls
//! every landing behind it, though most advances touch nothing the gate
//! selected (a doc, a script, a command file, another crate). The lander
//! gathers the facts of the advance (its changed paths, the workspace
//! packages, the packages whose tests read a changed file, the Python suites
//! it selects) and this module turns them into a [`Verdict`]: the test groups
//! and compile checks to run again. Everything else keeps the candidate's
//! earlier result, because the advance provably selects none of it.
//!
//! The rule is the one a landing's own filters follow. A change to a crate
//! selects that crate's tests, and its dependents' tests too (a dependent's
//! test can observe it; the candidate's combination with it was never
//! tested). A changed file outside every crate selects only the tests that
//! read it, which `scripts/check-touched-shards` names. A path that overlaps
//! the candidate's own diff, a workspace manifest or toolchain change, a path
//! the classification cannot place, or an unavailable selector re-runs
//! everything: found nothing never means run nothing.
//!
//! #1957: the rsid family's tests are finer than a target. When the advance
//! reaches the family only through the family's own crates and the
//! selector's family filters ([`Verdict::family_closed`]), the lander narrows
//! the family's re-run to the library tests and targets the advance can
//! observe (the call-graph closure of its diff plus those filters).

use rsi_codegraph::impact::is_rsid_shard_package;
use std::collections::BTreeSet;

/// A set of checks: none, the named ones, or all of them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Scope {
    Nothing,
    /// Test targets (`<package>:lib`, `<package>:bin:NAME`, ...; `<package>:*`
    /// is every target of the package) or package names, per use.
    Only(BTreeSet<String>),
    Everything,
}

impl Scope {
    fn from_set(set: BTreeSet<String>) -> Self {
        if set.is_empty() {
            Self::Nothing
        } else {
            Self::Only(set)
        }
    }

    /// Whether the test target `key` runs again. A command with no key
    /// (`None`) cannot be shown independent of the advance: it runs unless
    /// nothing runs.
    pub(crate) fn runs_test(&self, key: Option<&str>) -> bool {
        match (self, key) {
            (Self::Nothing, _) => false,
            (Self::Everything, _) | (Self::Only(_), None) => true,
            (Self::Only(keys), Some(key)) => {
                keys.contains(key)
                    || key
                        .split_once(':')
                        .is_some_and(|(package, _)| keys.contains(&format!("{package}:*")))
            }
        }
    }

    /// Whether the compile check of `package` (`None`: an unrecognized
    /// command) runs again.
    pub(crate) fn runs_check(&self, package: Option<&str>) -> bool {
        match (self, package) {
            (Self::Nothing, _) => false,
            (Self::Everything, _) | (Self::Only(_), None) => true,
            (Self::Only(packages), Some(package)) => packages.contains(package),
        }
    }

    pub(crate) fn is_nothing(&self) -> bool {
        matches!(self, Self::Nothing)
    }

    /// Whether any test target of `family` runs again.
    pub(crate) fn selects_family(&self, family: &str) -> bool {
        match self {
            Self::Nothing => false,
            Self::Everything => true,
            Self::Only(keys) => keys
                .iter()
                .any(|key| key.split_once(':').is_some_and(|(name, _)| name == family)),
        }
    }
}

/// What a rolling advance invalidates of the candidate's earlier gate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Verdict {
    /// A short stable token for the evidence (`stale_reason=`): `disjoint`,
    /// `overlap:<targets>` or why everything runs again.
    pub(crate) reason: String,
    /// Test targets that run again.
    pub(crate) tests: Scope,
    /// Packages whose `cargo check` runs again.
    pub(crate) checks: Scope,
    /// Every Python suite runs again (rather than only the ones the advance
    /// selects).
    pub(crate) python_everything: bool,
    /// #1957: the advance reaches the rsid family's tests only through the
    /// family's own crates and [`Self::family_filters`], so the closure of its
    /// family diff plus those filters names every family test it can observe.
    /// False when a crate the family depends on changed, or for everything.
    pub(crate) family_closed: bool,
    /// The selector's filters on the rsid family packages.
    pub(crate) family_filters: Vec<String>,
}

impl Verdict {
    pub(crate) fn everything(reason: &str) -> Self {
        Self {
            reason: reason.to_owned(),
            tests: Scope::Everything,
            checks: Scope::Everything,
            python_everything: true,
            family_closed: false,
            family_filters: Vec::new(),
        }
    }

    /// Whether the advance leaves every earlier test result valid. A compile
    /// check or Python suite may still need to run.
    pub(crate) fn reuses_every_test(&self) -> bool {
        self.tests.is_nothing()
    }
}

/// The facts of one advance, gathered by the lander.
pub(crate) struct Advance<'a> {
    /// Paths the advance changed (`fetched_tip..current`).
    pub(crate) paths: &'a BTreeSet<String>,
    /// Paths of the candidate's own diff.
    pub(crate) candidate_paths: &'a BTreeSet<String>,
    /// Workspace package names; `crates/<name>` holds package `<name>`.
    pub(crate) packages: &'a BTreeSet<String>,
    /// The packages that depend on any of the given ones, transitively
    /// (`None`: the workspace graph could not be read).
    pub(crate) dependents: &'a dyn Fn(&[String]) -> Option<Vec<String>>,
    /// The `PACKAGE=FILTER` landing filters the selector suggests for the
    /// advance (`None`: the selector did not run).
    pub(crate) selected_filters: Option<&'a [String]>,
}

/// The test group of a package: the rsid library tests of `rsid` and
/// `rsid-store` run as one build, so they are one family.
pub(crate) fn family(package: &str) -> &str {
    if is_rsid_shard_package(package) {
        "rsid"
    } else {
        package
    }
}

/// The test target a `PACKAGE=FILTER` landing filter runs: `<family>:lib`,
/// `<family>:bin:NAME` or `<family>:test:NAME`.
pub(crate) fn filter_target(filter: &str) -> Option<String> {
    let (package, value) = filter.split_once('=')?;
    let family = family(package);
    for (prefix, kind) in [("bin:", "bin"), ("test:", "test")] {
        if let Some(rest) = value.strip_prefix(prefix) {
            let name = rest.split(':').next().filter(|name| !name.is_empty())?;
            return Some(format!("{family}:{kind}:{name}"));
        }
    }
    Some(format!("{family}:lib"))
}

enum PathKind<'a> {
    /// A file of the workspace package `crates/<name>`.
    Crate(&'a str),
    /// A manifest, lockfile or toolchain file every crate builds against.
    Workspace,
    /// A Rust file outside every crate directory.
    RustOutsideCrates,
    /// Anything else: a doc, a script, a command file, data.
    Other,
}

fn classify_path(path: &str) -> PathKind<'_> {
    let name = path.rsplit('/').next().unwrap_or(path);
    if let Some(rest) = path.strip_prefix("crates/") {
        return match rest.split_once('/') {
            Some((package, _)) if !package.is_empty() => PathKind::Crate(package),
            _ => PathKind::Workspace,
        };
    }
    if name == "Cargo.lock"
        || name == "Cargo.toml"
        || name == "rust-toolchain.toml"
        || name == "rust-toolchain"
        || path.starts_with(".cargo/")
    {
        return PathKind::Workspace;
    }
    if path.ends_with(".rs") {
        return PathKind::RustOutsideCrates;
    }
    PathKind::Other
}

/// Classify one advance. Pure: every fact it needs is in `advance`.
pub(crate) fn classify(advance: &Advance<'_>) -> Verdict {
    if advance
        .paths
        .iter()
        .any(|path| advance.candidate_paths.contains(path))
    {
        return Verdict::everything("files_overlap");
    }
    let Some(selected) = advance.selected_filters else {
        return Verdict::everything("selector_unavailable");
    };
    let mut changed: Vec<String> = Vec::new();
    for path in advance.paths {
        match classify_path(path) {
            PathKind::Crate(package) => {
                if !advance.packages.contains(package) {
                    return Verdict::everything("unknown_crate");
                }
                if !changed.iter().any(|name| name == package) {
                    changed.push(package.to_owned());
                }
            }
            PathKind::Workspace => return Verdict::everything("workspace_manifest"),
            PathKind::RustOutsideCrates => return Verdict::everything("rust_outside_crates"),
            PathKind::Other => {}
        }
    }
    let mut tests = BTreeSet::new();
    let mut checks = BTreeSet::new();
    let mut family_closed = true;
    if !changed.is_empty() {
        let Some(dependents) = (advance.dependents)(&changed) else {
            return Verdict::everything("dependents_unknown");
        };
        for package in changed.iter().chain(&dependents) {
            tests.insert(format!("{}:*", family(package)));
            checks.insert(package.clone());
        }
        // A changed crate outside the family that the family builds against
        // reaches every family test; the family diff cannot name them.
        let foreign: Vec<String> = changed
            .iter()
            .filter(|package| family(package) != "rsid")
            .cloned()
            .collect();
        if !foreign.is_empty() {
            family_closed = (advance.dependents)(&foreign).is_some_and(|dependents| {
                !foreign
                    .iter()
                    .chain(&dependents)
                    .any(|package| family(package) == "rsid")
            });
        }
    }
    let family_filters: Vec<String> = selected
        .iter()
        .filter(|filter| {
            filter
                .split_once('=')
                .is_some_and(|(package, _)| family(package) == "rsid")
        })
        .cloned()
        .collect();
    for filter in selected {
        match filter_target(filter) {
            Some(target) => {
                tests.insert(target);
            }
            None => return Verdict::everything("selector_filter_unreadable"),
        }
    }
    let named = if tests.is_empty() { &checks } else { &tests };
    let reason = if named.is_empty() {
        "disjoint".to_owned()
    } else {
        format!(
            "overlap:{}",
            named
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(",")
        )
    };
    Verdict {
        reason,
        tests: Scope::from_set(tests),
        checks: Scope::from_set(checks),
        python_everything: false,
        family_closed,
        family_filters,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    /// A workspace of `rsid` and `rsid-store` (rsid depends on the store), the
    /// `rsi-common` leaf everything depends on, `rsi` (the TUI) and a loner.
    fn dependents(changed: &[String]) -> Option<Vec<String>> {
        let edges = [
            ("rsid-store", "rsid"),
            ("rsi-common", "rsid-store"),
            ("rsi-common", "rsid"),
            ("rsi-common", "rsi"),
            ("rsi-common", "rsi-tool"),
        ];
        let mut reached: BTreeSet<String> = changed.iter().cloned().collect();
        loop {
            let before = reached.len();
            for (dependency, dependent) in edges {
                if reached.contains(dependency) {
                    reached.insert(dependent.to_owned());
                }
            }
            if reached.len() == before {
                break;
            }
        }
        Some(
            reached
                .into_iter()
                .filter(|name| !changed.contains(name))
                .collect(),
        )
    }

    fn run(paths: &[&str], candidate: &[&str], selected: Option<&[String]>) -> Verdict {
        let packages = set(&["rsid", "rsid-store", "rsi-common", "rsi", "rsi-tool"]);
        let (paths, candidate) = (set(paths), set(candidate));
        classify(&Advance {
            paths: &paths,
            candidate_paths: &candidate,
            packages: &packages,
            dependents: &dependents,
            selected_filters: selected,
        })
    }

    #[test]
    fn a_doc_and_script_advance_selecting_no_test_reuses_everything() {
        let verdict = run(
            &["docs/agents/reference.md", "scripts/fmt-changed.sh"],
            &["crates/rsid/src/rpc.rs"],
            Some(&[]),
        );
        assert_eq!(verdict.reason, "disjoint");
        assert_eq!(verdict.tests, Scope::Nothing);
        assert_eq!(verdict.checks, Scope::Nothing);
        assert!(!verdict.python_everything);
        assert!(verdict.reuses_every_test());
    }

    #[test]
    fn a_command_file_a_bin_target_reads_reruns_only_that_target() {
        let selected = strings(&["rsid=bin:rsid"]);
        let verdict = run(
            &[".claude/commands/land.md"],
            &["crates/rsid/src/rpc.rs"],
            Some(&selected),
        );
        assert_eq!(verdict.tests, Scope::Only(set(&["rsid:bin:rsid"])));
        assert!(verdict.tests.runs_test(Some("rsid:bin:rsid")));
        assert!(
            !verdict.tests.runs_test(Some("rsid:lib")),
            "the candidate's library run keeps its result"
        );
        assert_eq!(verdict.checks, Scope::Nothing, "no Rust changed");
        assert!(!verdict.reuses_every_test());
    }

    #[test]
    fn a_crate_change_reruns_that_crate_and_its_dependents_only() {
        let verdict = run(
            &["crates/rsid-store/src/store/mod.rs"],
            &["crates/rsi/src/app.rs"],
            Some(&[]),
        );
        for key in ["rsid:lib", "rsid:bin:rsid", "rsid:test:x"] {
            assert!(verdict.tests.runs_test(Some(key)), "{key}");
        }
        assert!(
            !verdict.tests.runs_test(Some("rsi:lib")),
            "rsi does not depend on it"
        );
        assert!(!verdict.tests.runs_test(Some("rsi-common:lib")));
        assert!(verdict.checks.runs_check(Some("rsid-store")));
        assert!(verdict.checks.runs_check(Some("rsid")));
        assert!(!verdict.checks.runs_check(Some("rsi")));
        assert!(verdict.reason.starts_with("overlap:rsid:*"), "{verdict:?}");
    }

    #[test]
    fn a_leaf_crate_change_reaches_every_dependent() {
        let verdict = run(&["crates/rsi-common/src/rpc.rs"], &["docs/x.md"], Some(&[]));
        for key in ["rsid:lib", "rsi:lib", "rsi-tool:lib", "rsi-common:lib"] {
            assert!(verdict.tests.runs_test(Some(key)), "{key}");
        }
    }

    #[test]
    fn an_advance_in_another_crate_leaves_the_candidates_tests_alone() {
        let verdict = run(
            &["crates/rsi/src/app.rs"],
            &["crates/rsid/src/rpc.rs"],
            Some(&[]),
        );
        assert!(!verdict.tests.runs_test(Some("rsid:lib")));
        assert!(!verdict.tests.runs_test(Some("rsid:bin:rsid")));
        assert!(verdict.tests.runs_test(Some("rsi:lib")));
        assert!(!verdict.checks.runs_check(Some("rsid")));
        assert!(verdict.checks.runs_check(Some("rsi")));
    }

    #[test]
    fn an_overlapping_file_reruns_everything() {
        let verdict = run(
            &["crates/rsid/src/rpc.rs"],
            &["crates/rsid/src/rpc.rs", "docs/x.md"],
            Some(&[]),
        );
        assert_eq!(verdict, Verdict::everything("files_overlap"));
        assert!(verdict.tests.runs_test(Some("rsi:lib")));
        assert!(verdict.python_everything);
    }

    #[test]
    fn what_the_classification_cannot_place_reruns_everything() {
        for (paths, reason) in [
            (&["Cargo.lock"][..], "workspace_manifest"),
            (&["Cargo.toml"][..], "workspace_manifest"),
            (&["rust-toolchain.toml"][..], "workspace_manifest"),
            (&[".cargo/config.toml"][..], "workspace_manifest"),
            (&["crates/ghost/src/lib.rs"][..], "unknown_crate"),
            (&["xtask/src/main.rs"][..], "rust_outside_crates"),
        ] {
            let verdict = run(paths, &["docs/x.md"], Some(&[]));
            assert_eq!(verdict, Verdict::everything(reason), "{paths:?}");
        }
    }

    #[test]
    fn an_unavailable_selector_reruns_everything() {
        let verdict = run(
            &["docs/agents/reference.md"],
            &["crates/rsid/src/rpc.rs"],
            None,
        );
        assert_eq!(verdict, Verdict::everything("selector_unavailable"));
    }

    #[test]
    fn an_unreadable_dependency_graph_reruns_everything() {
        let packages = set(&["rsid"]);
        let (paths, candidate) = (set(&["crates/rsid/src/lib.rs"]), set(&["docs/x.md"]));
        let verdict = classify(&Advance {
            paths: &paths,
            candidate_paths: &candidate,
            packages: &packages,
            dependents: &|_| None,
            selected_filters: Some(&[]),
        });
        assert_eq!(verdict, Verdict::everything("dependents_unknown"));
    }

    #[test]
    fn an_unreadable_selector_filter_reruns_everything() {
        let selected = strings(&["no-equals-sign"]);
        let verdict = run(&["docs/x.md"], &["crates/rsid/src/rpc.rs"], Some(&selected));
        assert_eq!(verdict, Verdict::everything("selector_filter_unreadable"));
    }

    #[test]
    fn filters_name_the_target_they_run() {
        for (filter, target) in [
            ("rsid=shard:store-01:test(session)", Some("rsid:lib")),
            ("rsid-store=store::tests", Some("rsid:lib")),
            ("rsi-common=rpc", Some("rsi-common:lib")),
            ("rsid=bin:rsid", Some("rsid:bin:rsid")),
            ("rsi=test:e2e_tui:test(=x)", Some("rsi:test:e2e_tui")),
            ("rsi=bin:", None),
            ("rsi", None),
        ] {
            assert_eq!(filter_target(filter).as_deref(), target, "{filter}");
        }
    }

    #[test]
    fn a_scope_decides_per_target_and_per_check() {
        let only = Scope::Only(set(&["rsid:*", "rsi-common:lib"]));
        assert!(only.runs_test(Some("rsid:bin:rsid")));
        assert!(only.runs_test(Some("rsi-common:lib")));
        assert!(!only.runs_test(Some("rsi-common:bin:x")));
        assert!(only.runs_test(None), "an unclassified command always runs");
        assert!(!Scope::Nothing.runs_test(None));
        assert!(Scope::Everything.runs_test(Some("rsi:lib")));
        assert!(Scope::Everything.runs_check(None));
        assert!(!Scope::Nothing.runs_check(Some("rsid")));
        assert!(Scope::Only(set(&["rsid"])).runs_check(Some("rsid")));
        assert!(!Scope::Only(set(&["rsid"])).runs_check(Some("rsi")));
        assert!(only.selects_family("rsid"));
        assert!(!only.selects_family("rsi"));
        assert!(Scope::Everything.selects_family("rsi"));
        assert!(!Scope::Nothing.selects_family("rsid"));
    }

    /// #1957: the family's re-run narrows to the closure of the advance only
    /// when nothing outside the family reaches the family's tests.
    #[test]
    fn the_family_is_closed_unless_a_crate_it_builds_against_changed() {
        let selected = strings(&["rsid=session::manager", "rsi=app", "rsid-store=bin:x"]);
        for (paths, closed) in [
            (&["crates/rsid/src/rpc.rs", "docs/x.md"][..], true),
            (&["crates/rsid-store/src/store/mod.rs"][..], true),
            // The TUI is no dependency of the family.
            (
                &["crates/rsi/src/app.rs", "crates/rsid/src/rpc.rs"][..],
                true,
            ),
            (&["docs/x.md"][..], true),
            // Everything in the family depends on the leaf.
            (
                &["crates/rsi-common/src/rpc.rs", "crates/rsid/src/rpc.rs"][..],
                false,
            ),
        ] {
            let verdict = run(paths, &["crates/rsi-tool/src/lib.rs"], Some(&selected));
            assert_eq!(verdict.family_closed, closed, "{paths:?}");
            assert_eq!(
                verdict.family_filters,
                strings(&["rsid=session::manager", "rsid-store=bin:x"]),
                "{paths:?}"
            );
        }
        let everything = run(&["Cargo.lock"], &["docs/x.md"], Some(&selected));
        assert!(!everything.family_closed);
        assert!(everything.family_filters.is_empty());
    }
}
