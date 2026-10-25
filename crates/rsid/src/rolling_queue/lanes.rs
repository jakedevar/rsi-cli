//! #1224: merge-queue lanes. A queued source's package set is the Cargo
//! packages its diff touches plus every workspace package that depends on
//! them; the store claims a second batch beside a gating one only when the two
//! sets are disjoint (`Store::claim_rolling_queue_batch_lanes`).
//!
//! This set is a scheduling heuristic, not a safety claim: both lanes gate on
//! the same base tip, and the lander proves at publication whether the other
//! lane's advance may be reused (`stale_delta`) or must be re-gated. Anything
//! this module cannot attribute to a package (a manifest, the lockfile, a
//! script, a tool, a file outside every package) is `None`: the whole
//! workspace, which shares the gate with nothing.

use super::git;
use crate::store::rolling_queue::LanePackages;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

/// Paths that no Cargo package builds or tests: prose and records. A change
/// to only these shares the gate with every package.
const NON_PACKAGE_PREFIXES: [&str; 2] = ["docs/", "thoughts/"];

/// Forgets the memoized answers past this many, so a long-lived daemon's cache
/// stays small (a source is asked about for as long as it is queued).
const CACHE_LIMIT: usize = 1024;

/// One Cargo package of the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Package {
    name: String,
    /// Repository-relative directory, no trailing slash (`""` is the root).
    dir: String,
    /// Names of the workspace packages it depends on (normal, dev, build).
    depends_on: BTreeSet<String>,
}

/// The workspace packages and their dependency edges at one revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WorkspaceGraph {
    packages: Vec<Package>,
}

fn manifest_dir(path: &str) -> &str {
    path.strip_suffix("Cargo.toml")
        .map(|dir| dir.trim_end_matches('/'))
        .unwrap_or_default()
}

impl WorkspaceGraph {
    /// Build the graph from `(path of a Cargo.toml, its text)` pairs. A manifest
    /// without a `[package]` table (the workspace root) is not a package. `None`
    /// when a manifest does not parse or no package is found: the caller then
    /// treats every source as whole-workspace.
    pub(super) fn from_manifests(manifests: &[(String, String)]) -> Option<Self> {
        let mut parsed = Vec::new();
        for (path, text) in manifests {
            let table: toml::Table = text.parse().ok()?;
            let Some(name) = table
                .get("package")
                .and_then(|package| package.get("name"))
                .and_then(toml::Value::as_str)
            else {
                continue;
            };
            parsed.push((name.to_string(), manifest_dir(path).to_string(), table));
        }
        if parsed.is_empty() {
            return None;
        }
        let names: BTreeSet<String> = parsed.iter().map(|(name, _, _)| name.clone()).collect();
        let packages = parsed
            .into_iter()
            .map(|(name, dir, table)| {
                let mut depends_on = BTreeSet::new();
                collect_dependencies(&table, &names, &mut depends_on);
                depends_on.remove(&name);
                Package {
                    name,
                    dir,
                    depends_on,
                }
            })
            .collect();
        Some(Self { packages })
    }

    /// The packages a diff touches plus all their dependents. `None` when a path
    /// is not attributable: a manifest or lockfile (the graph itself may move),
    /// or a file outside every package that is not prose.
    pub(super) fn packages_for(&self, changed: &[String]) -> LanePackages {
        let mut touched = BTreeSet::new();
        for path in changed {
            if path == "Cargo.lock"
                || path == "Cargo.toml"
                || path.ends_with("/Cargo.toml")
                || path.ends_with("/Cargo.lock")
            {
                return None;
            }
            let owners: Vec<&Package> = self
                .packages
                .iter()
                .filter(|package| {
                    !package.dir.is_empty()
                        && path
                            .strip_prefix(package.dir.as_str())
                            .is_some_and(|rest| rest.starts_with('/'))
                })
                .collect();
            if owners.is_empty() {
                if NON_PACKAGE_PREFIXES
                    .iter()
                    .any(|prefix| path.starts_with(prefix))
                {
                    continue;
                }
                return None;
            }
            // A fixture package nested in another package belongs to both.
            touched.extend(owners.into_iter().map(|package| package.name.clone()));
        }
        // Reverse closure over the dependency edges.
        let mut reverse: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for package in &self.packages {
            for dependency in &package.depends_on {
                reverse
                    .entry(dependency.as_str())
                    .or_default()
                    .push(package.name.as_str());
            }
        }
        let mut closed = touched.clone();
        let mut frontier: Vec<String> = touched.into_iter().collect();
        while let Some(name) = frontier.pop() {
            for dependent in reverse.get(name.as_str()).into_iter().flatten() {
                if closed.insert((*dependent).to_string()) {
                    frontier.push((*dependent).to_string());
                }
            }
        }
        Some(closed)
    }
}

