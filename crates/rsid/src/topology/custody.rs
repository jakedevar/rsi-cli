//! Exact Git commit lineage for topology node chains.
//!
//! The whole-workflow plan is shared by the durable executor (#634), which
//! resolves pins from `topology_node_attempts`, and by the legacy in-memory
//! runner kept only as the `topology_executor_enabled=false` rollback path.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use crate::error::{DaemonError, Result};
use rsi_graph::format::{EdgeDef, NodeDef};
use rusqlite::OptionalExtension;
use sha2::Digest;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub(crate) struct TopologyForkSource {
    origin: PathBuf,
    commit: String,
}

impl TopologyForkSource {
    pub(crate) fn origin(&self) -> &Path {
        &self.origin
    }

    pub(crate) fn commit(&self) -> &str {
        &self.commit
    }
}

#[derive(Debug, Clone)]
pub(crate) struct TopologyCustody {
    pub(crate) repo_root: PathBuf,
    pub(crate) base_commit: String,
    pub(crate) execution_id: Uuid,
    results: Arc<Mutex<HashMap<String, String>>>,
    attempt_results: Arc<Mutex<HashMap<(String, u32), String>>>,
    pins: Arc<Mutex<HashMap<String, String>>>,
}

impl TopologyForkSource {
    /// A daemon-observed exact commit in `repo`; never a moving ref.
    pub(crate) fn verified(repo: &Path, commit: &str) -> Result<Self> {
        verify_exact_commit(repo, commit)?;
        Ok(Self {
            origin: repo.to_path_buf(),
            commit: commit.to_owned(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "node", rename_all = "snake_case")]
enum PlannedNodeSource {
    Base,
    Latest(String),
    CurrentIteration(String),
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct NodeForkPlan {
    initial: PlannedNodeSource,
    loop_previous: Option<String>,
    /// Index into the workflow's `scc_regions`; `None` outside any loop.
    #[serde(default)]
    region: Option<usize>,
}

/// Which recorded result a durable fork needs from its source node.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PinSelector {
    /// The source's result at exactly this iteration.
    Iteration(u32),
    /// The source's final (highest-iteration) result.
    Latest,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct TopologyCustodyPlan {
    nodes: BTreeMap<String, NodeForkPlan>,
}

impl TopologyCustodyPlan {
    pub(crate) fn node(&self, node: &str) -> Result<&NodeForkPlan> {
        self.nodes
            .get(node)
            .ok_or_else(|| DaemonError::InvalidParam(format!("node {node} has no custody plan")))
    }
}

impl NodeForkPlan {
    /// Durable resolution: which `(source node, pin)` this iteration forks
    /// from, or `None` for the execution base. Back-edges apply only from
    /// iteration 1; a cross-region source is always its final result (R4-3).
    pub(crate) const fn durable_source(&self, iteration: u32) -> Option<(&str, PinSelector)> {
        if iteration > 0
            && let Some(source) = &self.loop_previous
        {
            return Some((source.as_str(), PinSelector::Iteration(iteration - 1)));
        }
        match &self.initial {
            PlannedNodeSource::Base => None,
            PlannedNodeSource::Latest(source) => Some((source.as_str(), PinSelector::Latest)),
            PlannedNodeSource::CurrentIteration(source) => {
                Some((source.as_str(), PinSelector::Iteration(iteration)))
            }
        }
    }

    pub(crate) fn resolve(
        &self,
        custody: &TopologyCustody,
        iteration: u32,
    ) -> Result<TopologyForkSource> {
        if iteration > 0
            && let Some(source) = &self.loop_previous
        {
            return custody.fork_at(source, iteration - 1);
        }
        match &self.initial {
            PlannedNodeSource::Base => custody.fork(None),
            PlannedNodeSource::Latest(source) => custody.fork_latest(source),
            PlannedNodeSource::CurrentIteration(source) => custody.fork_at(source, iteration),
        }
    }
}

/// Resolve every node's lineage before a live workflow is accepted. Loop
/// back-edges are excluded from ordinary fork inputs and selected only after
/// the first loop iteration has produced a pinned result.
pub(crate) fn plan_topology_custody(
    nodes: &[NodeDef],
    edges: &[EdgeDef],
    loop_edges: &[(String, String)],
    scc_regions: &[Vec<String>],
) -> Result<TopologyCustodyPlan> {
    let node_ids: HashSet<&str> = nodes.iter().map(|node| node.id.as_str()).collect();
    let loop_edge_set: HashSet<(&str, &str)> = loop_edges
        .iter()
        .map(|(source, target)| (source.as_str(), target.as_str()))
        .collect();
    // R4-3: "same region" means the same SCC, never the union of all SCCs.
    let region_of: HashMap<&str, usize> = scc_regions
        .iter()
        .enumerate()
        .flat_map(|(index, region)| region.iter().map(move |node| (node.as_str(), index)))
        .collect();
    let mut plans = BTreeMap::new();

    for node in nodes {
        let incoming: Vec<&str> = edges
            .iter()
            .filter(|edge| edge.target == node.id)
            .filter(|edge| !loop_edge_set.contains(&(edge.source.as_str(), edge.target.as_str())))
            .map(|edge| edge.source.as_str())
            .collect();
        let loop_sources: Vec<&str> = loop_edges
            .iter()
            .filter(|(_, target)| target == &node.id)
            .map(|(source, _)| source.as_str())
            .collect();
        if loop_sources.len() > 1 {
            return Err(DaemonError::InvalidParam(format!(
                "node {} has multiple loop custody sources",
                node.id
            )));
        }

        let explicit: Vec<&str> = node
            .tags
            .iter()
            .filter_map(|tag| tag.strip_prefix("custody.from="))
            .collect();
        if explicit.len() > 1 {
            return Err(DaemonError::InvalidParam(format!(
                "node {} has multiple custody.from values",
                node.id
            )));
        }

        let initial = if let Some(explicit) = explicit.first() {
            if *explicit == "base" {
                PlannedNodeSource::Base
            } else if let Some(source) = explicit.strip_prefix("node:") {
                if !node_ids.contains(source)
                    || !is_ancestor(source, &node.id, edges, &loop_edge_set)
                {
                    return Err(DaemonError::InvalidParam(format!(
                        "node {} custody source is not upstream",
                        node.id
                    )));
                }
                planned_node_source(source, &node.id, &region_of)
            } else {
                return Err(DaemonError::InvalidParam("invalid custody.from".into()));
            }
        } else {
            match incoming.as_slice() {
                [] => PlannedNodeSource::Base,
                [source] => planned_node_source(source, &node.id, &region_of),
                _ => {
                    return Err(DaemonError::InvalidParam(format!(
                        "node {} has multiple inputs; custody.from is required",
                        node.id
                    )));
                }
            }
        };

        plans.insert(
            node.id.clone(),
            NodeForkPlan {
                initial,
                loop_previous: loop_sources.first().map(|source| (*source).to_owned()),
                region: region_of.get(node.id.as_str()).copied(),
            },
        );
    }

    Ok(TopologyCustodyPlan { nodes: plans })
}

fn planned_node_source(
    source: &str,
    target: &str,
    region_of: &HashMap<&str, usize>,
) -> PlannedNodeSource {
    let target_region = region_of.get(target);
    if target_region.is_some() && target_region == region_of.get(source) {
        PlannedNodeSource::CurrentIteration(source.to_owned())
    } else {
        PlannedNodeSource::Latest(source.to_owned())
    }
}

fn is_ancestor(
    source: &str,
    target: &str,
    edges: &[EdgeDef],
    loop_edges: &HashSet<(&str, &str)>,
) -> bool {
    let mut pending = vec![source];
    let mut visited = HashSet::new();
    while let Some(current) = pending.pop() {
        if current == target {
            return true;
        }
        if !visited.insert(current) {
            continue;
        }
        pending.extend(
            edges
                .iter()
                .filter(|edge| edge.source == current)
                .filter(|edge| !loop_edges.contains(&(edge.source.as_str(), edge.target.as_str())))
                .map(|edge| edge.target.as_str()),
        );
    }
    false
}

impl TopologyCustody {
    pub(crate) fn new(repo_root: PathBuf, base_commit: String, execution_id: Uuid) -> Self {
        Self {
            repo_root,
            base_commit,
            execution_id,
            results: Arc::new(Mutex::new(HashMap::new())),
            attempt_results: Arc::new(Mutex::new(HashMap::new())),
            pins: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(crate) fn fork(&self, from: Option<&str>) -> Result<TopologyForkSource> {
        let commit = match from.unwrap_or("base") {
            "base" => self.base_commit.clone(),
            source if source.starts_with("node:") => {
                let node = &source[5..];
                self.results
                    .lock()
                    .map_err(|_| DaemonError::Store("topology custody lock poisoned".into()))?
                    .get(node)
                    .cloned()
                    .ok_or_else(|| {
                        DaemonError::InvalidParam(format!(
                            "custody source node {node} has no committed result"
                        ))
                    })?
            }
            _ => return Err(DaemonError::InvalidParam("invalid custody.from".into())),
        };
        self.fork_commit(commit)
    }

    fn fork_commit(&self, commit: String) -> Result<TopologyForkSource> {
        // The commit is a daemon-observed SHA, never a caller-selected moving ref.
        verify_exact_commit(&self.repo_root, &commit)?;
        Ok(TopologyForkSource {
            origin: self.repo_root.clone(),
            commit,
        })
    }

    fn fork_at(&self, node: &str, iteration: u32) -> Result<TopologyForkSource> {
        let commit = self
            .attempt_results
            .lock()
            .map_err(|_| DaemonError::Store("topology custody lock poisoned".into()))?
            .get(&(node.to_owned(), iteration))
            .cloned()
            .ok_or_else(|| {
                DaemonError::InvalidParam(format!(
                    "custody source node {node} has no committed result for iteration {iteration}"
                ))
            })?;
        self.fork_commit(commit)
    }

    fn fork_latest(&self, node: &str) -> Result<TopologyForkSource> {
        let commit = self
            .results
            .lock()
            .map_err(|_| DaemonError::Store("topology custody lock poisoned".into()))?
            .get(node)
            .cloned()
            .ok_or_else(|| {
                DaemonError::InvalidParam(format!(
                    "custody source node {node} has no committed result"
                ))
            })?;
        self.fork_commit(commit)
    }

    pub(crate) fn observe(&self, node: &str, iteration: u32, sandbox: &Path) -> Result<String> {
        let root = repository_root(sandbox)?;
        if root != sandbox.canonicalize().map_err(DaemonError::Io)? {
            return Err(DaemonError::InvalidParam(
                "topology sandbox root changed".into(),
            ));
        }
        if !git(sandbox, &["status", "--porcelain", "--untracked-files=all"])?.is_empty() {
            return Err(DaemonError::InvalidParam("uncommitted_work".into()));
        }
        let commit = git(
            sandbox,
            &["rev-parse", "--verify", &format!("HEAD^{{commit}}")],
        )?;
        let pin = format!(
            "refs/rsi/topology/{}/{}/{}",
            self.execution_id, node, iteration
        );
        git(sandbox, &["check-ref-format", &pin])?;
        git(sandbox, &["update-ref", &pin, &commit])?;
        self.pins
            .lock()
            .map_err(|_| DaemonError::Store("topology pin lock poisoned".into()))?
            .insert(pin, commit.clone());
        self.results
            .lock()
            .map_err(|_| DaemonError::Store("topology custody lock poisoned".into()))?
            .insert(node.to_owned(), commit.clone());
        self.attempt_results
            .lock()
            .map_err(|_| DaemonError::Store("topology custody lock poisoned".into()))?
            .insert((node.to_owned(), iteration), commit.clone());
        Ok(commit)
    }

    pub(crate) fn release_pins(&self) -> Result<()> {
        let pins = self
            .pins
            .lock()
            .map_err(|_| DaemonError::Store("topology pin lock poisoned".into()))?
            .clone();
        for (pin, commit) in pins.iter() {
            git(&self.repo_root, &["update-ref", "-d", pin, commit])?;
        }
        Ok(())
    }
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(DaemonError::Io)?;
    if !output.status.success() {
        return Err(DaemonError::InvalidParam(format!(
            "git {} failed: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

pub(crate) fn repository_root(path: &Path) -> Result<PathBuf> {
    let canonical = path
        .canonicalize()
        .map_err(|_| DaemonError::InvalidParam("working_dir is not a Git worktree".into()))?;
    if git(&canonical, &["rev-parse", "--is-inside-work-tree"])? != "true" {
        return Err(DaemonError::InvalidParam(
            "working_dir is not a Git worktree".into(),
        ));
    }
    PathBuf::from(git(&canonical, &["rev-parse", "--show-toplevel"])?)
        .canonicalize()
        .map_err(DaemonError::Io)
}

fn verify_exact_commit(repo: &Path, commit: &str) -> Result<()> {
    if !matches!(commit.len(), 40 | 64) || !commit.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(DaemonError::InvalidParam(
            "base_commit must be a full Git OID".into(),
        ));
    }
    let resolved = git(
        repo,
        &["rev-parse", "--verify", &format!("{commit}^{{commit}}")],
    )?;
    if resolved != commit {
        return Err(DaemonError::InvalidParam(
            "base_commit is not a commit".into(),
        ));
    }
    Ok(())
}

pub(crate) async fn resolve_execution_base(
    working_dir: &Path,
    explicit_commit: Option<&str>,
) -> Result<(PathBuf, String)> {
    let root = repository_root(working_dir)?;
    // A caller may name only the repository, never a subdirectory or another
    // session's sandbox as the node's cwd.
    if working_dir.canonicalize().map_err(DaemonError::Io)? != root {
        return Err(DaemonError::InvalidParam(
            "working_dir must name the repository root".into(),
        ));
    }
    if let Some(commit) = explicit_commit {
        verify_exact_commit(&root, commit)?;
        return Ok((root, commit.to_owned()));
    }
    let fetch = tokio::process::Command::new("git")
        .args([
            "fetch",
            "--no-tags",
            "origin",
            "+refs/heads/rolling:refs/remotes/origin/rolling",
        ])
        .current_dir(&root)
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(std::time::Duration::from_secs(20), fetch)
        .await
        .map_err(|_| DaemonError::InvalidParam("rolling base fetch timed out".into()))?
        .map_err(DaemonError::Io)?;
    if !output.status.success() {
        return Err(DaemonError::InvalidParam(
            "cannot fetch origin/rolling".into(),
        ));
    }
    let commit = git(
        &root,
        &[
            "rev-parse",
            "--verify",
            &format!("refs/remotes/origin/rolling^{{commit}}"),
        ],
    )?;
    verify_exact_commit(&root, &commit)?;
    Ok((root, commit))
}

/// Target only the terminal node's authenticated custody, leaving every other
/// node sandbox and cache untouched.
#[cfg(test)]
pub(crate) fn reclaim_terminal_node_cache(
    store: &crate::store::Store,
    sandbox_base: &Path,
    session_id: Uuid,
) -> Result<bool> {
    let Some((custody_id, generation)) = terminal_node_cache_custody(store, session_id)? else {
        return Ok(false);
    };
    crate::sandbox::custody::CustodyService::reclaim_terminal_target(
        store,
        custody_id,
        generation,
        sandbox_base,
    )
}

pub(crate) fn terminal_node_cache_custody(
    store: &crate::store::Store,
    session_id: Uuid,
) -> Result<Option<(Uuid, u64)>> {
    let candidate: Option<(String, i64)> = store
        .conn
        .query_row(
            "SELECT r.custody_id,r.generation FROM sandbox_custody_roots r \
         JOIN sessions s ON s.sandbox_custody_id=r.custody_id \
         WHERE s.id=?1 AND s.status IN ('Completed','Failed','Interrupted','Archived','Deleted')",
            [session_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((custody_id, generation)) = candidate else {
        return Ok(None);
    };
    let custody_id = Uuid::parse_str(&custody_id)
        .map_err(|_| DaemonError::Store("invalid topology custody UUID".into()))?;
    let generation = u64::try_from(generation)
        .map_err(|_| DaemonError::Store("invalid topology custody generation".into()))?;
    Ok(Some((custody_id, generation)))
}

/// Caller holds the custody root stripe and Store lock. Kept separate so node
/// completion can perform slow filesystem reclamation on a blocking thread.
pub(crate) fn reclaim_terminal_node_cache_locked(
    store: &crate::store::Store,
    sandbox_base: &Path,
    custody_id: Uuid,
    generation: u64,
) -> Result<bool> {
    crate::sandbox::custody::CustodyService::reclaim_terminal_target_bytes_locked(
        store,
        custody_id,
        generation,
        sandbox_base,
    )
    .map(|bytes| bytes.is_some())
}

// ─── Durable executor custody observation (#634) ──────────────────────────────

/// Daemon observation of one node sandbox; never a reported SHA.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ObservedSandbox {
    pub(crate) head: String,
    pub(crate) dirty: bool,
}

/// Read HEAD and cleanliness after authenticating that `sandbox` is itself a
/// worktree root (a subdirectory or a replaced path is refused).
pub(crate) fn observe_sandbox(sandbox: &Path) -> Result<ObservedSandbox> {
    let root = repository_root(sandbox)?;
    if root != sandbox.canonicalize().map_err(DaemonError::Io)? {
        return Err(DaemonError::InvalidParam(
            "topology sandbox root changed".into(),
        ));
    }
    let dirty = !git(sandbox, &["status", "--porcelain", "--untracked-files=all"])?.is_empty();
    let head = git(sandbox, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    Ok(ObservedSandbox { head, dirty })
}

/// [`observe_sandbox`] ignoring daemon-owned scratch directories (the
/// catalog-op postcondition, plan §3.2).
pub(crate) fn observe_sandbox_excluding(
    sandbox: &Path,
    scratch: &[&str],
) -> Result<ObservedSandbox> {
    let root = repository_root(sandbox)?;
    if root != sandbox.canonicalize().map_err(DaemonError::Io)? {
        return Err(DaemonError::InvalidParam(
            "topology sandbox root changed".into(),
        ));
    }
    let excludes: Vec<String> = scratch
        .iter()
        .map(|dir| format!(":(exclude){dir}"))
        .collect();
    let mut args = vec!["status", "--porcelain", "--untracked-files=all", "--", "."];
    args.extend(excludes.iter().map(String::as_str));
    let dirty = !git(sandbox, &args)?.is_empty();
    let head = git(sandbox, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    Ok(ObservedSandbox { head, dirty })
}

pub(crate) fn node_pin_ref(execution_id: Uuid, node: &str, iteration: u32) -> String {
    format!("refs/rsi/topology/{execution_id}/{node}/{iteration}")
}

/// A separate namespace: the node pin is a leaf ref, so nesting under it would
/// be a Git directory/file conflict (plan §3.4).
pub(crate) fn preserved_ref(
    execution_id: Uuid,
    node: &str,
    iteration: u32,
    attempt: u32,
) -> String {
    format!("refs/rsi/topology-preserved/{execution_id}/{node}/{iteration}/{attempt}")
}

/// Point `pin` at `commit` (overwriting only an earlier pin of the same node
/// instance) so downstream forks survive sandbox cleanup.
pub(crate) fn pin_commit(repo: &Path, pin: &str, commit: &str) -> Result<()> {
    git(repo, &["check-ref-format", pin])?;
    git(repo, &["update-ref", pin, commit])?;
    Ok(())
}

/// Delete `name` only if it still points at `expected`.
pub(crate) fn delete_ref(repo: &Path, name: &str, expected: &str) -> Result<()> {
    git(repo, &["update-ref", "-d", name, expected])?;
    Ok(())
}

/// `true` iff `name` exists and resolves to exactly `commit`.
pub(crate) fn ref_points_at(repo: &Path, name: &str, commit: &str) -> bool {
    git(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{name}^{{commit}}"),
        ],
    )
    .is_ok_and(|resolved| resolved == commit)
}

fn zero_oid(like: &str) -> String {
    "0".repeat(like.len())
}

fn create_ref(repo: &Path, name: &str, commit: &str) -> Result<()> {
    git(repo, &["check-ref-format", name])?;
    // Create-only: an existing ref (from any writer) is never overwritten.
    git(repo, &["update-ref", name, commit, &zero_oid(commit)])?;
    Ok(())
}

/// Create-only ref that tolerates this executor's own earlier write: a crash
/// between creating the ref and recording it leaves the same deterministic
/// name behind. An existing ref is adopted only when it is `commit` or
/// `equivalent` proves it carries the same content; anything else refuses.
fn create_or_adopt_ref(
    repo: &Path,
    name: &str,
    commit: &str,
    equivalent: impl Fn(&str) -> bool,
) -> Result<String> {
    git(repo, &["check-ref-format", name])?;
    if let Ok(existing) = git(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{name}^{{commit}}"),
        ],
    ) {
        if existing == commit || equivalent(&existing) {
            return Ok(existing);
        }
        return Err(DaemonError::InvalidParam(format!(
            "{name} already points at a different commit"
        )));
    }
    create_ref(repo, name, commit)?;
    Ok(commit.to_owned())
}

/// The recorded preservation point of a diverged attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Preservation {
    pub(crate) ref_name: String,
    pub(crate) commit: String,
    /// Digest of the dirty path set; `None` for a clean diverged HEAD pin.
    pub(crate) paths_digest: Option<String>,
}

/// Preserve divergent sandbox state before any `preserved_work` block is
/// recorded (plan §3.4, R3-4/R4-2). Dirty custody is snapshotted through a
/// temporary index (the sandbox index and worktree are never touched); a clean
/// diverged HEAD is pinned and verified. Returns `None` when the sandbox is
/// clean and still at `base_commit`: nothing to preserve.
pub(crate) fn preserve_sandbox(
    sandbox: &Path,
    ref_name: &str,
    attempt_id: Uuid,
    base_commit: &str,
) -> Result<Option<Preservation>> {
    let observed = observe_sandbox(sandbox)?;
    if !observed.dirty {
        if observed.head == base_commit {
            return Ok(None);
        }
        create_or_adopt_ref(sandbox, ref_name, &observed.head, |_| false)?;
        if !ref_points_at(sandbox, ref_name, &observed.head) {
            return Err(DaemonError::Store(
                "preservation pin could not be verified".into(),
            ));
        }
        return Ok(Some(Preservation {
            ref_name: ref_name.to_owned(),
            commit: observed.head,
            paths_digest: None,
        }));
    }

    let status = git(
        sandbox,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )?;
    let paths_digest = format!(
        "sha256:{}",
        hex::encode(sha2::Sha256::digest(status.as_bytes()))
    );
    let index = PathBuf::from(git(
        sandbox,
        &[
            "rev-parse",
            "--git-path",
            &format!("rsi-preserve-{attempt_id}.index"),
        ],
    )?);
    let index = if index.is_absolute() {
        index
    } else {
        sandbox.join(index)
    };
    let snapshot = (|| -> Result<String> {
        git_with_index(sandbox, &index, &["read-tree", "HEAD"])?;
        git_with_index(sandbox, &index, &["add", "-A"])?;
        let tree = git_with_index(sandbox, &index, &["write-tree"])?;
        let message = format!("topology: preserved work\n\nTopology-Preserved: {attempt_id}\n");
        git_with_env(
            sandbox,
            &[
                ("GIT_AUTHOR_NAME", "rsi topology"),
                ("GIT_AUTHOR_EMAIL", "topology@rsi.invalid"),
                ("GIT_COMMITTER_NAME", "rsi topology"),
                ("GIT_COMMITTER_EMAIL", "topology@rsi.invalid"),
            ],
            &["commit-tree", &tree, "-p", &observed.head, "-m", &message],
        )
    })();
    let _ = std::fs::remove_file(&index);
    let snapshot = snapshot?;
    // A replay after a crash re-snapshots the unchanged sandbox; the commit
    // differs only by timestamp, so adopt a ref with the same tree, parent
    // and attempt trailer.
    let tree = git(sandbox, &["rev-parse", &format!("{snapshot}^{{tree}}")])?;
    let trailer = format!("Topology-Preserved: {attempt_id}");
    let commit = create_or_adopt_ref(sandbox, ref_name, &snapshot, |existing| {
        git(sandbox, &["rev-parse", &format!("{existing}^{{tree}}")]).is_ok_and(|t| t == tree)
            && git(sandbox, &["rev-parse", &format!("{existing}^")])
                .is_ok_and(|parent| parent == observed.head)
            && git(sandbox, &["log", "-1", "--format=%B", existing])
                .is_ok_and(|message| message.contains(&trailer))
    })?;
    if !ref_points_at(sandbox, ref_name, &commit) {
        return Err(DaemonError::Store(
            "preservation snapshot could not be verified".into(),
        ));
    }
    Ok(Some(Preservation {
        ref_name: ref_name.to_owned(),
        commit,
        paths_digest: Some(paths_digest),
    }))
}

/// Node outputs above the inline cap live in the repository object store
/// (plan §2.1 path@commit): one parentless commit whose tree holds
/// `output.json`, kept reachable by a create-only per-attempt ref. A separate
/// namespace, because the node pin is a leaf ref (§3.4).
pub(crate) const OUTPUT_PATH: &str = "output.json";

pub(crate) fn output_ref(execution_id: Uuid, node: &str, iteration: u32, attempt: u32) -> String {
    format!("refs/rsi/topology-output/{execution_id}/{node}/{iteration}/{attempt}")
}

/// Store `content` and return the commit holding it (idempotent per ref).
pub(crate) fn store_output(repo: &Path, ref_name: &str, content: &str) -> Result<String> {
    let blob = git_with_stdin(repo, &["hash-object", "-w", "--stdin"], content)?;
    let tree = git_with_stdin(
        repo,
        &["mktree"],
        &format!("100644 blob {blob}\t{OUTPUT_PATH}\n"),
    )?;
    let commit = git_with_env(
        repo,
        &[
            ("GIT_AUTHOR_NAME", "rsi topology"),
            ("GIT_AUTHOR_EMAIL", "topology@rsi.invalid"),
            ("GIT_COMMITTER_NAME", "rsi topology"),
            ("GIT_COMMITTER_EMAIL", "topology@rsi.invalid"),
        ],
        &["commit-tree", &tree, "-m", "topology: node output"],
    )?;
    create_or_adopt_ref(repo, ref_name, &commit, |existing| {
        git(repo, &["rev-parse", &format!("{existing}^{{tree}}")]).is_ok_and(|t| t == tree)
    })
}

/// Read a stored output back; the caller verifies its digest.
pub(crate) fn read_output(repo: &Path, commit: &str) -> Result<String> {
    let output = Command::new("git")
        .args(["cat-file", "blob", &format!("{commit}:{OUTPUT_PATH}")])
        .current_dir(repo)
        .output()
        .map_err(DaemonError::Io)?;
    if !output.status.success() {
        return Err(DaemonError::Store(format!(
            "topology output {commit} is unreadable"
        )));
    }
    String::from_utf8(output.stdout)
        .map_err(|_| DaemonError::Store("topology output is not UTF-8".into()))
}

fn git_with_stdin(dir: &Path, args: &[&str], input: &str) -> Result<String> {
    use std::io::Write;
    let mut child = Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(DaemonError::Io)?;
    child
        .stdin
        .take()
        .ok_or_else(|| DaemonError::Process("git stdin unavailable".into()))?
        .write_all(input.as_bytes())
        .map_err(DaemonError::Io)?;
    let output = child.wait_with_output().map_err(DaemonError::Io)?;
    if !output.status.success() {
        return Err(DaemonError::InvalidParam(format!(
            "git {} failed: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn git_with_index(dir: &Path, index: &Path, args: &[&str]) -> Result<String> {
    let index = index.to_string_lossy().into_owned();
    git_with_env(dir, &[("GIT_INDEX_FILE", index.as_str())], args)
}

fn git_with_env(dir: &Path, env: &[(&str, &str)], args: &[&str]) -> Result<String> {
    let mut command = Command::new("git");
    command.args(args).current_dir(dir);
    for (key, value) in env {
        command.env(key, value);
    }
    let output = command.output().map_err(DaemonError::Io)?;
    if !output.status.success() {
        return Err(DaemonError::InvalidParam(format!(
            "git {} failed: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Bounded operator report for a preserved attempt (plan §3.4 `inspect`).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub(crate) struct SandboxInspection {
    pub(crate) head: String,
    pub(crate) base_commit: String,
    pub(crate) dirty_paths: Vec<String>,
    pub(crate) diffstat: Vec<String>,
}

pub(crate) const INSPECT_ENTRY_LIMIT: usize = 64;

pub(crate) fn inspect_sandbox(sandbox: &Path, base_commit: &str) -> Result<SandboxInspection> {
    let observed = observe_sandbox(sandbox)?;
    let dirty_paths = git(sandbox, &["status", "--porcelain", "--untracked-files=all"])?
        .lines()
        .take(INSPECT_ENTRY_LIMIT)
        .map(str::to_owned)
        .collect();
    let diffstat = git(sandbox, &["diff", "--numstat", base_commit])?
        .lines()
        .take(INSPECT_ENTRY_LIMIT)
        .map(str::to_owned)
        .collect();
    Ok(SandboxInspection {
        head: observed.head,
        base_commit: base_commit.to_owned(),
        dirty_paths,
        diffstat,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::SandboxAllocator;
    use crate::topology::launch::NodeLaunchBuilder;
    use rsi_common::types::{SandboxCleanupState, SandboxKind, SessionKind, SessionStatus};
    use rsi_graph::format::NodeDef;
    use tempfile::TempDir;

    fn repo() -> (TempDir, String) {
        let dir = TempDir::new().unwrap();
        git(dir.path(), &["init", "-q"]).unwrap();
        std::fs::write(dir.path().join("file.txt"), "base").unwrap();
        git(dir.path(), &["add", "file.txt"]).unwrap();
        git(
            dir.path(),
            &[
                "-c",
                "user.name=Topology Test",
                "-c",
                "user.email=topology@test.invalid",
                "commit",
                "-q",
                "-m",
                "base",
            ],
        )
        .unwrap();
        let head = git(dir.path(), &["rev-parse", "HEAD"]).unwrap();
        (dir, head)
    }

    fn builder<'a>(custody: &'a TopologyCustody) -> NodeLaunchBuilder<'a> {
        NodeLaunchBuilder {
            custody,
            is_topology: true,
            workflow_id: Uuid::new_v4(),
            project_id: None,
            parent_id: None,
        }
    }

    #[tokio::test]
    async fn t1_a1_every_node_allocates_fresh_sandbox() {
        let (repo, base) = repo();
        let sandboxes = TempDir::new().unwrap();
        let custody = TopologyCustody::new(repo.path().to_path_buf(), base, Uuid::new_v4());
        let allocator = SandboxAllocator::new(sandboxes.path().to_path_buf());
        let node = NodeDef::action("A", "A");
        let plan = plan_topology_custody(&[node.clone()], &[], &[], &[])
            .unwrap()
            .node("A")
            .unwrap()
            .clone();
        let (config, fork) = builder(&custody)
            .build(&node, "work".into(), 0, 0, &plan)
            .unwrap();
        assert!(config.sandbox.is_some());
        let allocation = allocator
            .allocate(
                Uuid::new_v4(),
                fork.origin(),
                SandboxKind::GitWorktree,
                fork.commit(),
                None,
            )
            .unwrap();
        assert!(allocation.root.starts_with(sandboxes.path()));
        assert_ne!(allocation.root, repo.path());
        assert_ne!(allocation.root, std::env::current_dir().unwrap());
    }

    #[tokio::test]
    async fn t1_a2_downstream_forks_observed_upstream_commit() {
        let (repo, base) = repo();
        let sandboxes = TempDir::new().unwrap();
        let allocator = SandboxAllocator::new(sandboxes.path().to_path_buf());
        let custody = TopologyCustody::new(repo.path().to_path_buf(), base.clone(), Uuid::new_v4());
        let a_node = NodeDef::action("A", "A");
        let mut b_node = NodeDef::action("B", "B");
        b_node.tags.push("custody.from=node:A".into());
        let plan = plan_topology_custody(
            &[a_node.clone(), b_node.clone()],
            &[EdgeDef::new("A", "B")],
            &[],
            &[],
        )
        .unwrap();
        let a = allocator
            .allocate(
                Uuid::new_v4(),
                repo.path(),
                SandboxKind::GitWorktree,
                &base,
                None,
            )
            .unwrap();
        std::fs::write(a.root.join("file.txt"), "A's commit").unwrap();
        git(&a.root, &["add", "file.txt"]).unwrap();
        git(
            &a.root,
            &[
                "-c",
                "user.name=Topology Test",
                "-c",
                "user.email=topology@test.invalid",
                "commit",
                "-q",
                "-m",
                "A",
            ],
        )
        .unwrap();
        let observed = custody.observe("A", 0, &a.root).unwrap();
        assert_ne!(observed, base);
        let (_, source) = builder(&custody)
            .build(&b_node, "review".into(), 0, 0, plan.node("B").unwrap())
            .unwrap();
        let b = allocator
            .allocate(
                Uuid::new_v4(),
                source.origin(),
                SandboxKind::GitWorktree,
                source.commit(),
                None,
            )
            .unwrap();
        assert_eq!(git(&b.root, &["rev-parse", "HEAD"]).unwrap(), observed);
        assert_eq!(
            std::fs::read_to_string(b.root.join("file.txt")).unwrap(),
            "A's commit"
        );
    }

    #[tokio::test]
    async fn t1_a3_non_repo_working_dir_is_invalid_before_allocation() {
        let non_repo = TempDir::new().unwrap();
        let error = resolve_execution_base(non_repo.path(), None)
            .await
            .unwrap_err();
        assert!(matches!(error, DaemonError::InvalidParam(_)));
    }

    #[tokio::test]
    async fn t1_a4_dirty_upstream_fails_uncommitted_work() {
        let (repo, base) = repo();
        let sandboxes = TempDir::new().unwrap();
        let allocator = SandboxAllocator::new(sandboxes.path().to_path_buf());
        let custody = TopologyCustody::new(repo.path().to_path_buf(), base.clone(), Uuid::new_v4());
        let a = allocator
            .allocate(
                Uuid::new_v4(),
                repo.path(),
                SandboxKind::GitWorktree,
                &base,
                None,
            )
            .unwrap();
        std::fs::write(a.root.join("untracked.txt"), "dirty").unwrap();
        let error = custody.observe("A", 0, &a.root).unwrap_err();
        assert!(error.to_string().contains("uncommitted_work"));
        assert!(custody.fork(Some("node:A")).is_err());
    }

    #[tokio::test]
    async fn t1_a5_terminal_node_reclaims_only_its_build_cache() {
        let (repo, base) = repo();
        let db = TempDir::new().unwrap();
        let sandboxes = TempDir::new().unwrap();
        let allocator = SandboxAllocator::new(sandboxes.path().to_path_buf());
        let mut store = crate::store::Store::open(&db.path().join("rsi.db")).unwrap();
        let mut sessions = Vec::new();
        for node in ["A", "B"] {
            let id = Uuid::new_v4();
            let allocation = allocator
                .allocate(id, repo.path(), SandboxKind::GitWorktree, &base, None)
                .unwrap();
            let mut session =
                crate::session::agent_verbs::tests::test_session(id, repo.path().to_path_buf());
            session.status = SessionStatus::Completed;
            session.retry_attempt = None;
            session.max_retries = None;
            session.sandbox_kind = Some(SandboxKind::GitWorktree);
            session.sandbox_root = Some(allocation.root.clone());
            session.sandbox_branch = allocation.branch.clone();
            session.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
            store
                .insert_session_with_custody(
                    &session,
                    crate::store::sandbox_custody::SessionCustodyBinding::New(
                        crate::store::sandbox_custody::NewCustodyRoot {
                            custody_id: Uuid::new_v4(),
                            canonical_repo_dir: repo.path().display().to_string(),
                            sandbox_root: allocation.root.display().to_string(),
                            sandbox_branch: allocation.branch.unwrap(),
                            repository_identity: repo
                                .path()
                                .join(".git")
                                .canonicalize()
                                .unwrap()
                                .display()
                                .to_string(),
                            source_commit: base.clone(),
                            cause: crate::store::sandbox_custody::CustodyCause::FreshLaunch,
                        },
                    ),
                )
                .unwrap();
            let cache = allocation.root.join("target");
            std::fs::create_dir_all(&cache).unwrap();
            std::fs::write(cache.join(format!("{node}.bin")), "build-cache").unwrap();
            sessions.push((id, cache));
        }
        assert!(reclaim_terminal_node_cache(&store, sandboxes.path(), sessions[0].0).unwrap());
        assert!(!sessions[0].1.exists());
        assert!(sessions[1].1.join("B.bin").exists());
    }

    #[tokio::test]
    async fn t1_a6_effort_kind_and_node_id_pass_through() {
        let (repo, base) = repo();
        let custody = TopologyCustody::new(repo.path().to_path_buf(), base, Uuid::new_v4());
        let mut node = NodeDef::action("implement", "Implement");
        node.tags
            .extend(["kind:Feature".into(), "effort=high".into()]);
        let plan = plan_topology_custody(&[node.clone()], &[], &[], &[])
            .unwrap()
            .node("implement")
            .unwrap()
            .clone();
        let (config, _) = builder(&custody)
            .build(&node, "work".into(), 0, 0, &plan)
            .unwrap();
        assert_eq!(config.session_kind, Some(SessionKind::Feature));
        assert_eq!(config.effort.as_deref(), Some("high"));
        assert_eq!(config.topology_node_id.as_deref(), Some("implement"));
    }
}