/// Add every dependency of `table` (and its `target.*` tables) that names a
/// workspace package. The dependency key is the package unless it renames one
/// (`package = "real-name"`).
fn collect_dependencies(table: &toml::Table, names: &BTreeSet<String>, out: &mut BTreeSet<String>) {
    for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
        let Some(dependencies) = table.get(section).and_then(toml::Value::as_table) else {
            continue;
        };
        for (key, value) in dependencies {
            let real = value
                .get("package")
                .and_then(toml::Value::as_str)
                .unwrap_or(key);
            if names.contains(real) {
                out.insert(real.to_string());
            }
        }
    }
    if let Some(targets) = table.get("target").and_then(toml::Value::as_table) {
        for platform in targets.values().filter_map(toml::Value::as_table) {
            collect_dependencies(platform, names, out);
        }
    }
}

/// The commit the checkout's rolling ref names.
fn rolling_tip(repo: &Path) -> Option<String> {
    ["origin/rolling", "rolling"]
        .into_iter()
        .find_map(|reference| {
            let tip = git(
                repo,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("{reference}^{{commit}}"),
                ],
            )?;
            Some(tip.trim().to_string())
        })
}

fn graph_at(repo: &Path, tip: &str) -> Option<WorkspaceGraph> {
    let listing = git(repo, &["ls-tree", "-r", "--name-only", tip])?;
    let mut manifests = Vec::new();
    for path in listing
        .lines()
        .filter(|path| *path == "Cargo.toml" || path.ends_with("/Cargo.toml"))
    {
        let text = git(repo, &["show", &format!("{tip}:{path}")])?;
        manifests.push((path.to_string(), text));
    }
    WorkspaceGraph::from_manifests(&manifests)
}

#[derive(Default)]
struct Cache {
    graphs: HashMap<String, Option<Arc<WorkspaceGraph>>>,
    sources: HashMap<String, LanePackages>,
}

fn cache() -> &'static Mutex<Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(Cache::default()))
}

/// The package set of `source` in the repository checked out at `repo`: what
/// its diff against the rolling tip touches, plus dependents. `None` when
/// anything is unreadable (a reclaimed checkout, no rolling ref, an oversized
/// delta) or not attributable to a package. Memoized per rolling tip and source;
/// blocking git, so callers run it off the async executor.
pub(super) fn source_lane_packages(repo: &Path, source: &str) -> LanePackages {
    let tip = rolling_tip(repo)?;
    let key = format!("{tip}:{source}");
    let graph = {
        let mut cache = cache().lock().ok()?;
        if let Some(known) = cache.sources.get(&key) {
            return known.clone();
        }
        if cache.sources.len() >= CACHE_LIMIT || cache.graphs.len() >= CACHE_LIMIT {
            *cache = Cache::default();
        }
        cache.graphs.get(&tip).cloned()
    };
    let graph = match graph {
        Some(graph) => graph,
        None => {
            let built = graph_at(repo, &tip).map(Arc::new);
            if let Ok(mut cache) = cache().lock() {
                cache.graphs.insert(tip.clone(), built.clone());
            }
            built
        }
    };
    let answer = graph.and_then(|graph| match super::source_changed_paths(repo, source) {
        super::DeltaRead::Paths(paths) => graph.packages_for(&paths),
        _ => None,
    });
    if let Ok(mut cache) = cache().lock() {
        cache.sources.insert(key, answer.clone());
    }
    answer
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(name: &str, dependencies: &[&str], dev: &[&str]) -> String {
        let section = |title: &str, names: &[&str]| {
            let lines: String = names
                .iter()
                .map(|dependency| format!("{dependency} = {{ path = \"../{dependency}\" }}\n"))
                .collect();
            format!("[{title}]\n{lines}")
        };
        format!(
            "[package]\nname = \"{name}\"\n{}{}",
            section("dependencies", dependencies),
            section("dev-dependencies", dev)
        )
    }

    fn graph() -> WorkspaceGraph {
        let manifests = vec![
            (
                "Cargo.toml".to_string(),
                "[workspace]\nmembers = []\n".to_string(),
            ),
            (
                "crates/rsi-common/Cargo.toml".into(),
                manifest("rsi-common", &[], &[]),
            ),
            (
                "crates/rsid-store/Cargo.toml".into(),
                manifest("rsid-store", &["rsi-common"], &[]),
            ),
            (
                "crates/rsid/Cargo.toml".into(),
                manifest("rsid", &["rsid-store", "rsi-common"], &[]),
            ),
            (
                "crates/rsi/Cargo.toml".into(),
                manifest("rsi", &["rsi-common"], &[]),
            ),
            (
                "crates/rsid-store/tests/fixtures/consumer/Cargo.toml".into(),
                manifest("consumer", &[], &["rsid-store"]),
            ),
        ];
        WorkspaceGraph::from_manifests(&manifests).expect("graph")
    }

    fn set(names: &[&str]) -> LanePackages {
        Some(names.iter().map(|name| name.to_string()).collect())
    }

    fn paths(list: &[&str]) -> Vec<String> {
        list.iter().map(|path| path.to_string()).collect()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_leaf_package_change_is_just_that_package_1224() {
        assert_eq!(
            graph().packages_for(&paths(&["crates/rsi/src/app.rs"])),
            set(&["rsi"])
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_dependency_change_adds_every_dependent_1224() {
        assert_eq!(
            graph().packages_for(&paths(&["crates/rsid-store/src/store/mod.rs"])),
            set(&["consumer", "rsid", "rsid-store"])
        );
        assert_eq!(
            graph().packages_for(&paths(&["crates/rsi-common/src/lib.rs"])),
            set(&["consumer", "rsi", "rsi-common", "rsid", "rsid-store"])
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn disjoint_packages_do_not_share_a_lane_1224() {
        let tui = graph()
            .packages_for(&paths(&["crates/rsi/src/app.rs"]))
            .unwrap();
        let daemon = graph()
            .packages_for(&paths(&["crates/rsid/src/rpc.rs"]))
            .unwrap();
        assert!(tui.is_disjoint(&daemon));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_nested_fixture_package_belongs_to_its_parent_too_1224() {
        assert_eq!(
            graph().packages_for(&paths(&[
                "crates/rsid-store/tests/fixtures/consumer/src/lib.rs"
            ])),
            set(&["consumer", "rsid-store", "rsid"])
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn prose_shares_every_lane_and_anything_unattributable_shares_none_1224() {
        let graph = graph();
        assert_eq!(
            graph.packages_for(&paths(&["docs/agents/reference.md", "thoughts/x.md"])),
            set(&[])
        );
        for whole in [
            "Cargo.lock",
            "Cargo.toml",
            "crates/rsi/Cargo.toml",
            "scripts/scoped-test",
            "tools/released-migrations.json",
            "Makefile",
        ] {
            assert_eq!(
                graph.packages_for(&paths(&["crates/rsi/src/app.rs", whole])),
                None,
                "{whole}"
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_renamed_and_target_scoped_dependency_is_an_edge_1224() {
        let manifests = vec![
            ("crates/a/Cargo.toml".to_string(), manifest("a", &[], &[])),
            (
                "crates/b/Cargo.toml".to_string(),
                "[package]\nname = \"b\"\n[target.'cfg(unix)'.dependencies]\nalias = { package = \"a\", path = \"../a\" }\n"
                    .to_string(),
            ),
        ];
        let graph = WorkspaceGraph::from_manifests(&manifests).unwrap();
        assert_eq!(
            graph.packages_for(&paths(&["crates/a/src/lib.rs"])),
            set(&["a", "b"])
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn an_unparsable_manifest_or_no_package_leaves_no_graph_1224() {
        assert!(
            WorkspaceGraph::from_manifests(&[("crates/a/Cargo.toml".into(), "[package".into())])
                .is_none()
        );
        assert!(
            WorkspaceGraph::from_manifests(&[("Cargo.toml".into(), "[workspace]\n".into())])
                .is_none()
        );
    }
}
