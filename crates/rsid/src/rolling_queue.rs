//! Daemon-owned rolling merge queue runner (#1007 slice S1).
//!
//! The runner claims up to N ready sources in FIFO order (the operator's batch
//! size), runs the P0 lander (`rsi-rolling-land`) once with every source as an
//! `--accepted` argument (the lander chain-merges them onto the tip, renumbers
//! provisional migrations in that order, gates the candidate once with the
//! union of the members' test filters and publishes it with one fast-forward)
//! and settles each entry exactly once after verifying its ancestry. A batch
//! that is not green (red gate) is bisected: the
//! first half of the suspect window is gated against the tip it would publish
//! onto and published when green, narrowed when red, so each failure is
//! attributed to exactly one source and the innocent sources still publish. An
//! integration conflict is attributed the same way without a gate: the lander
//! names the conflicting source, only that entry is refused
//! (`queue_batch_merge_conflict`) and the batch is re-run without it (#1119).
//!
//! Restart reconcile runs once at boot: a `gating` entry whose source is
//! already an ancestor of `origin/rolling` settles as published; any other is
//! returned to `queued` and re-driven. The lander is idempotent (an integrated
//! source is refused), so re-driving cannot publish twice.
//!
//! A source that reached `origin/rolling` outside the queue (an operator merge
//! or a direct landing) is never gated: each member's ancestry is re-checked
//! right before its batch is gated, before every bisect step and after a
//! lander run that reports a source "already integrated", and a landed member
//! settles `published` with the tip that contains it (#1208). One batch's
//! gating is bounded by the operator's `rolling_queue_gate_timeout_mins`; past
//! it the unsettled members are refused with `queue_gate_timeout`.

use crate::config::RuntimeConfig;
use crate::error::{DaemonError, Result};
use crate::store::Store;
use chrono::Utc;
use rsi_common::rolling_queue::{
    QUEUE_BATCH_ANCESTRY_UNVERIFIED, QUEUE_BATCH_MERGE_CONFLICT, QUEUE_BATCH_POLICY_REFUSED,
    QUEUE_BISECT_NO_PROGRESS, QUEUE_FILTER_MATCHES_NO_TESTS, QUEUE_GATE_TIMEOUT,
    QUEUE_MIGRATION_OUT_OF_ORDER, QUEUE_REGATE_EXHAUSTED, QUEUE_SOURCE_ALREADY_INTEGRATED,
    ROLLING_QUEUE_DEFAULT_GATE_TIMEOUT_MINS, ROLLING_QUEUE_MAX_BATCH_SIZE, RollingQueueEntryState,
    RollingQueueEntryV1, RollingQueueOutcome,
};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::process::Command;
use uuid::Uuid;

/// Poll interval of the runner loop.
const TICK: Duration = Duration::from_secs(5);
/// The lander's refusal of an `--accepted` source `origin/rolling` already
/// contains ("accepted source <sha> is already integrated; ...").
const ALREADY_INTEGRATED_MARKER: &str = "is already integrated";
/// The out-of-band budget: a moved tip costs at most one regate of the head.
const MAX_REGATES_ENV: &str = "RSI_LANDER_MAX_REGATED_STALE_RETRIES";
const MAX_REGATES: usize = 1;
/// Queue runs never reuse a gate across a moved tip: every stale advance re-gates
/// the remade candidate, still bounded by the regate budget above.
const EXACT_GATE_ENV: &str = "RSI_LANDER_EXACT_GATE";
const OUTCOME_TAIL_BYTES: usize = 2000;
const MAX_FAILING_TESTS: usize = 32;
/// Per-run lander logs (#1135): each stream keeps its newest bytes, and only the
/// newest files stay in the log directory.
const RUN_LOG_STREAM_BYTES: usize = 1 << 20;
const RUN_LOG_KEEP_FILES: usize = 40;
/// Failing tests named in an event's cause (the log has the rest).
const CAUSE_TESTS: usize = 5;

/// Files that many concurrent sources touch. Informational only: the queue
/// never waits on, claims or seals them (operator directive 2026-09-29).
const HOT_FILES: &[&str] = &[
    "crates/rsid-store/src/config.rs",
    // Before the #1021 S4 crate split (stale sources still touch it).
    "crates/rsid/src/config.rs",
    "crates/rsi-common/src/rpc.rs",
    "crates/rsi-common/src/agent_control_schema.rs",
    "crates/rsi-common/src/bin/rsi-rpc.rs",
    "crates/rsi/src/settings_registry.rs",
    "crates/rsi/src/settings_keys.rs",
];
/// Each schema version is its own file (`vNNN.rs`) here; the head is the
/// highest one, so adding a migration never edits a shared file.
const MIGRATION_DIR: &str = "crates/rsid-store/src/store/migrations";
/// The layout before the #1021 S4 crate split (the store lived in `crates/rsid`):
/// a rolling base or a stale source cut before it is still read.
const PRE_SPLIT_MIGRATION_DIR: &str = "crates/rsid/src/store/migrations";
/// A source that declares a provisional migration is renumbered by the lander
/// at landing, so its number is assigned in queue order.
const PROVISIONAL_DIR: &str = "tools/provisional-migrations/";
/// Older revisions declared the head as a constant in this file.
const LEGACY_MIGRATION_FILES: [&str; 2] = [
    "crates/rsid-store/src/store/mod.rs",
    "crates/rsid/src/store/mod.rs",
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SourceFacts {
    pub migration_version: Option<u32>,
    pub hot_files: Vec<String>,
}

/// Environment an UNFENCED queue git inherits from the daemon: locale, the
/// user's identity and home, and the transport settings a fetch needs. Every
/// other name (above all `GIT_TRACE*`, which git opens for append wherever it
/// points, and the `GIT_*` knobs that reconfigure it) is dropped (#1160).
fn inherited_git_env(
    vars: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    const KEEP: &[&str] = &[
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "LANG",
        "LANGUAGE",
        "TZ",
        "TMPDIR",
        "XDG_CONFIG_HOME",
        "XDG_RUNTIME_DIR",
        "SSH_AUTH_SOCK",
        "GIT_SSH",
        "GIT_SSH_COMMAND",
        "GIT_ASKPASS",
        "SSH_ASKPASS",
        "GIT_TERMINAL_PROMPT",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "no_proxy",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "CURL_CA_BUNDLE",
    ];
    vars.into_iter()
        .filter(|(name, _)| {
            let name = name.to_string_lossy();
            KEEP.contains(&name.as_ref()) || name.starts_with("LC_")
        })
        .collect()
}

/// A `git` command with the daemon's environment reduced to the allowlist.
fn git_command() -> std::process::Command {
    let mut command = std::process::Command::new("git");
    command
        .env_clear()
        .envs(inherited_git_env(std::env::vars_os()));
    command
}

fn git(repo: &Path, args: &[&str]) -> Option<String> {
    let output = git_command()
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Whether `source` names a commit in `repo`'s object store.
pub(crate) fn source_commit_exists(repo: &Path, source: &str) -> bool {
    git(repo, &["cat-file", "-e", &format!("{source}^{{commit}}")]).is_some()
}

/// Exit status of `git grep` run in `repo`: `Some(true)` on a match,
/// `Some(false)` on none, `None` when git itself failed.
fn git_grep_hits(repo: &Path, args: &[&str]) -> Option<(bool, String)> {
    let output = git_command()
        .arg("-C")
        .arg(repo)
        .arg("grep")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    match output.status.code() {
        Some(0) => Some((true, String::from_utf8_lossy(&output.stdout).into_owned())),
        Some(1) => Some((false, String::new())),
        _ => None,
    }
}

/// First test filter in `filters` that provably selects no test at `commit`
/// (#1129), or `None`. A cheap static necessary condition, never a refusal on
/// doubt: every `::`-separated segment of the filter's test-name pattern must
/// occur in the source of the package (for `rsid=shard:S:test(N)`, of the
/// files carrying shard S's gate). Forms the check cannot reason about
/// (`bin:`/`test:` targets, filtersets other than `test(...)`) are accepted.
#[must_use]
pub fn filter_selecting_no_tests(repo: &Path, commit: &str, filters: &[String]) -> Option<String> {
    filters
        .iter()
        .find(|filter| filter_provably_empty(repo, commit, filter))
        .cloned()
}

fn filter_provably_empty(repo: &Path, commit: &str, filter: &str) -> bool {
    let Some((package, value)) = filter.split_once('=') else {
        return false;
    };
    let (shard, pattern) = match value.strip_prefix("shard:") {
        Some(rest) => match rest.split_once(':') {
            Some((shard, filterset)) => match filterset
                .strip_prefix("test(")
                .and_then(|inner| inner.strip_suffix(')'))
            {
                Some(name) => (Some(shard), name),
                None => return false,
            },
            // A whole shard always has tests.
            None => return false,
        },
        None => (None, value),
    };
    if pattern.is_empty()
        || pattern.starts_with("bin:")
        || pattern.starts_with("test:")
        || !pattern
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_:.-".contains(&byte))
    {
        return false;
    }
    let dir = format!("crates/{package}");
    // A tree without the package (a fixture, a foreign repo) cannot be reasoned about.
    if git(repo, &["ls-tree", "--name-only", commit, &dir]).is_none_or(|out| out.trim().is_empty())
    {
        return false;
    }
    let mut scope: Vec<String> = vec![dir.clone()];
    let mut package_dirs: Vec<String> = vec![dir];
    // #1244: the lander runs a focused `rsid`/`rsid-store` filter (shard form
    // or plain name) on one lib build of both packages, so the name may live
    // in either package and in any shard; only an unknown shard still refuses.
    let lib_family = rsid_lib_family_dirs(repo, commit, package);
    if let Some(shard) = shard {
        // A shard spans every package that declares its feature (rsid and
        // rsid-store since the #1021 S4 crate split), so the gate lookup covers
        // all of them at this commit; with no manifest declaring it (a fixture,
        // an older layout) it stays in the named package.
        let declaring = shard_declaring_package_dirs(repo, commit, shard);
        let search_dirs = if declaring.is_empty() {
            scope.clone()
        } else {
            declaring
        };
        let gate = format!("test-shard-{shard}\"");
        let mut grep_args = vec!["-l", "-F", "-e", &gate, commit, "--"];
        grep_args.extend(search_dirs.iter().map(String::as_str));
        let Some((hit, listing)) = git_grep_hits(repo, &grep_args) else {
            return false;
        };
        if !hit {
            return true;
        }
        scope = listing
            .lines()
            .filter_map(|line| line.split_once(':').map(|(_, path)| path.to_string()))
            .collect();
        package_dirs = search_dirs;
    }
    if !lib_family.is_empty() {
        scope.clone_from(&lib_family);
        package_dirs = lib_family;
    }
    pattern
        .split("::")
        .filter(|segment| !segment.is_empty())
        .any(|segment| segment_provably_absent(repo, commit, segment, &scope, &package_dirs))
}

/// Whether the test-name `segment` provably names nothing at `commit` (#1169).
/// A nextest test path is `<module path>::<fn>`: a segment is present when it
/// occurs in the text of a gated file in `scope`, or names a module the way
/// the file tree does (a path component or stem of a `scope` file:
/// `thread_stacks.rs` carries `thread_stacks::tests::...` without ever
/// spelling the name), or is declared `mod <segment>` anywhere in
/// `package_dirs` (a `#[path]`/inline module). Git failing is not a miss.
fn segment_provably_absent(
    repo: &Path,
    commit: &str,
    segment: &str,
    scope: &[String],
    package_dirs: &[String],
) -> bool {
    let mut args = vec!["-q", "-F", "-e", segment, commit, "--"];
    args.extend(scope.iter().map(String::as_str));
    if !matches!(git_grep_hits(repo, &args), Some((false, _))) {
        return false;
    }
    let names_a_module = |path: &str| {
        path.split('/')
            .any(|component| component.strip_suffix(".rs").unwrap_or(component) == segment)
    };
    if scope.iter().any(|path| names_a_module(path)) {
        return false;
    }
    let declaration = format!(r"\bmod[[:space:]]+{segment}\b");
    let mut args = vec!["-q", "-E", "-e", declaration.as_str(), commit, "--"];
    args.extend(package_dirs.iter().map(String::as_str));
    matches!(git_grep_hits(repo, &args), Some((false, _)))
}

/// `crates/rsid` and `crates/rsid-store` as present at `commit` when
/// `package` is one of them (empty otherwise): the packages whose library
/// tests one focused lander run selects from (#1244).
fn rsid_lib_family_dirs(repo: &Path, commit: &str, package: &str) -> Vec<String> {
    if package != "rsid" && package != "rsid-store" {
        return Vec::new();
    }
    ["crates/rsid", "crates/rsid-store"]
        .into_iter()
        .filter(|dir| {
            git(repo, &["ls-tree", "--name-only", commit, dir])
                .is_some_and(|out| !out.trim().is_empty())
        })
        .map(str::to_owned)
        .collect()
}

/// `crates/<package>` for every workspace package whose manifest at `commit`
/// declares the `test-shard-<shard>` feature (empty when none does).
fn shard_declaring_package_dirs(repo: &Path, commit: &str, shard: &str) -> Vec<String> {
    let declaration = format!("test-shard-{shard} = ");
    let Some((true, listing)) = git_grep_hits(
        repo,
        &[
            "-l",
            "-F",
            "-e",
            &declaration,
            commit,
            "--",
            ":(glob)crates/*/Cargo.toml",
        ],
    ) else {
        return Vec::new();
    };
    let mut dirs: Vec<String> = listing
        .lines()
        .filter_map(|line| {
            let (_, path) = line.split_once(':')?;
            Some(path.strip_suffix("/Cargo.toml")?.to_string())
        })
        .collect();
    dirs.sort();
    dirs.dedup();
    dirs
}

/// The schema head at `revision`: the highest `migrations/vNNN.rs`, else the
/// legacy constant in `store/mod.rs`.
fn schema_version_at(repo: &Path, revision: &str) -> Option<u32> {
    // The head comes from the directory the revision's store build consumes:
    // the rsid-store one when populated, else the pre-split one (#1021 S4). A
    // unit in the other directory never runs, so it must not count.
    let head_in = |directory: &str| {
        git(
            repo,
            &["ls-tree", "--name-only", revision, &format!("{directory}/")],
        )
        .and_then(|listing| {
            listing
                .lines()
                .filter_map(|path| {
                    let name = path.rsplit('/').next()?;
                    name.strip_prefix('v')?.strip_suffix(".rs")?.parse().ok()
                })
                .max()
        })
    };
    let from_files = head_in(MIGRATION_DIR).or_else(|| head_in(PRE_SPLIT_MIGRATION_DIR));
    from_files.or_else(|| {
        let text = LEGACY_MIGRATION_FILES
            .iter()
            .find_map(|file| git(repo, &["show", &format!("{revision}:{file}")]))?;
        let marker = "pub const LATEST_SCHEMA_VERSION: i32 = ";
        text.split(marker)
            .nth(1)?
            .split(';')
            .next()?
            .trim()
            .parse()
            .ok()
    })
}

/// Migration version and hot files the source adds relative to rolling.
/// Best effort: any git failure leaves the facts empty.
pub(crate) fn derive_source_facts(repo: &Path, source: &str) -> SourceFacts {
    let base = ["origin/rolling", "rolling"].iter().find_map(|reference| {
        git(repo, &["merge-base", source, reference]).map(|out| out.trim().to_string())
    });
    let Some(base) = base.filter(|base| !base.is_empty()) else {
        return SourceFacts::default();
    };
    let version_at = |revision: &str| schema_version_at(repo, revision);
    let migration_version = match (version_at(source), version_at(&base)) {
        (Some(source_version), Some(base_version)) if source_version > base_version => {
            Some(source_version)
        }
        _ => None,
    };
    let changed = git(
        repo,
        &["diff", "--name-only", "--no-renames", &base, source],
    )
    .unwrap_or_default();
    let hot_files = changed
        .lines()
        .filter(|path| HOT_FILES.contains(path))
        .map(str::to_string)
        .collect();
    SourceFacts {
        migration_version,
        hot_files,
    }
}

/// After a fetch, the published tip when `source` is an ancestor of
/// `origin/rolling`. `None` when it is not (or git cannot say).
pub(crate) fn landed_tip(repo: &Path, source: &str) -> Option<String> {
    landed_tips(repo, &[source.to_string()]).pop().flatten()
}

/// After ONE fetch, the published tip for each source that is an ancestor of
/// `origin/rolling`, in input order (`None`: not landed, or git cannot say).
pub(crate) fn landed_tips(repo: &Path, sources: &[String]) -> Vec<Option<String>> {
    let Some(tip) = fetch_tip(repo) else {
        return vec![None; sources.len()];
    };
    sources
        .iter()
        .map(|source| {
            git(repo, &["merge-base", "--is-ancestor", source, &tip]).map(|_| tip.clone())
        })
        .collect()
}

/// The migration number the lander actually gave `source`, read from the
/// published history: the provisional commit the lander writes has the queue's
/// source as one parent, and the migration file it adds relative to its other
/// parent carries the final number. `None` when no such commit exists (a plain
/// merge or fast-forward keeps the source's own number) or git cannot say.
pub(crate) fn published_migration_number(repo: &Path, source: &str) -> Option<u32> {
    let history = git(
        repo,
        &[
            "log",
            "--format=%H %P",
            "--ancestry-path",
            &format!("{source}..origin/rolling"),
        ],
    )?;
    let (commit, other_parent) = history.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let commit = fields.next()?;
        let parents: Vec<&str> = fields.collect();
        (parents.len() == 2 && parents.contains(&source)).then(|| {
            let other = parents.iter().find(|parent| **parent != source)?;
            Some((commit.to_string(), (*other).to_string()))
        })?
    })?;
    let added = git(
        repo,
        &[
            "diff",
            "--name-only",
            "--no-renames",
            "--diff-filter=A",
            &other_parent,
            &commit,
            "--",
            &format!("{MIGRATION_DIR}/"),
            &format!("{PRE_SPLIT_MIGRATION_DIR}/"),
        ],
    )?;
    added
        .lines()
        .filter_map(|path| {
            let name = path.rsplit('/').next()?;
            name.strip_prefix('v')?
                .strip_suffix(".rs")?
                .parse::<u32>()
                .ok()
        })
        .max()
}

/// Whether `source` adds a provisional-migration declaration relative to
/// rolling: the lander renumbers such a source mechanically at landing.
pub(crate) fn source_declares_provisional(repo: &Path, source: &str) -> bool {
    let base = ["origin/rolling", "rolling"].iter().find_map(|reference| {
        git(repo, &["merge-base", source, reference]).map(|out| out.trim().to_string())
    });
    let Some(base) = base.filter(|base| !base.is_empty()) else {
        return false;
    };
    git(
        repo,
        &["diff", "--name-only", "--no-renames", &base, source],
    )
    .unwrap_or_default()
    .lines()
    .any(|path| path.starts_with(PROVISIONAL_DIR) && path.ends_with(".json"))
}

/// One batch member's migration facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MigrationMember {
    pub id: Uuid,
    /// The schema head the source carries (`None`: no migration).
    pub derived: Option<u32>,
    /// Whether the source declares a provisional migration (renumberable).
    pub declared: bool,
}

/// Migration numbers the queue assigns a batch, in queue order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct MigrationPlan {
    pub assigned: Vec<(Uuid, u32)>,
    /// Members whose fixed number is already taken by a predecessor.
    pub out_of_order: Vec<Uuid>,
}

/// Assign migration numbers tip + 1, + 2, ... in queue order. A declared
/// source is renumbered to the next free number; an undeclared source keeps
/// its fixed number, which must not be one a predecessor already took (the
/// lander's tip + 1 rule then checks every prefix).
pub(crate) fn plan_migration_order(tip_version: u32, members: &[MigrationMember]) -> MigrationPlan {
    let mut plan = MigrationPlan::default();
    let mut next = tip_version + 1;
    for member in members {
        let Some(derived) = member.derived else {
            continue;
        };
        if member.declared {
            plan.assigned.push((member.id, next));
            next += 1;
        } else if derived < next {
            plan.out_of_order.push(member.id);
        } else {
            plan.assigned.push((member.id, derived));
            next = derived + 1;
        }
    }
    plan
}

/// How to launch the lander.
#[derive(Debug, Clone)]
pub struct LanderLauncher {
    binary: PathBuf,
    /// The daemon-owned queue directory (`~/.rsi/queue`): the persistent cargo
    /// target and the detached landing worktrees live under it (#1108). `None`
    /// runs the lander in the entry's own checkout with the daemon's env.
    workspace: Option<PathBuf>,
    /// How the host's Landlock ABI is read before the queue worktree is reset;
    /// replaced only by tests.
    fence_abi: write_fence::AbiProbe,
    /// Wall-time budget of one batch's gating, every lander run included
    /// (#1208). The runner loop refreshes it from the operator setting
    /// `rolling_queue_gate_timeout_mins` before each batch.
    gate_timeout: Duration,
}

impl LanderLauncher {
    #[must_use]
    pub fn new(binary: PathBuf) -> Self {
        Self {
            binary,
            workspace: None,
            fence_abi: WriteFence::kernel_abi,
            gate_timeout: Duration::from_secs(
                u64::from(ROLLING_QUEUE_DEFAULT_GATE_TIMEOUT_MINS) * 60,
            ),
        }
    }

    /// Bound each batch's gating to `timeout` of wall time (#1208).
    #[must_use]
    pub fn with_gate_timeout(mut self, timeout: Duration) -> Self {
        self.gate_timeout = timeout;
        self
    }

    /// The wall-time budget one batch's gating is given.
    #[must_use]
    pub fn gate_timeout(&self) -> Duration {
        self.gate_timeout
    }

    /// Run landings in daemon-owned worktrees and a dedicated cargo target
    /// under `root`, never in the entry's checkout (#1108).
    #[must_use]
    pub fn with_workspace(mut self, root: PathBuf) -> Self {
        self.workspace = Some(root);
        self
    }

    /// The persistent cargo target the lander is given, when workspace-owned.
    #[must_use]
    pub fn target_dir(&self) -> Option<PathBuf> {
        self.workspace.as_ref().map(|root| root.join("target"))
    }

    /// Where each lander run's stdout and stderr are kept, when workspace-owned.
    #[must_use]
    pub fn log_dir(&self) -> Option<PathBuf> {
        self.workspace.as_ref().map(|root| root.join("logs"))
    }

    /// The lander executable this launcher runs.
    #[must_use]
    pub fn binary(&self) -> &Path {
        &self.binary
    }

    /// `RSI_ROLLING_LAND_BIN`, the daemon's sibling binary, the shared debug
    /// build, then `PATH`; always with the daemon-owned queue workspace.
    #[must_use]
    pub fn discover() -> Self {
        Self::discover_binary().with_workspace(rsi_common::identity::data_dir().join("queue"))
    }

    fn discover_binary() -> Self {
        if let Some(path) = std::env::var_os("RSI_ROLLING_LAND_BIN") {
            return Self::new(PathBuf::from(path));
        }
        let sibling = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join("rsi-rolling-land")))
            .filter(|path| path.is_file());
        if let Some(path) = sibling {
            return Self::new(path);
        }
        let shared = std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(".cargo/shared-target/debug/rsi-rolling-land"))
            .filter(|path| path.is_file());
        Self::new(shared.unwrap_or_else(|| PathBuf::from("rsi-rolling-land")))
    }
}

/// FNV-1a: a stable, dependency-free key for a repository's git directory.
fn stable_key(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn git_ok(repo: &Path, args: &[&str]) -> std::result::Result<String, String> {
    let output = git_command()
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("cannot run git: {error}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// The queue's one fetch of the source repository's `origin/rolling`.
///
/// The source (an operator checkout or a session sandbox) is only read by the
/// queue, but a fetch necessarily writes its object store and its
/// `refs/remotes/origin/rolling`. It is NOT run under the write fence (#1170):
/// the transport children (`ssh`, credential helpers, `git-remote-https`) write
/// their own state (`known_hosts`, control sockets, credential caches) and
/// inherit a fence, and a ref update needs the `packed-refs` lock in the common
/// directory root, which a directory grant cannot give without also granting
/// `config` and `hooks`. A read-only copy (fetching into a private directory
/// with the source's objects as an alternate) would leave the fetched commits
/// out of the shared store the lander and the queue worktree read, so it is not
/// done either. What is done instead is to remove every way the SOURCE's own
/// configuration can make this fetch run a program or write elsewhere:
/// hooks (`reference-transaction`, `pre-auto-gc`), `fsmonitor`, automatic
/// `gc`/maintenance (which can repack the operator's store in the background)
/// and the commit-graph, submodule recursion, tag following, `FETCH_HEAD` (no
/// reader needs it: the tip is read from `origin/rolling`) and the `ext::`
/// transport are all off on the command line, which wins over every config
/// file. The daemon's environment is already reduced to the allowlist. The
/// source's transport settings (`core.sshCommand`, `core.gitProxy`,
/// `remote.origin.uploadpack`, credential helpers, `url.*.insteadOf`) are left
/// alone on purpose: they carry the operator's authentication (see
/// `write_fence.rs` for the full trade-off).
const FETCH_ROLLING: &[&str] = &[
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "gc.auto=0",
    "-c",
    "maintenance.auto=false",
    "-c",
    "fetch.writeCommitGraph=false",
    "-c",
    "fetch.recurseSubmodules=false",
    "-c",
    "protocol.ext.allow=never",
    "fetch",
    "--quiet",
    "--no-write-fetch-head",
    "--no-recurse-submodules",
    "--no-auto-maintenance",
    "--no-auto-gc",
    "--no-tags",
    "origin",
    "rolling",
];

fn fetch_rolling(repo: &Path) -> std::result::Result<String, String> {
    git_ok(repo, FETCH_ROLLING)
}

/// The daemon-owned landing worktree for the repository `source` belongs to: a
/// detached checkout of the fetched `origin/rolling`, created once and
/// refreshed on every call so builds stay warm. `source` (an operator checkout
/// or a session sandbox) is only read: the worktree shares its object store and
/// remotes, so any accepted source commit is reachable here, but no file in
/// `source` is ever touched. Callers serialize use (the queue runs one batch at
/// a time).
pub(crate) fn ensure_queue_worktree(
    root: &Path,
    source: &Path,
) -> std::result::Result<PathBuf, String> {
    ensure_queue_worktree_with(root, source, WriteFence::kernel_abi)
}

/// [`ensure_queue_worktree`] with the Landlock ABI probe injected (tests).
fn ensure_queue_worktree_with(
    root: &Path,
    source: &Path,
    fence_abi: write_fence::AbiProbe,
) -> std::result::Result<PathBuf, String> {
    let common = git_ok(
        source,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    // Fail closed before any git command runs in (or deletes) a queue path:
    // the workspace, its worktrees directory and the worktree are real,
    // owned directories, never symlinks or aliases (#1113).
    prepare_queue_workspace(root)?;
    // The fence guards every reset; a host that cannot enforce it is refused
    // before any worktree is created or touched (#1160).
    write_fence::require_abi(fence_abi())?;
    let path = root.join("worktrees").join(stable_key(&common));
    let rolling = match fetch_rolling(source) {
        Ok(_) => "origin/rolling^{commit}",
        // Offline or no remote: the lander reports the real error; keep the
        // last fetched tip (or the source HEAD) so the worktree still exists.
        Err(_) => "HEAD",
    };
    let rev = match git_ok(source, &["rev-parse", "--verify", rolling]) {
        Ok(rev) => rev,
        Err(_) => git_ok(source, &["rev-parse", "--verify", "HEAD"])?,
    };
    if path.symlink_metadata().is_ok() {
        match pin_queue_worktree(root, &path, &common) {
            Ok(mut pinned) => {
                pinned.fence_abi = fence_abi;
                reset_pinned_worktree(&pinned, &rev)?;
                return Ok(path);
            }
            // An empty directory (a crashed `worktree add`) is safe to
            // retire; anything else is refused with nothing touched.
            Err(refusal) => remove_empty_queue_dir(root, &path).map_err(|_| refusal)?,
        }
    }
    // `git worktree add` registers by path (R1 residual of #1170, see
    // `write_fence.rs`); it runs with hooks, fsmonitor and attributes off on
    // the command line and checks nothing out, and the population below is the
    // fenced, pinned runner.
    let _ = git_ok(source, &["worktree", "prune"]);
    create_queue_registration(source, &path, &rev)?;
    // The registration exists but nothing is checked out: populate it exactly
    // like every later reset, through the pinned, fenced runner (#1160).
    let mut pinned = pin_queue_worktree(root, &path, &common)?;
    pinned.fence_abi = fence_abi;
    reset_pinned_worktree(&pinned, &rev)?;
    Ok(path)
}

/// Register a detached worktree at `path` without checking anything out: git
/// then creates only the registration and the `.git` pointer, and runs no
/// checkout, hook or filter. The hook and filter settings are also disabled on
/// the command line, so nothing the repository configures can run here.
fn create_queue_registration(
    source: &Path,
    path: &Path,
    rev: &str,
) -> std::result::Result<(), String> {
    let path_arg = path.display().to_string();
    git_ok(
        source,
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.attributesFile=/dev/null",
            "worktree",
            "add",
            "--quiet",
            "--no-checkout",
            "--detach",
            "--force",
            &path_arg,
            rev,
        ],
    )
    .map(drop)
}

/// Retire an empty queue slot without trusting any path: open the vetted
/// `worktrees` directory without following symlinks, check the slot through
/// that handle (`fstatat`, no-follow) and remove it with `unlinkat` relative to
/// the same handle. `AT_REMOVEDIR` never removes a symlink or a non-empty
/// directory, so a swap of the slot or of `worktrees` after the check cannot
/// redirect the removal (#1141).
fn remove_empty_queue_dir(root: &Path, path: &Path) -> std::result::Result<(), String> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    let refuse = |why: &str| {
        Err(format!(
            "cannot retire queue slot {}: {why}",
            path.display()
        ))
    };
    let Some(name) = path.file_name() else {
        return refuse("no file name");
    };
    let parent_path = root.join("worktrees");
    if path.parent() != Some(parent_path.as_path()) {
        return refuse("not directly inside the worktrees directory");
    }
    let parent = open_queue_dir(&parent_path)?;
    let parent_meta = parent
        .metadata()
        .map_err(|error| format!("cannot stat {}: {error}", parent_path.display()))?;
    // SAFETY: `geteuid` has no preconditions and does not mutate state.
    let uid = unsafe { nix::libc::geteuid() };
    if parent_meta.uid() != uid {
        return refuse("the worktrees directory is not owned by the daemon user");
    }
    let real_parent = PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd()))
        .canonicalize()
        .map_err(|error| format!("cannot resolve {}: {error}", parent_path.display()))?;
    let real_root = root
        .canonicalize()
        .map_err(|error| format!("cannot resolve {}: {error}", root.display()))?;
    if real_parent != real_root.join("worktrees") {
        return refuse("the worktrees directory is not the queue's");
    }
    let name = CString::new(name.as_bytes()).map_err(|_| "slot name has a NUL".to_string())?;
    // SAFETY: `stat` is plain old data, `name` is NUL-terminated and the parent
    // handle stays open for both calls.
    let removed = unsafe {
        let mut stat: nix::libc::stat = std::mem::zeroed();
        if nix::libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            &mut stat,
            nix::libc::AT_SYMLINK_NOFOLLOW,
        ) != 0
        {
            return refuse("the slot is unreadable");
        }
        if stat.st_mode & nix::libc::S_IFMT != nix::libc::S_IFDIR || stat.st_uid != uid {
            return refuse("the slot is not an owned directory");
        }
        nix::libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), nix::libc::AT_REMOVEDIR)
    };
    if removed != 0 {
        return refuse(&std::io::Error::last_os_error().to_string());
    }
    Ok(())
}

/// `path` is a real directory (no-follow metadata) owned by this process.
fn vet_queue_dir(path: &Path) -> std::result::Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let meta = path
        .symlink_metadata()
        .map_err(|error| format!("queue path {} unreadable: {error}", path.display()))?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(format!(
            "queue path {} is a symlink or not a directory; refusing to use it",
            path.display()
        ));
    }
    // SAFETY: `geteuid` has no preconditions and does not mutate state.
    let uid = unsafe { nix::libc::geteuid() };
    if meta.uid() != uid {
        return Err(format!(
            "queue path {} is not owned by the daemon user; refusing to use it",
            path.display()
        ));
    }
    Ok(())
}

/// Create (when absent) and vet the queue workspace: the root, its `worktrees`
/// directory and its `target` directory. Parents of the root belong to the
/// daemon's data directory and are not inspected.
pub(crate) fn prepare_queue_workspace(root: &Path) -> std::result::Result<(), String> {
    if root.symlink_metadata().is_err() {
        std::fs::create_dir_all(root)
            .map_err(|error| format!("cannot create the queue workspace: {error}"))?;
    }
    vet_queue_dir(root)?;
    for name in ["worktrees", "target"] {
        let child = root.join(name);
        if child.symlink_metadata().is_err() {
            std::fs::create_dir(&child)
                .map_err(|error| format!("cannot create the queue workspace: {error}"))?;
        }
        vet_queue_dir(&child)?;
    }
    Ok(())
}

mod private_git_dir;
mod write_fence;
use private_git_dir::PrivateGitDir;
use write_fence::WriteFence;

/// A queue worktree validated once and then held by open directory handles.
/// Destructive git commands run from the handles, never from mutable paths, so a
/// slot or a registration swapped for a symlink after validation cannot
/// redirect them (#1126, #1141).
pub(crate) struct PinnedWorktree {
    dir: std::fs::File,
    /// The worktree registration (`<common>/worktrees/<name>`) opened without
    /// following a symlink. Git is pointed at the handle (`GIT_DIR=/proc/self/fd/N`),
    /// so the registration is never resolved by path again.
    admin_dir: std::fs::File,
    /// Where the registration was validated; the path is only used to detect a
    /// swap, never to run git.
    admin: PathBuf,
    admin_id: (u64, u64),
    /// The shared object store (`<common>/objects`), read by the private git
    /// directory's git through `GIT_OBJECT_DIRECTORY`.
    objects: PathBuf,
    /// How the kernel's Landlock ABI is read; replaced only by tests.
    fence_abi: write_fence::AbiProbe,
    /// The environment the fenced git is filtered from instead of the daemon's
    /// own (`None` in production); tests use it to prove an inherited
    /// `GIT_TRACE` is never honored.
    inherited_env: Option<Vec<(std::ffi::OsString, std::ffi::OsString)>>,
}

impl PinnedWorktree {
    /// `/proc/self/fd/N` names the pinned inode, not whatever `path` now is.
    fn handle_path(&self) -> PathBuf {
        use std::os::fd::AsRawFd;
        PathBuf::from(format!("/proc/self/fd/{}", self.dir.as_raw_fd()))
    }

    fn admin_handle_path(&self) -> PathBuf {
        use std::os::fd::AsRawFd;
        PathBuf::from(format!("/proc/self/fd/{}", self.admin_dir.as_raw_fd()))
    }

    /// The registration path still names the pinned inode: same device and
    /// inode, not a symlink, not moved. Fail closed otherwise.
    fn ensure_admin_unchanged(&self) -> std::result::Result<(), String> {
        use std::os::unix::fs::MetadataExt;
        let swapped = |why: &str| {
            Err(format!(
                "queue worktree registration {} changed under the queue ({why}); refusing to run git",
                self.admin.display()
            ))
        };
        let Ok(meta) = self.admin.symlink_metadata() else {
            return swapped("missing");
        };
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return swapped("symlink or not a directory");
        }
        if (meta.dev(), meta.ino()) != self.admin_id {
            return swapped("identity changed");
        }
        match self.admin_handle_path().canonicalize() {
            Ok(real) if real == self.admin => Ok(()),
            _ => swapped("moved"),
        }
    }
}

/// Open `path` as a directory without following a final symlink.
fn open_queue_dir(path: &Path) -> std::result::Result<std::fs::File, String> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| {
            format!(
                "queue path {} is a symlink or not a directory ({error}); refusing to use it",
                path.display()
            )
        })
}

/// `path` is a worktree this queue created: a real owned directory inside the
/// workspace whose `.git` file points at a registered worktree of `common`,
/// and whose registration points back at `path`. Every check runs against one
/// no-follow directory handle, which is returned so the caller keeps using
/// exactly the inode that was validated.
fn pin_queue_worktree(
    root: &Path,
    path: &Path,
    common: &str,
) -> std::result::Result<PinnedWorktree, String> {
    use std::os::unix::fs::MetadataExt;
    let dir = open_queue_dir(path)?;
    let meta = dir
        .metadata()
        .map_err(|error| format!("queue path {} unreadable: {error}", path.display()))?;
    // SAFETY: `geteuid` has no preconditions and does not mutate state.
    if meta.uid() != unsafe { nix::libc::geteuid() } {
        return Err(format!(
            "queue path {} is not owned by the daemon user; refusing to use it",
            path.display()
        ));
    }
    let handle = {
        use std::os::fd::AsRawFd;
        PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()))
    };
    let refuse = |why: &str| {
        Err(format!(
            "queue worktree {} is not a registered worktree of this queue ({why}); refusing to reset it",
            path.display()
        ))
    };
    // The handle resolves to the pinned inode's real location.
    let real = handle
        .canonicalize()
        .map_err(|error| format!("cannot resolve {}: {error}", path.display()))?;
    let real_root = root
        .canonicalize()
        .map_err(|error| format!("cannot resolve {}: {error}", root.display()))?;
    if !real.starts_with(real_root.join("worktrees")) {
        return refuse("outside the queue workspace");
    }
    let dot_git = handle.join(".git");
    match dot_git.symlink_metadata() {
        Ok(meta) if meta.is_file() => {}
        _ => return refuse("no regular .git file"),
    }
    let pointer = std::fs::read_to_string(&dot_git)
        .map_err(|error| format!("cannot read {}: {error}", dot_git.display()))?;
    let Some(admin) = pointer.trim().strip_prefix("gitdir:").map(str::trim) else {
        return refuse("malformed .git file");
    };
    // Pin the registration itself: opened without following a final symlink,
    // then every check below reads through the handle, so a swap of the
    // registration path after this point cannot change what is validated or
    // what git later uses.
    let Ok(admin_dir) = open_queue_dir(Path::new(admin)) else {
        return refuse("registration is missing or is a symlink");
    };
    let admin_meta = admin_dir.metadata().map_err(|error| {
        format!(
            "cannot stat the registration of {}: {error}",
            path.display()
        )
    })?;
    // SAFETY: `geteuid` has no preconditions and does not mutate state.
    if admin_meta.uid() != unsafe { nix::libc::geteuid() } {
        return refuse("registration is not owned by the daemon user");
    }
    let admin_handle = {
        use std::os::fd::AsRawFd;
        PathBuf::from(format!("/proc/self/fd/{}", admin_dir.as_raw_fd()))
    };
    let (Ok(admin_real), Ok(common)) = (
        admin_handle.canonicalize(),
        Path::new(common).canonicalize(),
    ) else {
        return refuse("registration is missing");
    };
    if admin_real.parent() != Some(common.join("worktrees").as_path()) {
        return refuse("gitdir is not a worktree registration of the source repository");
    }
    let back = std::fs::read_to_string(admin_handle.join("gitdir")).unwrap_or_default();
    if Path::new(back.trim()).canonicalize().ok().as_deref() != Some(real.join(".git").as_path()) {
        return refuse("registration does not point back at the worktree");
    }
    let pinned = PinnedWorktree {
        dir,
        admin_dir,
        admin: admin_real,
        admin_id: (admin_meta.dev(), admin_meta.ino()),
        objects: common.join("objects"),
        fence_abi: WriteFence::kernel_abi,
        inherited_env: None,
    };
    pinned.ensure_admin_unchanged()?;
    Ok(pinned)
}

/// Run git inside the pinned worktree against the private git directory: the
/// child's current directory comes from the inherited worktree handle and
/// `GIT_DIR` names the inherited private-directory handle, so neither the
/// worktree path nor any registration path is resolved again. The child gets no
/// operator config, hooks or filters (the private directory has its own config
/// and no hooks; global and system config are off) and runs under a kernel
/// write fence that permits writes only beneath those two handles (#1160). The
/// registration identity is also checked before and after, failing closed on a
/// swap. `env` is extra environment for the child (tests park git between its
/// work-tree normalization and its `chdir`).
fn git_private(
    pinned: &PinnedWorktree,
    private: &PrivateGitDir,
    args: &[&str],
    env: &[(String, String)],
) -> std::result::Result<String, String> {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    pinned.ensure_admin_unchanged()?;
    let fence = WriteFence::new(&[&pinned.dir, &private.dir], pinned.fence_abi)?;
    let private_fd = private.dir.as_raw_fd();
    let mut command = std::process::Command::new("git");
    // A clean environment: only what git needs to start, the inherited names
    // the test seam names (production inherits none), and what is set below.
    command.env_clear();
    let raw = pinned
        .inherited_env
        .clone()
        .unwrap_or_else(|| std::env::vars_os().collect());
    command.envs(inherited_git_env(raw).into_iter().filter(|(name, _)| {
        let name = name.to_string_lossy();
        name == "PATH" || name == "LANG" || name == "TZ" || name.starts_with("LC_")
    }));
    // The settings that stop an external program or an append-in-place from
    // being configured are on the command line, which wins over the private
    // directory's own config file.
    command
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.attributesFile=/dev/null",
            "-c",
            "core.logAllRefUpdates=false",
            "-c",
            "core.sharedRepository=false",
        ])
        .args(args)
        .current_dir(pinned.handle_path());
    command
        .env("HOME", "/dev/null")
        .env("GIT_DIR", private.handle_path())
        // Naming the common directory stops git from honoring a `commondir`
        // file planted in the private directory, which would redirect config,
        // refs and `info/` to wherever it points; replace refs are ignored for
        // the same reason (#1170).
        .env("GIT_COMMON_DIR", private.handle_path())
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_WORK_TREE", ".")
        .env("GIT_OBJECT_DIRECTORY", &pinned.objects)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .envs(
            env.iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        )
        .stdin(Stdio::null());
    // SAFETY: the closure runs between fork and exec and only calls `fcntl`,
    // which is async-signal-safe. It lets the child (and git's own children)
    // inherit the private-directory handle that `GIT_DIR` names; the parent's
    // copy stays close-on-exec so no unrelated process inherits it.
    unsafe {
        command.pre_exec(move || {
            let flags = nix::libc::fcntl(private_fd, nix::libc::F_GETFD);
            if flags < 0
                || nix::libc::fcntl(
                    private_fd,
                    nix::libc::F_SETFD,
                    flags & !nix::libc::FD_CLOEXEC,
                ) < 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    fence.confine_on_exec(&mut command);
    let output = command.output().map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => format!("cannot run git: {error}"),
        // `pre_exec` (no-new-privs, `landlock_restrict_self`) or exec failed:
        // git did not run, and the fence is what could not be applied.
        _ => format!(
            "{}: cannot start git under the write fence: {error}",
            write_fence::UNSUPPORTED_CODE
        ),
    })?;
    pinned.ensure_admin_unchanged()?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_string());
    }
    Err(format!(
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

/// Reset the pinned queue worktree to `rev` and drop untracked files.
///
/// git never touches the registration: it works in a fresh private git
/// directory seeded with a copy of the registration's index, and the resulting
/// `HEAD` and index are published back by exclusive create plus rename relative
/// to the pinned registration handle. So nothing in the registration, nor any
/// operator file a link there aliases, is ever written in place.
fn reset_pinned_worktree(pinned: &PinnedWorktree, rev: &str) -> std::result::Result<(), String> {
    reset_pinned_worktree_with_env(pinned, rev, &|_| Vec::new(), &|_, _| {})
}

/// [`reset_pinned_worktree`] with test seams: extra child environment per git
/// subcommand, and a callback run just before each subcommand with the private
/// directory's path (tests plant files in it, as a racing same-uid process
/// would).
fn reset_pinned_worktree_with_env(
    pinned: &PinnedWorktree,
    rev: &str,
    env_for: &dyn Fn(&str) -> Vec<(String, String)>,
    before_git: &dyn Fn(&str, &Path),
) -> std::result::Result<(), String> {
    pinned.ensure_admin_unchanged()?;
    private_git_dir::retire_stale_scratch(&pinned.admin_dir);
    let private = PrivateGitDir::create(&pinned.admin_dir)?;
    let outcome = reset_in_private_dir(pinned, &private, rev, env_for, before_git);
    let removed = private.remove(&pinned.admin_dir);
    outcome?;
    removed
}

fn reset_in_private_dir(
    pinned: &PinnedWorktree,
    private: &PrivateGitDir,
    rev: &str,
    env_for: &dyn Fn(&str) -> Vec<(String, String)>,
    before_git: &dyn Fn(&str, &Path),
) -> std::result::Result<(), String> {
    let run = |args: &[&str]| {
        before_git(args[0], &private.handle_path());
        git_private(pinned, private, args, &env_for(args[0]))
    };
    let commit = run(&["rev-parse", "--verify", &format!("{rev}^{{commit}}")])?;
    // `read-tree` updates the index and the worktree and touches no ref, so no
    // reflog is ever opened (`reset --hard` would append to `logs/HEAD`).
    run(&["read-tree", "--reset", "-u", &commit])?;
    run(&["clean", "--quiet", "-fd"])?;
    pinned.ensure_admin_unchanged()?;
    private_git_dir::replace_file_at(&pinned.admin_dir, "index", &private.index()?, 0o644)?;
    private_git_dir::replace_file_at(
        &pinned.admin_dir,
        "HEAD",
        format!("{commit}\n").as_bytes(),
        0o644,
    )
}

/// The repository checkout a claimed batch runs in: the daemon-owned worktree
/// when the launcher owns a workspace, else the entry's own checkout.
async fn queue_repo(
    launcher: &LanderLauncher,
    repo_path: &str,
) -> std::result::Result<PathBuf, String> {
    let Some(root) = launcher.workspace.clone() else {
        return Ok(PathBuf::from(repo_path));
    };
    let source = PathBuf::from(repo_path);
    let fence_abi = launcher.fence_abi;
    tokio::task::spawn_blocking(move || {
        prepare_queue_workspace(&root)?;
        ensure_queue_worktree_with(&root, &source, fence_abi)
    })
    .await
    .map_err(|error| format!("queue workspace task: {error}"))?
}

/// The refusal code a failed queue workspace settles with: a host that cannot
/// enforce the write fence gets its own typed code (#1160), anything else is
/// the generic `lander_unavailable`.
fn workspace_refusal_code(message: &str) -> &'static str {
    match message.starts_with(write_fence::UNSUPPORTED_CODE) {
        true => write_fence::UNSUPPORTED_CODE,
        false => "lander_unavailable",
    }
}

/// What a lander run resolved to, before settlement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunResult {
    pub state: RollingQueueEntryState,
    pub outcome: RollingQueueOutcome,
}

pub(crate) fn tail(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.len() <= OUTCOME_TAIL_BYTES {
        return trimmed.to_string();
    }
    let mut start = trimmed.len() - OUTCOME_TAIL_BYTES;
    while !trimmed.is_char_boundary(start) {
        start += 1;
    }
    trimmed[start..].to_string()
}

/// The last `key=value` line's value: a gate re-run after a stale advance
/// prints its timing again, and the final line is the one that decided the run.
pub(crate) fn last_line_value<'a>(stdout: &'a str, key: &str) -> Option<&'a str> {
    stdout
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
}

pub(crate) fn line_value<'a>(stdout: &'a str, key: &str) -> Option<&'a str> {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
}

/// The marker the lander appends to an integration conflict message: the one
/// accepted source whose merge conflicted with the target plus the sources
/// merged before it. A batch refuses only that member (#1119).
pub const CONFLICT_SOURCE_KEY: &str = "conflict_source";

/// Append the conflicting source to a lander `error` that is an integration
/// conflict; any other error is returned unchanged.
#[must_use]
pub fn mark_conflict_source(error: String, source: &str) -> String {
    if !error.contains("Conflict {") {
        return error;
    }
    format!("{error}; {CONFLICT_SOURCE_KEY}={source}")
}

/// The source a lander conflict message names (`None`: an older lander, or
/// not a conflict).
#[must_use]
pub fn conflict_source(stderr: &str) -> Option<String> {
    let marker = format!("{CONFLICT_SOURCE_KEY}=");
    let (_, rest) = stderr.rsplit_once(&marker)?;
    let source: String = rest
        .chars()
        .take_while(char::is_ascii_alphanumeric)
        .collect();
    Some(source).filter(|source| !source.is_empty())
}

/// The receipt key for one failing test: the lander prints one
/// `failing_test=<name>` stdout line per new failure it refuses on.
pub const FAILING_TEST_KEY: &str = "failing_test";

/// One machine-readable receipt line for a failing test.
#[must_use]
pub fn failing_test_line(name: &str) -> String {
    format!("{FAILING_TEST_KEY}={name}")
}

/// The failing tests a run reported: the lander's `failing_test=` receipt
/// lines first, else the raw libtest `test ... FAILED` lines.
#[must_use]
pub fn failing_tests(text: &str) -> Vec<String> {
    let marked: Vec<String> = text
        .lines()
        .filter_map(|line| line_value(line.trim(), FAILING_TEST_KEY))
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect();
    let mut names: Vec<String> = if marked.is_empty() {
        text.lines()
            .filter_map(|line| {
                let rest = line.trim().strip_prefix("test ")?;
                rest.strip_suffix(" ... FAILED").map(str::to_string)
            })
            .collect()
    } else {
        marked
    };
    names.sort();
    names.dedup();
    names.truncate(MAX_FAILING_TESTS);
    names
}

/// Map one lander exit (`exit`, stdout evidence lines, stderr message) to a
/// typed result. `landed` is the ancestry probe used when publication is
/// uncertain, so a push that succeeded before the lander could report it is
/// still recorded as published.
pub(crate) fn classify_run(
    exit: Option<i32>,
    stdout: &str,
    stderr: &str,
    landed: impl FnOnce() -> Option<String>,
) -> RunResult {
    // The lander's gate timing (#1132): shards, base cache hits and runs,
    // prebuild and wall seconds. Kept in the outcome detail so the manager can
    // read the queue ETA from the entry row.
    let timing = last_line_value(stdout, "gate_timing").map(|value| format!("gate_timing={value}"));
    let published = |sha: String| RunResult {
        state: RollingQueueEntryState::Published,
        outcome: RollingQueueOutcome {
            landed_sha: Some(sha),
            detail: timing.clone(),
            ..RollingQueueOutcome::default()
        },
    };
    let detail = match (Some(tail(stderr)).filter(|text| !text.is_empty()), &timing) {
        (Some(text), Some(timing)) => Some(format!("{timing}\n{text}")),
        (Some(text), None) => Some(text),
        (None, timing) => timing.clone(),
    };
    let refuse = |code: &str, state: RollingQueueEntryState| RunResult {
        state,
        outcome: RollingQueueOutcome {
            landed_sha: None,
            refusal: Some(code.to_string()),
            failing_tests: failing_tests(&format!("{stdout}\n{stderr}")),
            detail: detail.clone(),
        },
    };
    if exit == Some(0) {
        return match line_value(stdout, "published_target_id") {
            Some(tip) => published(tip.to_string()),
            None => refuse("lander_report_missing_tip", RollingQueueEntryState::Failed),
        };
    }
    let status = line_value(stdout, "publication_status");
    if matches!(status, Some("published" | "unknown")) {
        // The push may have landed before the report failed: trust ancestry.
        return match landed() {
            Some(tip) => published(tip),
            None => refuse("publication_unknown", RollingQueueEntryState::Failed),
        };
    }
    if stderr.contains("stale retries exhausted") {
        return refuse(QUEUE_REGATE_EXHAUSTED, RollingQueueEntryState::Refused);
    }
    if stderr.contains(ALREADY_INTEGRATED_MARKER) {
        // Already on rolling: the landing happened, just not through this run
        // (#1208). Never a red gate.
        return match landed() {
            Some(tip) => RunResult {
                state: RollingQueueEntryState::Published,
                outcome: RollingQueueOutcome {
                    landed_sha: Some(tip),
                    detail: Some("already on origin/rolling: landed outside the queue".into()),
                    ..RollingQueueOutcome::default()
                },
            },
            None => refuse(
                QUEUE_SOURCE_ALREADY_INTEGRATED,
                RollingQueueEntryState::Refused,
            ),
        };
    }
    if stderr.contains("Conflict {") {
        return refuse(QUEUE_BATCH_MERGE_CONFLICT, RollingQueueEntryState::Refused);
    }
    if stderr.contains("error: no tests to run") {
        return refuse(
            QUEUE_FILTER_MATCHES_NO_TESTS,
            RollingQueueEntryState::Refused,
        );
    }
    if exit == Some(8) {
        let fence = line_value(stdout, "policy_fence").unwrap_or("unknown");
        return refuse(
            &format!("policy_refused:{fence}"),
            RollingQueueEntryState::Refused,
        );
    }
    refuse("gate_failed", RollingQueueEntryState::Refused)
}

/// Whether a changed path can alter what the Rust gate builds or tests: any
/// Rust source (migrations and `build.rs` included), a Cargo manifest or lock,
/// the toolchain pin, or cargo configuration.
fn path_affects_rust_gate(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    path.ends_with(".rs")
        || name == "Cargo.toml"
        || name == "Cargo.lock"
        || name == "rust-toolchain.toml"
        || name == "rust-toolchain"
        || path.starts_with(".cargo/")
}

/// Whether `source` is provably free of Rust, Cargo and migration changes
/// relative to rolling. Any git failure answers `false`: the member is then
/// treated as code, so an unknown diff never narrows the gate.
pub(crate) fn source_is_rust_free(repo: &Path, source: &str) -> bool {
    let base = ["origin/rolling", "rolling"].iter().find_map(|reference| {
        git(repo, &["merge-base", source, reference]).map(|out| out.trim().to_string())
    });
    let Some(base) = base.filter(|base| !base.is_empty()) else {
        return false;
    };
    git(
        repo,
        &["diff", "--name-only", "--no-renames", &base, source],
    )
    .is_some_and(|changed| !changed.lines().any(path_affects_rust_gate))
}

/// Union of the members' filters. A member with Rust changes and no filters
/// makes the batch run the lander's full affected gate; a member whose diff
/// touches no Rust, Cargo or migration file (`rust_free`) cannot change what
/// the gate tests, so it contributes only the filters it carries and never
/// widens the batch (#1527).
pub(crate) fn union_filters(
    entries: &[RollingQueueEntryV1],
    rust_free: impl Fn(&RollingQueueEntryV1) -> bool,
) -> Vec<String> {
    if entries
        .iter()
        .any(|entry| entry.test_filters.is_empty() && !rust_free(entry))
    {
        return Vec::new();
    }
    let mut union: Vec<String> = Vec::new();
    for filter in entries.iter().flat_map(|entry| &entry.test_filters) {
        if !union.contains(filter) {
            union.push(filter.clone());
        }
    }
    union
}

/// Keep the newest `max` bytes of `text`.
fn newest_bytes(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// Delete all but the newest `keep` regular files in `dir`.
fn prune_logs(dir: &Path, keep: usize) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = read
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            meta.is_file().then(|| {
                (
                    meta.modified().unwrap_or(std::time::UNIX_EPOCH),
                    entry.path(),
                )
            })
        })
        .collect();
    if files.len() <= keep {
        return;
    }
    files.sort();
    let excess = files.len() - keep;
    for (_, path) in files.into_iter().take(excess) {
        let _ = std::fs::remove_file(path);
    }
}

static RUN_LOG_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Write one lander run's output to `<dir>/<batch>-<run>.log` (bounded, the
/// directory rotated) and return its path. Best effort: `None` on an I/O error.
fn write_run_log(
    dir: &Path,
    batch_id: Uuid,
    header: &str,
    stdout: &str,
    stderr: &str,
) -> Option<PathBuf> {
    std::fs::create_dir_all(dir).ok()?;
    let seq = RUN_LOG_SEQ.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!(
        "{batch_id}-{}-{seq}.log",
        Utc::now().timestamp_millis()
    ));
    let body = format!(
        "{header}\n--- stdout ---\n{}\n--- stderr ---\n{}\n",
        newest_bytes(stdout, RUN_LOG_STREAM_BYTES),
        newest_bytes(stderr, RUN_LOG_STREAM_BYTES),
    );
    std::fs::write(&path, body).ok()?;
    prune_logs(dir, RUN_LOG_KEEP_FILES);
    Some(path)
}

/// The conflicting paths a lander conflict message names
/// (`Conflict { paths: ["a", "b"] }`).
fn conflict_paths(stderr: &str) -> Option<String> {
    let (_, rest) = stderr.split_once("Conflict { paths: [")?;
    let (list, _) = rest.split_once(']')?;
    let paths: Vec<&str> = list
        .split(',')
        .map(|path| path.trim().trim_matches('"'))
        .filter(|path| !path.is_empty())
        .collect();
    (!paths.is_empty()).then(|| paths.join(","))
}

/// A short, human-readable cause for a non-green run: the refusal code, the
/// conflicting paths or the first failing tests (#1135).
fn run_cause(result: &RunResult, stderr: &str) -> Option<String> {
    let refusal = result.outcome.refusal.as_deref()?;
    let mut cause = refusal.to_string();
    if let Some(paths) = conflict_paths(stderr) {
        cause.push_str(&format!("; conflict_paths={paths}"));
    }
    let tests = &result.outcome.failing_tests;
    if !tests.is_empty() {
        let shown: Vec<&str> = tests.iter().take(CAUSE_TESTS).map(String::as_str).collect();
        cause.push_str(&format!("; failing_tests={}", shown.join(",")));
        if tests.len() > CAUSE_TESTS {
            cause.push_str(&format!(" (+{} more)", tests.len() - CAUSE_TESTS));
        }
    }
    Some(cause)
}

/// Why a lander run produced no report.
#[derive(Debug)]
enum LanderError {
    /// The lander could not be started or waited for.
    Unavailable(String),
    /// The batch's gate wall-time budget ran out (#1208); the run (if any) was
    /// stopped.
    TimedOut,
}

async fn run_lander(
    launcher: &LanderLauncher,
    repo: &Path,
    entries: &[RollingQueueEntryV1],
    regates_left: usize,
    expected_tip: Option<&str>,
    deadline: tokio::time::Instant,
) -> std::result::Result<std::process::Output, LanderError> {
    let budget = deadline.saturating_duration_since(tokio::time::Instant::now());
    if budget.is_zero() {
        return Err(LanderError::TimedOut);
    }
    let mut command = Command::new(&launcher.binary);
    command
        .current_dir(repo)
        .arg("--repo")
        .arg(repo)
        .args(["--remote", "origin"]);
    for entry in entries {
        command.args(["--accepted", &entry.source_commit]);
    }
    for filter in union_filters(entries, |entry| {
        source_is_rust_free(repo, &entry.source_commit)
    }) {
        command.args(["--test-filter", &filter]);
    }
    if let Some(tip) = expected_tip {
        // The tip the queue probed just before this run: a different initial
        // fetch is an out-of-band advance the lander charges as a re-gate.
        command.args(["--expected-tip", tip]);
    }
    command
        .env(MAX_REGATES_ENV, regates_left.to_string())
        .env(EXACT_GATE_ENV, "1")
        .env_remove("RSI_SESSION_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(target) = launcher.target_dir() {
        // The queue owns its build directory: no per-session environment.
        std::fs::create_dir_all(&target).map_err(|error| {
            LanderError::Unavailable(format!("cannot create the queue target: {error}"))
        })?;
        command.env("CARGO_TARGET_DIR", target);
    }
    // Its own process group, so a timed-out run is stopped with everything it
    // started that stayed in the group. The group is signalled only while the
    // lander is unreaped, so a saved id can never hit a reused group (#1251).
    command.process_group(0);
    let child = command
        .spawn()
        .map_err(|error| LanderError::Unavailable(format!("cannot start the lander: {error}")))?;
    crate::process_control::capture_owned_group(
        child,
        deadline,
        LANDER_SETTLE_LIMITS,
        "rolling queue gate wall-time budget ran out; stopping the lander group",
    )
    .await
    .map_err(|error| match error {
        crate::process_control::OwnedGroupError::TimedOut => LanderError::TimedOut,
        crate::process_control::OwnedGroupError::Io(error) => {
            LanderError::Unavailable(format!("lander wait failed: {error}"))
        }
    })
}

/// How long a finished lander's output may stay open (a descendant that left
/// its group still holding the pipes), and how long a stopped lander gets to
/// exit, before the queue moves on (#1251).
const LANDER_SETTLE_LIMITS: crate::process_control::OwnedGroupLimits =
    crate::process_control::OwnedGroupLimits {
        post_exit_drain: Duration::from_secs(10),
        cleanup: Duration::from_secs(5),
    };

async fn with_store<T: Send + 'static>(
    store: &Arc<tokio::sync::Mutex<Store>>,
    op: impl FnOnce(&Store) -> Result<T> + Send + 'static,
) -> Result<T> {
    let store = Arc::clone(store);
    tokio::task::spawn_blocking(move || op(&store.blocking_lock()))
        .await
        .map_err(|error| DaemonError::Process(format!("rolling queue store join: {error}")))?
}

/// One lander run over `entries`, classified. `stdout` is kept for the
/// evidence lines (`fetched_target_id`, `migration_N_*`).
struct GroupRun {
    result: RunResult,
    stdout: String,
    /// Out-of-band re-gates the lander spent in this run (from its
    /// `stale_retry_N=..:regated` evidence lines).
    regates_used: usize,
    /// The exact commit this run pushed (the lander's `published_target_id`,
    /// or its forward revert): the only tip the queue may call its own. The
    /// observed remote head in `result.outcome.landed_sha` can be a later
    /// external descendant. `None` when the run reported no published commit.
    own_tip: Option<String>,
    /// The member an integration conflict is attributable to, when the lander
    /// named it.
    conflict_source: Option<String>,
    /// The per-run log of the lander's stdout and stderr (#1135).
    log_path: Option<PathBuf>,
    /// Short cause of a non-green run: refusal code, conflict paths or the
    /// first failing tests.
    cause: Option<String>,
    /// The lander refused a source as already integrated into the tip (#1208).
    already_integrated: bool,
}

impl GroupRun {
    /// The event detail fields every gate event carries: the cause and the log.
    fn evidence(&self) -> serde_json::Value {
        serde_json::json!({
            "cause": self.cause,
            "log": self.log_path.as_ref().map(|path| path.display().to_string()),
        })
    }
}

fn own_published_tip(stdout: &str) -> Option<String> {
    let reverted = line_value(stdout, "forward_revert_status") == Some("published");
    let revert = line_value(stdout, "forward_revert_id").filter(|_| reverted);
    revert
        .or_else(|| line_value(stdout, "published_target_id"))
        .map(|tip| tip.trim().to_string())
        .filter(|tip| !tip.is_empty())
}

fn regates_used(stdout: &str) -> usize {
    stdout
        .lines()
        .filter(|line| line.starts_with("stale_retry_") && line.ends_with(":regated"))
        .count()
}

async fn run_group(
    launcher: &LanderLauncher,
    batch_id: Uuid,
    repo: &Path,
    entries: &[RollingQueueEntryV1],
    regates_left: usize,
    expected_tip: Option<&str>,
    deadline: tokio::time::Instant,
) -> GroupRun {
    match run_lander(
        launcher,
        repo,
        entries,
        regates_left,
        expected_tip,
        deadline,
    )
    .await
    {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            let already_integrated =
                output.status.code() != Some(0) && stderr.contains(ALREADY_INTEGRATED_MARKER);
            // Only an uncertain publication, or a lone source the lander calls
            // already integrated, needs the ancestry probe.
            let uncertain = output.status.code() != Some(0)
                && (matches!(
                    line_value(&stdout, "publication_status"),
                    Some("published" | "unknown")
                ) || (already_integrated && entries.len() == 1));
            let landed = if uncertain {
                let source = entries[0].source_commit.clone();
                let probe_repo = repo.to_path_buf();
                tokio::task::spawn_blocking(move || landed_tip(&probe_repo, &source))
                    .await
                    .ok()
                    .flatten()
            } else {
                None
            };
            let mut result = classify_run(output.status.code(), &stdout, &stderr, || landed);
            let log_path = launcher.log_dir().and_then(|dir| {
                let header = format!(
                    "lander exit={:?} sources={}",
                    output.status.code(),
                    entries
                        .iter()
                        .map(|entry| entry.source_commit.as_str())
                        .collect::<Vec<_>>()
                        .join(",")
                );
                write_run_log(&dir, batch_id, &header, &stdout, &stderr)
            });
            let cause = run_cause(&result, &stderr);
            if let Some(path) = &log_path {
                let line = format!("lander_log={}", path.display());
                result.outcome.detail = Some(match result.outcome.detail.take() {
                    Some(detail) => format!("{detail}\n{line}"),
                    None => line,
                });
            }
            let mut regates_used = regates_used(&stdout);
            if result.outcome.refusal.as_deref() == Some(QUEUE_REGATE_EXHAUSTED) {
                regates_used = regates_used.max(regates_left);
            }
            let own_tip = own_published_tip(&stdout);
            let conflict_source = conflict_source(&stderr);
            GroupRun {
                result,
                stdout,
                regates_used,
                own_tip,
                conflict_source,
                log_path,
                cause,
                already_integrated,
            }
        }
        Err(LanderError::TimedOut) => GroupRun {
            result: gate_timeout_result(launcher),
            stdout: String::new(),
            regates_used: 0,
            own_tip: None,
            conflict_source: None,
            log_path: None,
            cause: Some(QUEUE_GATE_TIMEOUT.into()),
            already_integrated: false,
        },
        Err(LanderError::Unavailable(message)) => GroupRun {
            result: RunResult {
                state: RollingQueueEntryState::Failed,
                outcome: RollingQueueOutcome {
                    refusal: Some("lander_unavailable".into()),
                    detail: Some(message),
                    ..RollingQueueOutcome::default()
                },
            },
            stdout: String::new(),
            regates_used: 0,
            own_tip: None,
            conflict_source: None,
            log_path: None,
            cause: Some("lander_unavailable".into()),
            already_integrated: false,
        },
    }
}

/// The typed refusal of a batch that ran past its gate wall-time budget.
fn gate_timeout_result(launcher: &LanderLauncher) -> RunResult {
    RunResult {
        state: RollingQueueEntryState::Refused,
        outcome: RollingQueueOutcome {
            refusal: Some(QUEUE_GATE_TIMEOUT.to_string()),
            detail: Some(format!(
                "the batch gate ran past its {}-minute wall-time budget \
                 (operator setting rolling_queue_gate_timeout_mins); the lander was \
                 stopped and nothing was published for this source: re-enqueue it",
                launcher.gate_timeout.as_secs() / 60
            )),
            ..RollingQueueOutcome::default()
        },
    }
}

fn is_gate_timeout(result: &RunResult) -> bool {
    result.outcome.refusal.as_deref() == Some(QUEUE_GATE_TIMEOUT)
}

/// Settle every member that already reached `origin/rolling` outside the
/// queue as `published` with the tip that contains it, so a landed commit is
/// never gated, re-gated or refused (#1208). Returns the members still to
/// gate, in queue order, and the tip they were checked against when any member
/// had landed.
async fn settle_already_landed(
    store: &Arc<tokio::sync::Mutex<Store>>,
    repo: &Path,
    batch_id: Uuid,
    members: Vec<RollingQueueEntryV1>,
    settled: &mut Vec<(Uuid, RunResult)>,
) -> Result<(Vec<RollingQueueEntryV1>, Option<String>)> {
    if members.is_empty() {
        return Ok((members, None));
    }
    let probe_repo = repo.to_path_buf();
    let sources: Vec<String> = members.iter().map(|m| m.source_commit.clone()).collect();
    let tips = tokio::task::spawn_blocking(move || landed_tips(&probe_repo, &sources))
        .await
        .map_err(|error| DaemonError::Process(format!("landed probe: {error}")))?;
    let mut kept = Vec::new();
    let mut landed = Vec::new();
    for (entry, tip) in members.into_iter().zip(tips) {
        match tip {
            Some(tip) => landed.push((entry, tip)),
            None => kept.push(entry),
        }
    }
    let observed = landed.first().map(|(_, tip)| tip.clone());
    let mut group = Vec::with_capacity(landed.len());
    for (entry, tip) in landed {
        let id = entry.id;
        bisect_event(
            store,
            batch_id,
            "already_landed",
            Some(id),
            serde_json::json!({ "source": entry.source_commit, "tip": tip }),
        )
        .await?;
        if entry.migration_version.is_some() {
            // The number it actually landed with, when a provisional commit
            // renumbered it; otherwise it kept its own.
            let (probe_repo, source) = (repo.to_path_buf(), entry.source_commit.clone());
            let published = tokio::task::spawn_blocking(move || {
                published_migration_number(&probe_repo, &source)
            })
            .await
            .map_err(|error| DaemonError::Process(format!("landed migration probe: {error}")))?;
            if let Some(version) = published {
                with_store(store, move |s| {
                    s.record_rolling_queue_assigned_migration(id, version)
                })
                .await?;
            }
        }
        group.push((
            id,
            RunResult {
                state: RollingQueueEntryState::Published,
                outcome: RollingQueueOutcome {
                    landed_sha: Some(tip),
                    detail: Some(format!(
                        "{} was already on origin/rolling (landed outside the queue); \
                         settled without a gate",
                        entry.source_commit
                    )),
                    ..RollingQueueOutcome::default()
                },
            },
        ));
    }
    settle_group(store, &group).await?;
    settled.extend(group);
    Ok((kept, observed))
}

/// Settle `members` after the batch's gate wall-time budget ran out: a member
/// that landed meanwhile is published, every other one is refused with
/// `queue_gate_timeout` (#1208).
async fn settle_timed_out(
    store: &Arc<tokio::sync::Mutex<Store>>,
    repo: &Path,
    batch_id: Uuid,
    members: Vec<RollingQueueEntryV1>,
    result: &RunResult,
    settled: &mut Vec<(Uuid, RunResult)>,
) -> Result<()> {
    let (rest, _) = settle_already_landed(store, repo, batch_id, members, settled).await?;
    bisect_event(
        store,
        batch_id,
        "gate_timeout",
        None,
        serde_json::json!({ "refused": rest.len(), "detail": result.outcome.detail }),
    )
    .await?;
    let group: Vec<(Uuid, RunResult)> = rest
        .iter()
        .map(|entry| (entry.id, result.clone()))
        .collect();
    settle_group(store, &group).await?;
    settled.extend(group);
    Ok(())
}

/// Remove and return the member a conflicting multi-member run names, so only
/// that entry is refused and the rest go on. `None` when the run is not a
/// conflict, the lander named no member of `members`, or the group is a single
/// entry (then the ordinary settlement refuses it).
fn take_conflict_culprit(
    members: &mut Vec<RollingQueueEntryV1>,
    run: &GroupRun,
) -> Option<RollingQueueEntryV1> {
    if members.len() < 2
        || run.result.outcome.refusal.as_deref() != Some(QUEUE_BATCH_MERGE_CONFLICT)
    {
        return None;
    }
    let source = run.conflict_source.as_deref()?;
    let index = members
        .iter()
        .position(|entry| entry.source_commit == source)?;
    Some(members.remove(index))
}

/// Batch members share one candidate, so a lander policy refusal is reported
/// as the batch code with the fence kept in the detail.
fn as_batch_result(mut result: RunResult) -> RunResult {
    if let Some(fence) = result
        .outcome
        .refusal
        .as_deref()
        .and_then(|code| code.strip_prefix("policy_refused:"))
    {
        let detail = result.outcome.detail.take().unwrap_or_default();
        result.outcome.detail = Some(tail(&format!("policy_fence={fence}\n{detail}")));
        result.outcome.refusal = Some(QUEUE_BATCH_POLICY_REFUSED.to_string());
    }
    result
}

/// Whether a non-green gate is a red gate: only that is attributable to one
/// member by bisecting. A policy refusal is a property of the batch as a whole
/// and settles every member with its typed refusal; a merge conflict is
/// attributed by the lander (`take_conflict_culprit`), not by bisecting.
fn is_bisectable(result: &RunResult) -> bool {
    result.state == RollingQueueEntryState::Refused
        && matches!(
            result.outcome.refusal.as_deref(),
            Some("gate_failed" | QUEUE_FILTER_MATCHES_NO_TESTS)
        )
}

async fn settle(
    store: &Arc<tokio::sync::Mutex<Store>>,
    id: Uuid,
    result: &RunResult,
) -> Result<()> {
    let (state, outcome) = (result.state, result.outcome.clone());
    with_store(store, move |s| {
        s.settle_rolling_queue_entry(id, state, &outcome, Utc::now())
    })
    .await?;
    Ok(())
}

/// Settle `members` with their results in one transaction: each entry keeps
/// its own outcome row, and each resolved owner gets ONE wake naming every
/// entry settled here (#1146). Separate calls still wake separately.
async fn settle_group(
    store: &Arc<tokio::sync::Mutex<Store>>,
    members: &[(Uuid, RunResult)],
) -> Result<()> {
    let items: Vec<(Uuid, RollingQueueEntryState, RollingQueueOutcome)> = members
        .iter()
        .map(|(id, result)| (*id, result.state, result.outcome.clone()))
        .collect();
    with_store(store, move |s| {
        s.settle_rolling_queue_entries(&items, Utc::now())
    })
    .await?;
    Ok(())
}

/// Ancestry of each source in the published tip: after one fetch, a source is
/// verified only when it is an ancestor of `origin/rolling`. Migration
/// evidence in the lander's report never substitutes for the probe: the
/// provisional commit the lander writes keeps the original source as a parent.
fn verify_ancestry(repo: &Path, sources: &[String]) -> Vec<bool> {
    let _ = fetch_rolling(repo);
    sources
        .iter()
        .map(|source| {
            git(
                repo,
                &["merge-base", "--is-ancestor", source, "origin/rolling"],
            )
            .is_some()
        })
        .collect()
}

/// `(source, final_version)` per provisional migration the lander renumbered.
fn migration_evidence(stdout: &str) -> Vec<(String, u32)> {
    let mut found = Vec::new();
    for index in 0.. {
        let (Some(source), Some(version)) = (
            line_value(stdout, &format!("migration_{index}_source_id")),
            line_value(stdout, &format!("migration_{index}_final_version")),
        ) else {
            break;
        };
        let Ok(version) = version.trim().parse() else {
            break;
        };
        found.push((source.to_string(), version));
    }
    found
}

/// Refuse the members whose migration number is out of order, returning the
/// rest (still `gating`) in queue order. Best effort: without a readable tip
/// the lander's own tip + 1 rule decides.
async fn order_migrations(
    store: &Arc<tokio::sync::Mutex<Store>>,
    repo: &Path,
    members: Vec<RollingQueueEntryV1>,
    settled: &mut Vec<(Uuid, RunResult)>,
) -> Result<Vec<RollingQueueEntryV1>> {
    if members
        .iter()
        .all(|entry| entry.migration_version.is_none())
    {
        return Ok(members);
    }
    let probe_repo = repo.to_path_buf();
    let probe_members = members.clone();
    let plan = tokio::task::spawn_blocking(move || {
        let _ = fetch_rolling(&probe_repo);
        let tip = schema_version_at(&probe_repo, "origin/rolling")?;
        let facts: Vec<MigrationMember> = probe_members
            .iter()
            .map(|entry| MigrationMember {
                id: entry.id,
                derived: entry.migration_version,
                declared: entry.migration_version.is_some()
                    && source_declares_provisional(&probe_repo, &entry.source_commit),
            })
            .collect();
        Some(plan_migration_order(tip, &facts))
    })
    .await
    .map_err(|error| DaemonError::Process(format!("migration order probe: {error}")))?;
    let Some(plan) = plan else {
        return Ok(members);
    };
    for (id, version) in plan.assigned {
        with_store(store, move |s| {
            s.record_rolling_queue_assigned_migration(id, version)
        })
        .await?;
    }
    let mut kept = Vec::new();
    let mut refused = Vec::new();
    for entry in members {
        if plan.out_of_order.contains(&entry.id) {
            let result = RunResult {
                state: RollingQueueEntryState::Refused,
                outcome: RollingQueueOutcome {
                    refusal: Some(QUEUE_MIGRATION_OUT_OF_ORDER.to_string()),
                    detail: Some(format!(
                        "migration {} is already taken by an earlier queued source; \
                         re-enqueue after renumbering (declare a provisional migration)",
                        entry.migration_version.unwrap_or_default()
                    )),
                    ..RollingQueueOutcome::default()
                },
            };
            refused.push((entry.id, result));
        } else {
            kept.push(entry);
        }
    }
    settle_group(store, &refused).await?;
    settled.extend(refused);
    Ok(kept)
}

/// Claim the FIFO head, run it and settle it. `Ok(None)` when nothing was
/// runnable (empty queue or another entry is gating).
pub(crate) async fn run_next(
    store: &Arc<tokio::sync::Mutex<Store>>,
    launcher: &LanderLauncher,
) -> Result<Option<(Uuid, RunResult)>> {
    Ok(run_next_batch(store, launcher, 1).await?.into_iter().next())
}

/// Claim up to `batch_size` ready sources in FIFO order, gate them once as one
/// candidate, publish with one fast-forward and settle every member exactly
/// once. Empty when nothing was runnable.
pub(crate) async fn run_next_batch(
    store: &Arc<tokio::sync::Mutex<Store>>,
    launcher: &LanderLauncher,
    batch_size: usize,
) -> Result<Vec<(Uuid, RunResult)>> {
    let size = batch_size.clamp(1, ROLLING_QUEUE_MAX_BATCH_SIZE as usize);
    let Some(claimed) = with_store(store, move |s| {
        s.claim_rolling_queue_batch(Utc::now(), size)
    })
    .await?
    else {
        return Ok(Vec::new());
    };
    let batch_id = claimed.batch_id;
    let deadline = tokio::time::Instant::now() + launcher.gate_timeout;
    let mut settled = Vec::new();
    let mut members = claimed.entries;
    let repo = match queue_repo(launcher, &claimed.repo_path).await {
        Ok(repo) => repo,
        Err(message) => {
            let result = RunResult {
                state: RollingQueueEntryState::Failed,
                outcome: RollingQueueOutcome {
                    refusal: Some(workspace_refusal_code(&message).into()),
                    detail: Some(format!("queue workspace unavailable: {message}")),
                    ..RollingQueueOutcome::default()
                },
            };
            let group: Vec<(Uuid, RunResult)> = members
                .iter()
                .map(|entry| (entry.id, result.clone()))
                .collect();
            settle_group(store, &group).await?;
            settled.extend(group);
            return Ok(settled);
        }
    };
    // A member already on rolling (landed outside the queue) is published
    // as is, never gated (#1208).
    (members, _) = settle_already_landed(store, &repo, batch_id, members, &mut settled).await?;
    if members.len() > 1 {
        members = order_migrations(store, &repo, members, &mut settled).await?;
    }
    if members.is_empty() {
        return Ok(settled);
    }
    let mut run = run_group(
        launcher,
        batch_id,
        &repo,
        &members,
        MAX_REGATES,
        None,
        deadline,
    )
    .await;
    loop {
        if let Some(culprit) = take_conflict_culprit(&mut members, &run) {
            // An integration conflict belongs to one member: refuse that entry
            // with its conflict path and gate the rest as if it had never been
            // in the batch.
            bisect_event(
                store,
                batch_id,
                "conflict_isolated",
                Some(culprit.id),
                run.evidence(),
            )
            .await?;
            settle(store, culprit.id, &run.result).await?;
            settled.push((culprit.id, run.result.clone()));
        } else if run.already_integrated {
            // The lander found a member already on rolling: publish every
            // landed member and gate only the rest.
            let before = members.len();
            (members, _) =
                settle_already_landed(store, &repo, batch_id, members, &mut settled).await?;
            if members.is_empty() {
                return Ok(settled);
            }
            if members.len() == before {
                break;
            }
        } else {
            break;
        }
        run = run_group(
            launcher,
            batch_id,
            &repo,
            &members,
            MAX_REGATES,
            None,
            deadline,
        )
        .await;
    }
    if is_gate_timeout(&run.result) {
        settle_timed_out(store, &repo, batch_id, members, &run.result, &mut settled).await?;
        return Ok(settled);
    }
    let in_batch = members.len() > 1;
    let regates_left = MAX_REGATES.saturating_sub(run.regates_used);
    let result = if in_batch {
        as_batch_result(run.result.clone())
    } else {
        run.result.clone()
    };
    if result.state == RollingQueueEntryState::Published {
        publish_group(
            store,
            &repo,
            batch_id,
            &members,
            &run,
            &result,
            &mut settled,
        )
        .await?;
        return Ok(settled);
    }
    if in_batch && is_bisectable(&result) {
        let reason = result.outcome.refusal.clone().unwrap_or_default();
        let mut detail = run.evidence();
        detail["reason"] = serde_json::json!(reason);
        with_store(store, move |s| {
            s.begin_rolling_queue_bisect_with(batch_id, detail, Utc::now())
        })
        .await?;
        let first_base = match line_value(&run.stdout, "fetched_target_id") {
            Some(tip) => Some(tip.to_string()),
            None => probe_tip(&repo).await,
        };
        bisect_batch(
            store,
            launcher,
            &repo,
            batch_id,
            members,
            result,
            first_base,
            regates_left,
            deadline,
            &mut settled,
        )
        .await?;
        return Ok(settled);
    }
    let group: Vec<(Uuid, RunResult)> = members
        .iter()
        .map(|entry| (entry.id, result.clone()))
        .collect();
    settle_group(store, &group).await?;
    settled.extend(group);
    Ok(settled)
}

/// Settle a group whose lander run published: verify every source against the
/// published tip (a lander report is never trusted alone), record the final
/// migration numbers the lander assigned, and settle every member once with its
/// landed SHA or a typed `queue_batch_ancestry_unverified` failure.
async fn publish_group(
    store: &Arc<tokio::sync::Mutex<Store>>,
    repo: &Path,
    batch_id: Uuid,
    group: &[RollingQueueEntryV1],
    run: &GroupRun,
    result: &RunResult,
    settled: &mut Vec<(Uuid, RunResult)>,
) -> Result<()> {
    let tip = result.outcome.landed_sha.clone();
    let sources: Vec<String> = group.iter().map(|m| m.source_commit.clone()).collect();
    let probe_repo = repo.to_path_buf();
    let probe_sources = sources.clone();
    let verified =
        tokio::task::spawn_blocking(move || verify_ancestry(&probe_repo, &probe_sources))
            .await
            .map_err(|error| DaemonError::Process(format!("ancestry probe: {error}")))?;
    let base = line_value(&run.stdout, "fetched_target_id").map(str::to_string);
    let candidate = line_value(&run.stdout, "candidate_id").map(str::to_string);
    let summary = serde_json::json!({
        "sources": sources,
        "published": tip,
        "verified": verified,
    });
    with_store(store, move |s| {
        s.record_rolling_queue_batch_gate(batch_id, base.as_deref(), candidate.as_deref(), &summary)
    })
    .await?;
    if let Some(published) = tip.clone() {
        // The queue's own push: the next advance beyond it is external.
        bisect_event(
            store,
            batch_id,
            "expected_tip",
            None,
            serde_json::json!({
                "tip": published,
                "candidate": line_value(&run.stdout, "candidate_id"),
                "sources": group.iter().map(|m| m.source_commit.clone()).collect::<Vec<_>>(),
                "migrations": migration_evidence(&run.stdout)
                    .iter()
                    .map(|(source, version)| serde_json::json!({"source": source, "version": version}))
                    .collect::<Vec<_>>(),
            }),
        )
        .await?;
    }
    let versions = migration_evidence(&run.stdout);
    let mut member_results = Vec::with_capacity(group.len());
    for (entry, ok) in group.iter().zip(verified) {
        let member_result = if ok {
            if let Some((_, version)) = versions
                .iter()
                .find(|(source, _)| *source == entry.source_commit)
            {
                let (id, version) = (entry.id, *version);
                // The actual number is durable before it is applied, so a
                // restart never replaces it with the pre-run plan.
                bisect_event(
                    store,
                    batch_id,
                    "migration_receipt",
                    Some(id),
                    serde_json::json!({ "version": version, "source": entry.source_commit }),
                )
                .await?;
                with_store(store, move |s| {
                    s.record_rolling_queue_assigned_migration(id, version)
                })
                .await?;
            }
            result.clone()
        } else {
            RunResult {
                state: RollingQueueEntryState::Failed,
                outcome: RollingQueueOutcome {
                    refusal: Some(QUEUE_BATCH_ANCESTRY_UNVERIFIED.to_string()),
                    detail: Some(format!(
                        "{} is not contained in the published tip {}",
                        entry.source_commit,
                        tip.clone().unwrap_or_default()
                    )),
                    ..RollingQueueOutcome::default()
                },
            }
        };
        member_results.push((entry.id, member_result));
    }
    settle_group(store, &member_results).await?;
    settled.extend(member_results);
    Ok(())
}

/// `origin/rolling` after one fetch (`None` when git cannot say).
fn fetch_tip(repo: &Path) -> Option<String> {
    let _ = fetch_rolling(repo);
    git(repo, &["rev-parse", "--verify", "origin/rolling^{commit}"])
        .map(|tip| tip.trim().to_string())
        .filter(|tip| !tip.is_empty())
}

async fn probe_tip(repo: &Path) -> Option<String> {
    let repo = repo.to_path_buf();
    tokio::task::spawn_blocking(move || fetch_tip(&repo))
        .await
        .ok()
        .flatten()
}

/// Durably record, before `group` is run, the migration numbers the lander
/// will give its members (tip + 1, ... in queue order). A crash after the
/// lander pushed but before the report was recorded is then repaired by the
/// restart reconcile from this evidence.
async fn record_migration_plan(
    store: &Arc<tokio::sync::Mutex<Store>>,
    repo: &Path,
    batch_id: Uuid,
    group: &[RollingQueueEntryV1],
) -> Result<()> {
    if group.iter().all(|entry| entry.migration_version.is_none()) {
        return Ok(());
    }
    let probe_repo = repo.to_path_buf();
    let members = group.to_vec();
    let planned = tokio::task::spawn_blocking(move || {
        let _ = fetch_rolling(&probe_repo);
        let tip = schema_version_at(&probe_repo, "origin/rolling")?;
        let facts: Vec<MigrationMember> = members
            .iter()
            .map(|entry| MigrationMember {
                id: entry.id,
                derived: entry.migration_version,
                declared: entry.migration_version.is_some()
                    && source_declares_provisional(&probe_repo, &entry.source_commit),
            })
            .collect();
        let plan = plan_migration_order(tip, &facts);
        Some(
            plan.assigned
                .iter()
                .filter_map(|(id, version)| {
                    let member = members.iter().find(|entry| entry.id == *id)?;
                    Some(serde_json::json!({
                        "id": id.to_string(),
                        "source": member.source_commit,
                        "version": version,
                    }))
                })
                .collect::<Vec<_>>(),
        )
    })
    .await
    .map_err(|error| DaemonError::Process(format!("migration plan probe: {error}")))?;
    let Some(assigned) = planned.filter(|list| !list.is_empty()) else {
        return Ok(());
    };
    bisect_event(
        store,
        batch_id,
        "migration_plan",
        None,
        serde_json::json!({ "assigned": assigned }),
    )
    .await
}

async fn bisect_event(
    store: &Arc<tokio::sync::Mutex<Store>>,
    batch_id: Uuid,
    kind: &'static str,
    entry: Option<Uuid>,
    detail: serde_json::Value,
) -> Result<()> {
    with_store(store, move |s| {
        s.record_rolling_queue_batch_event(batch_id, kind, entry, Some(detail), Utc::now())
    })
    .await
}

/// The tip a lander run gated against (its `fetched_target_id`), else the tip
/// probed just before it ran.
fn run_base(stdout: &str, probed: Option<&String>) -> Option<String> {
    line_value(stdout, "fetched_target_id")
        .map(str::to_string)
        .or_else(|| probed.cloned())
}

/// A red batch: attribute the failure to exactly one source per red, and
/// publish every innocent one.
///
/// `pending[..window]` is a prefix known to be red as a candidate on
/// `red_base` (`None`: unknown). Each step gates the first half of the
/// window against the tip it would publish onto: green publishes it (one
/// fast-forward) and the rest of the window stays red; red narrows the window
/// to that half. A window of one is the isolated bad source and settles with
/// the failing tests of the gate that condemned it. The tail after an
/// isolated source is gated whole against the new tip.
///
/// Red evidence is only as fresh as the base it was observed on: when the tip
/// the remaining window would publish onto is not that base (an external push
/// landed, or a prefix was gated on a different tip), the old red is stale and
/// the window is gated again instead of refused. `expected_tip` follows the
/// queue's own pushes; any other advance is external and spends the batch's one
/// out-of-band re-gate (`regates_left`, carried across every run). A second
/// external advance settles the rest with `queue_out_of_band_regate_exhausted`.
/// Every step shrinks the window or the pending list;
/// `queue_bisect_no_progress` settles the rest if that ever fails to hold.
async fn bisect_batch(
    store: &Arc<tokio::sync::Mutex<Store>>,
    launcher: &LanderLauncher,
    repo: &Path,
    batch_id: Uuid,
    members: Vec<RollingQueueEntryV1>,
    first_red: RunResult,
    first_base: Option<String>,
    mut regates_left: usize,
    deadline: tokio::time::Instant,
    settled: &mut Vec<(Uuid, RunResult)>,
) -> Result<()> {
    let mut pending = members;
    let mut window: Option<usize> = Some(pending.len());
    let mut evidence = first_red;
    let mut red_base = first_base.clone();
    let mut expected_tip = first_base;
    let mut budget = pending.len() * 4 + 4;
    while !pending.is_empty() {
        let no_progress = budget == 0 || window.is_some_and(|w| w == 0 || w > pending.len());
        if no_progress {
            let result = RunResult {
                state: RollingQueueEntryState::Refused,
                outcome: RollingQueueOutcome {
                    refusal: Some(QUEUE_BISECT_NO_PROGRESS.to_string()),
                    detail: Some(format!(
                        "bisect made no progress with {} source(s) pending",
                        pending.len()
                    )),
                    ..RollingQueueOutcome::default()
                },
            };
            bisect_event(
                store,
                batch_id,
                "bisect_no_progress",
                None,
                serde_json::json!({ "pending": pending.len() }),
            )
            .await?;
            let rest: Vec<(Uuid, RunResult)> = pending
                .drain(..)
                .map(|entry| (entry.id, result.clone()))
                .collect();
            settle_group(store, &rest).await?;
            settled.extend(rest);
            break;
        }
        budget -= 1;
        // A member that reached rolling outside the queue since the last step
        // is published as is (#1208). The advance that landed it is accounted
        // for, not charged as an out-of-band re-gate, and any red evidence that
        // was observed with it in the window is dropped.
        let before = pending.len();
        let observed;
        (pending, observed) =
            settle_already_landed(store, repo, batch_id, pending, settled).await?;
        if pending.len() != before {
            window = None;
            red_base = None;
            if observed.is_some() {
                expected_tip = observed;
            }
            if pending.is_empty() {
                break;
            }
        }
        let current = probe_tip(repo).await;
        if let Some(current) = current.as_ref() {
            if expected_tip.as_ref().is_some_and(|tip| tip != current) {
                // Not the queue's own push: an external advance.
                if regates_left == 0 {
                    let result = RunResult {
                        state: RollingQueueEntryState::Refused,
                        outcome: RollingQueueOutcome {
                            refusal: Some(QUEUE_REGATE_EXHAUSTED.to_string()),
                            detail: Some(format!(
                                "rolling advanced again to {current} outside the queue after the \
                                 batch's one re-gate"
                            )),
                            ..RollingQueueOutcome::default()
                        },
                    };
                    bisect_event(
                        store,
                        batch_id,
                        "regate_exhausted",
                        None,
                        serde_json::json!({ "expected": expected_tip, "observed": current }),
                    )
                    .await?;
                    let rest: Vec<(Uuid, RunResult)> = pending
                        .drain(..)
                        .map(|entry| (entry.id, result.clone()))
                        .collect();
                    settle_group(store, &rest).await?;
                    settled.extend(rest);
                    break;
                }
                regates_left -= 1;
                bisect_event(
                    store,
                    batch_id,
                    "regate_spent",
                    None,
                    serde_json::json!({ "expected": expected_tip, "observed": current }),
                )
                .await?;
            }
            expected_tip = Some(current.clone());
            if window.is_some() && red_base.as_ref() != Some(current) {
                bisect_event(
                    store,
                    batch_id,
                    "bisect_red_stale",
                    None,
                    serde_json::json!({ "red_base": red_base, "tip": current }),
                )
                .await?;
                window = None;
                red_base = None;
            }
        }
        if window == Some(1) {
            let entry = pending.remove(0);
            bisect_event(
                store,
                batch_id,
                "bisect_isolated",
                Some(entry.id),
                serde_json::json!({
                    "refusal": evidence.outcome.refusal,
                    "failing_tests": evidence.outcome.failing_tests,
                }),
            )
            .await?;
            settle(store, entry.id, &evidence).await?;
            settled.push((entry.id, evidence.clone()));
            window = None;
            red_base = None;
            continue;
        }
        let take = window.map_or(pending.len(), |w| w.div_ceil(2));
        let group: Vec<RollingQueueEntryV1> = pending[..take].to_vec();
        record_migration_plan(store, repo, batch_id, &group).await?;
        let run = run_group(
            launcher,
            batch_id,
            repo,
            &group,
            regates_left,
            expected_tip.as_deref(),
            deadline,
        )
        .await;
        if is_gate_timeout(&run.result) {
            settle_timed_out(store, repo, batch_id, pending, &run.result, settled).await?;
            break;
        }
        if run.already_integrated {
            // Publish the landed member(s) and gate the rest again: this run's
            // outcome says nothing about them.
            let before = pending.len();
            let observed;
            (pending, observed) =
                settle_already_landed(store, repo, batch_id, pending, settled).await?;
            if pending.len() != before {
                window = None;
                red_base = None;
                expected_tip = observed.or(expected_tip);
                continue;
            }
        }
        regates_left = regates_left.saturating_sub(run.regates_used);
        let result = as_batch_result(run.result.clone());
        let base = run_base(&run.stdout, expected_tip.as_ref());
        let outcome = if result.state == RollingQueueEntryState::Published {
            "green"
        } else if is_bisectable(&result) {
            "red"
        } else {
            "unattributable"
        };
        bisect_event(
            store,
            batch_id,
            "bisect_gate",
            None,
            serde_json::json!({
                "sources": group.iter().map(|e| e.source_commit.clone()).collect::<Vec<_>>(),
                "window": window,
                "outcome": outcome,
                "base": base,
                "cause": run.cause,
                "log": run.log_path.as_ref().map(|path| path.display().to_string()),
            }),
        )
        .await?;
        match outcome {
            "green" => {
                publish_group(store, repo, batch_id, &group, &run, &result, settled).await?;
                pending.drain(..take);
                // The red evidence carries over only when the prefix was gated
                // on the very base the red was observed on.
                // The queue's own push is the exact candidate the lander
                // reported; the observed remote head can be a later external
                // descendant that may have fixed (or changed) the red.
                let landed = result.outcome.landed_sha.clone();
                let exact = match (run.own_tip.as_ref(), landed.as_ref()) {
                    (Some(own), Some(landed)) => own == landed,
                    _ => false,
                };
                let carried = exact && window.is_some() && red_base.is_some() && red_base == base;
                if !exact {
                    bisect_event(
                        store,
                        batch_id,
                        "bisect_external_descendant",
                        None,
                        serde_json::json!({ "own": run.own_tip, "observed": landed }),
                    )
                    .await?;
                }
                window = if carried {
                    window.map(|w| w - take)
                } else {
                    None
                };
                red_base = if carried {
                    result.outcome.landed_sha.clone()
                } else {
                    None
                };
                // Anything past the exact candidate is external: expecting the
                // candidate makes the next probe charge the re-gate. Without a
                // reported candidate the head is adopted (nothing to charge).
                expected_tip = run.own_tip.clone().or(landed).or(expected_tip);
            }
            "red" => {
                evidence = result;
                window = Some(take);
                red_base = base.clone();
                expected_tip = base.or(expected_tip);
            }
            _ => {
                let mut rest = group.clone();
                if let Some(culprit) = take_conflict_culprit(&mut rest, &run) {
                    // Only the conflicting member is refused; the group is
                    // gated again without it.
                    settle(store, culprit.id, &result).await?;
                    settled.push((culprit.id, result.clone()));
                    pending.retain(|entry| entry.id != culprit.id);
                    window = None;
                    red_base = None;
                    expected_tip = base.or(expected_tip);
                    continue;
                }
                let refused: Vec<(Uuid, RunResult)> = group
                    .iter()
                    .map(|entry| (entry.id, result.clone()))
                    .collect();
                settle_group(store, &refused).await?;
                settled.extend(refused);
                pending.drain(..take);
                if window.is_some() {
                    // The remaining window's red was observed with this group
                    // in it: do not trust it once the group is gone.
                    window = None;
                    red_base = None;
                }
                expected_tip = base.or(expected_tip);
            }
        }
    }
    Ok(())
}

/// One-shot restart reconcile: settle or re-queue every `gating` entry.
/// Returns `(settled, requeued)`.
pub(crate) async fn reconcile_gating(
    store: &Arc<tokio::sync::Mutex<Store>>,
) -> Result<(usize, usize)> {
    let gating = with_store(store, |s| {
        s.list_rolling_queue_entries(Some(RollingQueueEntryState::Gating), 64)
    })
    .await?;
    let (mut settled, mut requeued) = (0, 0);
    for (entry, repo_path) in gating {
        let source = entry.source_commit.clone();
        let repo = PathBuf::from(repo_path);
        let probe_repo = repo.clone();
        let tip = tokio::task::spawn_blocking(move || landed_tip(&repo, &source))
            .await
            .map_err(|error| DaemonError::Process(format!("reconcile probe: {error}")))?;
        let id = entry.id;
        match tip {
            Some(tip) => {
                // The run published before its report was settled. The actual
                // number wins, in order: the lander receipt the queue saved,
                // the number in the published provisional commit, and only
                // then the number the queue planned before the run.
                let receipt =
                    with_store(store, move |s| s.rolling_queue_migration_receipt(id)).await?;
                let recovered = match receipt {
                    Some(version) => Some(version),
                    None => {
                        let (repo, source) = (probe_repo.clone(), entry.source_commit.clone());
                        tokio::task::spawn_blocking(move || {
                            published_migration_number(&repo, &source)
                        })
                        .await
                        .map_err(|error| {
                            DaemonError::Process(format!("reconcile migration probe: {error}"))
                        })?
                    }
                };
                let planned = match recovered {
                    Some(version) => Some(version),
                    None => {
                        with_store(store, move |s| s.planned_rolling_queue_migration(id)).await?
                    }
                };
                if let Some(version) = planned {
                    with_store(store, move |s| {
                        s.record_rolling_queue_assigned_migration(id, version)
                    })
                    .await?;
                }
                let outcome = RollingQueueOutcome {
                    landed_sha: Some(tip),
                    detail: Some("settled by restart reconcile after ancestry check".into()),
                    ..RollingQueueOutcome::default()
                };
                let woke = with_store(store, move |s| {
                    s.settle_rolling_queue_entry(
                        id,
                        RollingQueueEntryState::Published,
                        &outcome,
                        Utc::now(),
                    )
                })
                .await?;
                settled += usize::from(woke.is_some());
            }
            None => {
                let done = with_store(store, move |s| {
                    s.requeue_gating_rolling_queue_entry(id, Utc::now())
                })
                .await?;
                requeued += usize::from(done);
            }
        }
    }
    Ok((settled, requeued))
}

/// One queue tick that respects the deploy drain (#1128): while a deploy is
/// draining no new batch is admitted (queued entries wait; the caller already
/// finished any running batch, as the loop is sequential), so the deploy's
/// `landing_in_progress` quiet-point blocker can clear. The held queue is
/// reported in `deploy_drain.held`.
pub(crate) async fn run_drain_aware_batch(
    store: &Arc<tokio::sync::Mutex<Store>>,
    launcher: &LanderLauncher,
    size: usize,
    drain: &crate::deploy_drain::DeployDrain,
) -> Result<Vec<(Uuid, RunResult)>> {
    if drain.is_draining() {
        let queued = with_store(store, |s| {
            s.list_rolling_queue_entries(Some(RollingQueueEntryState::Queued), 1)
        })
        .await?;
        if drain.hold_queue_admission(!queued.is_empty()) {
            return Ok(Vec::new());
        }
    } else {
        drain.hold_queue_admission(false);
    }
    run_next_batch(store, launcher, size).await
}

/// The daemon task: reconcile once, then run the FIFO head whenever the
/// operator has the queue enabled.
pub async fn run_rolling_queue_loop(
    store: Arc<tokio::sync::Mutex<Store>>,
    config: Arc<RuntimeConfig>,
    launcher: LanderLauncher,
    drain: Arc<crate::deploy_drain::DeployDrain>,
) {
    match reconcile_gating(&store).await {
        Ok((settled, requeued)) if settled + requeued > 0 => {
            tracing::info!(settled, requeued, "rolling queue restart reconcile");
        }
        Ok(_) => {}
        Err(error) => tracing::warn!(%error, "rolling queue restart reconcile deferred"),
    }
    let mut launcher = launcher;
    let mut interval = tokio::time::interval(TICK);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        if !config.rolling_queue_enabled.load(Ordering::Relaxed) {
            continue;
        }
        let size = (config.rolling_queue_batch_size.load(Ordering::Relaxed) as usize)
            .clamp(1, ROLLING_QUEUE_MAX_BATCH_SIZE as usize);
        let minutes = config
            .rolling_queue_gate_timeout_mins
            .load(Ordering::Relaxed);
        launcher.gate_timeout = Duration::from_secs(u64::from(minutes.max(1)) * 60);
        match run_drain_aware_batch(&store, &launcher, size, &drain).await {
            Ok(results) => {
                for (id, result) in results {
                    tracing::info!(%id, state = result.state.as_str(), "rolling queue entry settled");
                }
            }
            Err(error) => tracing::warn!(%error, "rolling queue run deferred"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::rolling_queue::RollingQueueBinding;
    use std::os::unix::fs::PermissionsExt;

    fn ok_run(stdout: &str) -> RunResult {
        classify_run(Some(0), stdout, "", || None)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_green_lander_run_records_the_published_tip() {
        let result = ok_run("publication_status=published\npublished_target_id=abc123\n");
        assert_eq!(result.state, RollingQueueEntryState::Published);
        assert_eq!(result.outcome.landed_sha.as_deref(), Some("abc123"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_lander_gate_timing_is_kept_in_the_outcome_detail() {
        let timing = "gate_timing=shards=2 base_hits=1 base_runs=1 prebuild_secs=9 wall_secs=30";
        let published = ok_run(&format!(
            "gate_timing=shards=9 base_hits=0 base_runs=9 prebuild_secs=1 wall_secs=1\n{timing}\npublished_target_id=abc123\n"
        ));
        assert_eq!(published.outcome.detail.as_deref(), Some(timing));
        let refused = classify_run(Some(1), &format!("{timing}\n"), "gate red", || None);
        assert_eq!(
            refused.outcome.detail.as_deref(),
            Some(format!("{timing}\ngate red").as_str())
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn policy_refusal_and_gate_failure_are_typed_with_failing_tests() {
        let policy = classify_run(
            Some(8),
            "policy_fence=migration_number\n",
            "rsi-rolling-land: migration must be tip + 1",
            || None,
        );
        assert_eq!(policy.state, RollingQueueEntryState::Refused);
        assert_eq!(
            policy.outcome.refusal.as_deref(),
            Some("policy_refused:migration_number")
        );

        let gate = classify_run(
            Some(1),
            "publication_status=not_published\n",
            "test rolling_queue::red ... FAILED\ntest rolling_queue::green ... ok\n",
            || None,
        );
        assert_eq!(gate.outcome.refusal.as_deref(), Some("gate_failed"));
        assert_eq!(gate.outcome.failing_tests, vec!["rolling_queue::red"]);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_gate_that_ran_no_tests_is_its_own_refusal_and_stays_bisectable() {
        let result = classify_run(
            Some(1),
            "publication_status=not_published\n",
            "affected-crate guard failed (Failed { code: Some(4) }) running scripts/run-rsid-test-shards.sh [\"shard\", \"session-05\", \"--filterset\", \"test(rolling_land)\"]: error: no tests to run",
            || None,
        );
        assert_eq!(result.state, RollingQueueEntryState::Refused);
        assert_eq!(
            result.outcome.refusal.as_deref(),
            Some(QUEUE_FILTER_MATCHES_NO_TESTS)
        );
        assert!(
            result
                .outcome
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("test(rolling_land)"))
        );
        assert!(is_bisectable(&result));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_filter_that_selects_no_test_is_found_statically_and_real_filters_pass() {
        let (_dir, _origin, work) = repos();
        sh(
            &work,
            "mkdir -p crates/rsid/src && printf '%s\\n' \
               '#[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-other-01\"))]' \
               '#[test]' 'fn queue_lands_things() {}' > crates/rsid/src/q.rs \
             && printf '%s\\n' \
               '#[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-session-05\"))]' \
               '#[test]' 'fn other_tests() {}' > crates/rsid/src/s.rs \
             && git add -A && git commit -qm tests",
        );
        let commit = head(&work);
        let find = |filters: &[&str]| {
            let filters: Vec<String> = filters.iter().map(|f| (*f).to_string()).collect();
            filter_selecting_no_tests(&work, &commit, &filters)
        };
        // The #1129 shape (the name exists, but in another shard) is
        // accepted since #1244: the lander's focused run selects from every
        // rsid library test, not from the named shard.
        assert_eq!(find(&["rsid=shard:session-05:test(queue_lands)"]), None);
        assert_eq!(find(&["rsid=shard:other-01:test(queue_lands)"]), None);
        assert_eq!(find(&["rsid=shard:other-01"]), None);
        assert_eq!(
            find(&["rsid=shard:other-99:test(queue_lands)"]).is_some(),
            true
        );
        assert_eq!(find(&["rsid=queue_lands"]), None);
        assert_eq!(
            find(&["rsid=queue_lands", "rsid=nowhere_to_be_found"]).as_deref(),
            Some("rsid=nowhere_to_be_found")
        );
        // Forms the check cannot reason about are accepted.
        assert_eq!(
            find(&["rsid=bin:rsi-rolling-land", "rsid=test:anything"]),
            None
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_filter_naming_a_module_the_source_adds_is_accepted_by_file_or_mod_declaration() {
        // #1169: `thread_stacks.rs` holds the tests of `thread_stacks::tests`
        // but never spells the module name; `lib.rs` declares it.
        let (_dir, _origin, work) = repos();
        sh(
            &work,
            "mkdir -p crates/rsid/src \
             && printf '%s\\n' 'pub mod thread_stacks;' 'pub mod renamed;' > crates/rsid/src/lib.rs \
             && printf '%s\\n' \
               '#[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-other-05\"))]' \
               'mod tests {' '#[test]' 'fn a_collector_never_returns() {}' '}' > crates/rsid/src/thread_stacks.rs \
             && printf '%s\\n' \
               '#[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-other-05\"))]' \
               '#[test]' 'fn kept() {}' > crates/rsid/src/other_name.rs \
             && git add -A && git commit -qm adds-module",
        );
        let commit = head(&work);
        let find = |filters: &[&str]| {
            let filters: Vec<String> = filters.iter().map(|f| (*f).to_string()).collect();
            filter_selecting_no_tests(&work, &commit, &filters)
        };
        // The module name is only the file's name.
        assert_eq!(find(&["rsid=shard:other-05:test(thread_stacks)"]), None);
        assert_eq!(
            find(&["rsid=shard:other-05:test(thread_stacks::tests::a_collector_never_returns)"]),
            None
        );
        // A truly absent module or function is still refused.
        assert_eq!(
            find(&["rsid=shard:other-05:test(no_such_module)"]).as_deref(),
            Some("rsid=shard:other-05:test(no_such_module)")
        );
        assert_eq!(
            find(&["rsid=shard:other-05:test(thread_stacks::tests::no_such_fn)"]).as_deref(),
            Some("rsid=shard:other-05:test(thread_stacks::tests::no_such_fn)")
        );
        // A module declared in a lib.rs that carries no gate is accepted even
        // when no gated file is named after it (a `#[path]` module).
        assert_eq!(find(&["rsid=shard:other-05:test(renamed)"]), None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_shard_filter_searches_every_package_that_declares_the_shard() {
        // #1021 S4: the store tests live in crates/rsid-store while the
        // filter keeps its `rsid=shard:...` spelling.
        let (_dir, _origin, work) = repos();
        sh(
            &work,
            "mkdir -p crates/rsid/src crates/rsid-store/src/store \
             && printf '%s\\n' '[features]' 'test-shard-mode = []' \
                'test-shard-store-04 = [\"test-shard-mode\"]' \
                'test-shard-session-01 = [\"test-shard-mode\"]' > crates/rsid/Cargo.toml \
             && printf '%s\\n' '[features]' 'test-shard-mode = []' \
                'test-shard-store-01 = [\"test-shard-mode\"]' \
                'test-shard-store-04 = [\"test-shard-mode\"]' > crates/rsid-store/Cargo.toml \
             && printf '%s\\n' '// module store::tests' \
               '#[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-store-01\"))]' \
               '#[test]' 'fn h1_store_only_keysets() {}' > crates/rsid-store/src/store/tests.rs \
             && printf '%s\\n' \
               '#[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-store-04\"))]' \
               '#[test]' 'fn remote_read_page() {}' > crates/rsid/src/remote.rs \
             && printf '%s\\n' \
               '#[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-store-04\"))]' \
               '#[test]' 'fn store_page_cursor() {}' > crates/rsid-store/src/store/cursor.rs \
             && printf '%s\\n' \
               '#[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-session-01\"))]' \
               '#[test]' 'fn session_only() {}' > crates/rsid/src/session.rs \
             && git add -A && git commit -qm split-tests",
        );
        let commit = head(&work);
        let find = |filters: &[&str]| {
            let filters: Vec<String> = filters.iter().map(|f| (*f).to_string()).collect();
            filter_selecting_no_tests(&work, &commit, &filters)
        };
        // A store-only test, named through the unchanged `rsid=` spelling.
        assert_eq!(
            find(&["rsid=shard:store-01:test(store::tests::h1_store_only_keysets)"]),
            None
        );
        // A shard shared by both packages finds a test in either of them.
        assert_eq!(find(&["rsid=shard:store-04:test(remote_read_page)"]), None);
        assert_eq!(find(&["rsid=shard:store-04:test(store_page_cursor)"]), None);
        // #1244: a name in another shard or the sibling package is accepted,
        // the plain spelling too; the lander's run selects from both packages.
        assert_eq!(
            find(&["rsid=shard:session-01:test(h1_store_only_keysets)"]),
            None
        );
        assert_eq!(find(&["rsid=h1_store_only_keysets"]), None);
        assert_eq!(find(&["rsid-store=session_only"]), None);
        assert_eq!(
            find(&["rsid=shard:store-01:test(no_such_store_test)"]).as_deref(),
            Some("rsid=shard:store-01:test(no_such_store_test)")
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_second_out_of_band_regate_is_refused_not_looped() {
        let result = classify_run(
            Some(1),
            "publication_status=not_published\nobserved_target_id=def\n",
            "rsi-rolling-land: stale retries exhausted: def overlaps the candidate after 1 re-gated retries (1 total)",
            || None,
        );
        assert_eq!(result.state, RollingQueueEntryState::Refused);
        assert_eq!(
            result.outcome.refusal.as_deref(),
            Some(QUEUE_REGATE_EXHAUSTED)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn an_unknown_publication_is_settled_from_ancestry() {
        let landed = classify_run(Some(3), "publication_status=unknown\n", "", || {
            Some("f00d".into())
        });
        assert_eq!(landed.state, RollingQueueEntryState::Published);
        assert_eq!(landed.outcome.landed_sha.as_deref(), Some("f00d"));
        let unknown = classify_run(Some(3), "publication_status=unknown\n", "", || None);
        assert_eq!(unknown.state, RollingQueueEntryState::Failed);
        assert_eq!(
            unknown.outcome.refusal.as_deref(),
            Some("publication_unknown")
        );
    }

    fn sh(dir: &Path, script: &str) {
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success(), "{script}");
    }

    /// origin (bare) + a work clone with one commit on `rolling`.
    fn repos() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let origin = dir.path().join("origin.git");
        let work = dir.path().join("work");
        sh(dir.path(), "git init -q --bare -b rolling origin.git");
        sh(dir.path(), "git clone -q origin.git work 2>/dev/null");
        sh(
            &work,
            "git config user.email t@t && git config user.name t && git checkout -q -b rolling \
             && mkdir -p crates/rsid-store/src/store \
             && printf 'pub const LATEST_SCHEMA_VERSION: i32 = 900;\\n' > crates/rsid-store/src/store/mod.rs \
             && git add -A && git commit -qm base && git push -q origin rolling",
        );
        (dir, origin, work)
    }

    fn head(repo: &Path) -> String {
        git(repo, &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn source_facts_report_the_migration_and_hot_files_a_source_adds() {
        let (_dir, _origin, work) = repos();
        sh(
            &work,
            "git checkout -q -b feature \
             && mkdir -p crates/rsid-store/src/store/migrations crates/rsid/src \
             && printf 'impl Store {}\\n' > crates/rsid-store/src/store/migrations/v901.rs \
             && printf '// hot\\n' > crates/rsid-store/src/config.rs \
             && printf '// router\\n' > crates/rsid/src/rpc.rs \
             && git add -A && git commit -qm feature",
        );
        let source = head(&work);
        assert!(source_commit_exists(&work, &source));
        assert!(!source_commit_exists(&work, &"1".repeat(40)));
        let facts = derive_source_facts(&work, &source);
        assert_eq!(facts.migration_version, Some(901));
        assert_eq!(
            facts.hot_files,
            vec!["crates/rsid-store/src/config.rs".to_string()]
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn landed_tip_reports_ancestry_of_origin_rolling() {
        let (_dir, _origin, work) = repos();
        let base = head(&work);
        assert_eq!(landed_tip(&work, &base), Some(base.clone()));
        sh(
            &work,
            "git checkout -q -b feature && echo x > x && git add x && git commit -qm x",
        );
        assert_eq!(landed_tip(&work, &head(&work)), None);
    }

    fn fake_lander(dir: &Path, body: &str) -> LanderLauncher {
        let path = dir.join("fake-lander.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        LanderLauncher::new(path)
    }

    async fn queued(
        store: &Arc<tokio::sync::Mutex<Store>>,
        repo: &Path,
        commit: &str,
        key: &str,
    ) -> Uuid {
        queued_by(store, repo, commit, key, Uuid::new_v4()).await
    }

    async fn queued_by(
        store: &Arc<tokio::sync::Mutex<Store>>,
        repo: &Path,
        commit: &str,
        key: &str,
        session: Uuid,
    ) -> Uuid {
        let new = crate::store::rolling_queue::NewQueueEntry {
            project_id: None,
            repo_path: repo.display().to_string(),
            source_commit: commit.into(),
            source_session_id: session,
            owner_epic_id: None,
            binding: RollingQueueBinding::Unbound,
            work_key: None,
            migration_version: None,
            hot_files: vec![],
            test_filters: vec!["rsid=rolling_queue".into()],
            idempotency_key: key.into(),
        };
        store
            .lock()
            .await
            .enqueue_rolling_queue_source(&new, Utc::now())
            .unwrap()
            .0
            .id
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn run_next_settles_a_green_run_and_wakes_the_owner_once() {
        let (dir, _origin, work) = repos();
        let sha = commit_file(&work, "a", "a", "a");
        let log = dir.path().join("lander.log");
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let launcher = merging_lander(dir.path(), &work, &log, "");
        let id = queued(&store, &work, &sha, "k").await;
        let (settled, result) = run_next(&store, &launcher).await.unwrap().unwrap();
        assert_eq!(settled, id);
        assert_eq!(result.state, RollingQueueEntryState::Published);
        assert!(run_next(&store, &launcher).await.unwrap().is_none());
        let landed = entry_of(&store, id).await.outcome.unwrap().landed_sha;
        assert_eq!(landed.as_deref(), Some(head(&work).as_str()));
        assert!(is_ancestor_of_origin(&work, &sha));
        assert_eq!(wake_count(&store).await, 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_draining_deploy_admits_no_new_batch_and_the_running_one_finishes() {
        let (dir, _origin, work) = repos();
        let first = commit_file(&work, "a", "a", "a");
        let second = commit_file(&work, "b", "b", "b");
        let log = dir.path().join("lander.log");
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let launcher = merging_lander(dir.path(), &work, &log, "");
        let a = queued(&store, &work, &first, "a").await;
        let b = queued(&store, &work, &second, "b").await;
        let drain = crate::deploy_drain::DeployDrain::new();

        // No deploy: the head batch (size 1) runs to settlement.
        let ran = run_drain_aware_batch(&store, &launcher, 1, &drain)
            .await
            .unwrap();
        assert_eq!(ran.len(), 1);
        assert_eq!(ran[0].0, a);
        assert_eq!(
            entry_of(&store, a).await.state,
            RollingQueueEntryState::Published
        );

        // A deploy is requested while `b` is still queued.
        let now = Utc::now();
        drain.sync(
            Some(&crate::store::agent_deploys::DeployRow {
                id: Uuid::new_v4(),
                owner_session_id: Some(Uuid::new_v4()),
                sha: "0".repeat(40),
                manifest: Vec::new(),
                state: rsi_common::agent_deploy::DeployState::Staged,
                reason: None,
                deadline_at: now + chrono::Duration::minutes(5),
                operator: false,
                forced: false,
            }),
            true,
            now,
        );
        let held = run_drain_aware_batch(&store, &launcher, 1, &drain)
            .await
            .unwrap();
        assert!(held.is_empty(), "no batch is admitted while draining");
        assert_eq!(
            entry_of(&store, b).await.state,
            RollingQueueEntryState::Queued
        );
        assert_eq!(drain.status().held.len(), 1);
        assert_eq!(drain.status().held[0].kind, "queue_batch");
        // The queued-but-unadmitted entry is not a quiet-point blocker.
        let blockers = store
            .lock()
            .await
            .deploy_quiet_blockers(Uuid::new_v4())
            .unwrap();
        assert!(!blockers.contains(&"landing_in_progress"), "{blockers:?}");

        // The deploy settles; the queue resumes and lands `b`.
        drain.sync(None, true, Utc::now());
        let ran = run_drain_aware_batch(&store, &launcher, 1, &drain)
            .await
            .unwrap();
        assert_eq!(ran.len(), 1);
        assert_eq!(ran[0].0, b);
        assert_eq!(
            entry_of(&store, b).await.state,
            RollingQueueEntryState::Published
        );
        assert!(drain.status().held.is_empty());
    }

    /// A workspace lander that records where and with what build directory it
    /// ran, then reports an unpublished refusal (the settlement is not under
    /// test).
    fn recording_lander(dir: &Path, log: &Path, source: &str, workspace: &Path) -> LanderLauncher {
        let body = r#"
echo "cwd $(pwd -P)" >> @LOG@
echo "target ${CARGO_TARGET_DIR:-unset}" >> @LOG@
echo "args $*" >> @LOG@
echo "head $(git rev-parse HEAD)" >> @LOG@
git cat-file -e @SRC@^{commit} && echo "reachable yes" >> @LOG@
echo publication_status=not_published
exit 1
"#
        .replace("@LOG@", &log.display().to_string())
        .replace("@SRC@", source);
        fake_lander(dir, &body).with_workspace(workspace.to_path_buf())
    }

    fn logged(log: &Path, key: &str) -> Vec<String> {
        std::fs::read_to_string(log)
            .unwrap()
            .lines()
            .filter_map(|line| line.strip_prefix(&format!("{key} ")).map(str::to_string))
            .collect()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn the_queue_lands_in_its_own_worktree_with_its_own_cargo_target() {
        let (dir, _origin, work) = repos();
        let sha = commit_file(&work, "a", "a", "a");
        // The operator's checkout is on `a` with a dirty file; it must not move.
        sh(&work, "echo scratch > operator-note.txt");
        let operator_head = head(&work);
        let log = dir.path().join("lander.log");
        let workspace = dir.path().join("queue");
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let launcher = recording_lander(dir.path(), &log, &sha, &workspace);
        queued(&store, &work, &sha, "k").await;
        run_next(&store, &launcher).await.unwrap().unwrap();

        let target = workspace.join("target");
        assert!(target.is_dir(), "the queue creates its persistent target");
        assert_eq!(
            logged(&log, "target"),
            vec![target.display().to_string()],
            "the lander gets the queue's own CARGO_TARGET_DIR"
        );
        let cwd = PathBuf::from(&logged(&log, "cwd")[0]);
        assert!(cwd.starts_with(workspace.join("worktrees").canonicalize().unwrap()));
        assert_ne!(cwd, work.canonicalize().unwrap());
        assert_eq!(
            logged(&log, "args")[0]
                .split_whitespace()
                .nth(1)
                .map(PathBuf::from),
            Some(cwd.clone()),
            "--repo is the queue worktree"
        );
        assert_eq!(logged(&log, "reachable"), vec!["yes".to_string()]);
        // Detached at the fetched rolling tip, not at the operator's HEAD.
        let rolling = git(&work, &["rev-parse", "origin/rolling"]).unwrap();
        assert_eq!(logged(&log, "head"), vec![rolling.trim().to_string()]);
        assert_ne!(rolling.trim(), operator_head);
        // The operator checkout is untouched.
        assert_eq!(head(&work), operator_head);
        assert!(work.join("operator-note.txt").is_file());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn the_queue_worktree_is_reused_and_refreshed_between_landings() {
        let (dir, _origin, work) = repos();
        let first = commit_file(&work, "a", "a", "a");
        let log = dir.path().join("lander.log");
        let workspace = dir.path().join("queue");
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let launcher = recording_lander(dir.path(), &log, &first, &workspace);
        queued(&store, &work, &first, "k1").await;
        run_next(&store, &launcher).await.unwrap().unwrap();
        // rolling moves; the next landing sees the new tip in the same place.
        sh(
            &work,
            "git checkout -q rolling && echo more > more.txt && git add more.txt \
             && git commit -qm more && git push -q origin rolling",
        );
        let advanced = head(&work);
        let second = commit_file(&work, "b", "b", "b");
        queued(&store, &work, &second, "k2").await;
        run_next(&store, &launcher).await.unwrap().unwrap();

        let cwds = logged(&log, "cwd");
        assert_eq!(cwds.len(), 2);
        assert_eq!(cwds[0], cwds[1], "one reusable worktree");
        assert_eq!(logged(&log, "head")[1], advanced);
    }

    /// The operator checkout with a dirty tracked file and an untracked file:
    /// a refused queue must leave both exactly as they are.
    fn dirty_operator(work: &Path) -> (String, PathBuf) {
        sh(work, "echo scratch > operator-note.txt");
        sh(work, "echo changed >> crates/rsid-store/src/store/mod.rs");
        (head(work), work.join("operator-note.txt"))
    }

    fn assert_operator_untouched(work: &Path, head_before: &str, note: &Path) {
        assert_eq!(head(work), head_before);
        assert!(note.is_file(), "untracked operator file survives `clean`");
        let dirty =
            std::fs::read_to_string(work.join("crates/rsid-store/src/store/mod.rs")).unwrap();
        assert!(
            dirty.ends_with("changed\n"),
            "tracked edit survives `checkout --force`"
        );
    }

    fn worktree_dir(workspace: &Path, work: &Path) -> PathBuf {
        let common = git(
            work,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .unwrap()
        .trim()
        .to_string();
        workspace.join("worktrees").join(stable_key(&common))
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_symlinked_worktree_dir_is_refused_and_the_operator_checkout_is_untouched() {
        let (dir, _origin, work) = repos();
        let (before, note) = dirty_operator(&work);
        let workspace = dir.path().join("queue");
        prepare_queue_workspace(&workspace).unwrap();
        // An attacker (or a bad config) aims the worktree slot at the operator
        // checkout, whose common dir matches: the old health check accepted it.
        let slot = worktree_dir(&workspace, &work);
        std::os::unix::fs::symlink(&work, &slot).unwrap();
        let error = ensure_queue_worktree(&workspace, &work).unwrap_err();
        assert!(error.contains("symlink"), "{error}");
        assert!(slot.symlink_metadata().unwrap().file_type().is_symlink());
        assert_operator_untouched(&work, &before, &note);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_slot_swapped_for_a_symlink_after_validation_cannot_redirect_the_reset() {
        let (dir, _origin, work) = repos();
        let workspace = dir.path().join("queue");
        let slot = ensure_queue_worktree(&workspace, &work).unwrap();
        std::fs::write(slot.join("queue-scratch.txt"), "junk").unwrap();
        let (before, note) = dirty_operator(&work);
        let common = git(
            &work,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .unwrap()
        .trim()
        .to_string();
        // Validation passes against the real queue worktree...
        let pinned = pin_queue_worktree(&workspace, &slot, &common).unwrap();
        // ...then the slot is swapped for a symlink to the operator checkout.
        let moved = dir.path().join("moved-queue-worktree");
        std::fs::rename(&slot, &moved).unwrap();
        std::os::unix::fs::symlink(&work, &slot).unwrap();
        reset_pinned_worktree(&pinned, &head(&work)).unwrap();
        // The symlink target keeps its tracked and untracked edits.
        assert_operator_untouched(&work, &before, &note);
        // The command ran in the validated worktree instead.
        assert!(!moved.join("queue-scratch.txt").exists());
        assert!(moved.join(".git").is_file());
        // A slot that is already a symlink is refused at validation.
        assert!(pin_queue_worktree(&workspace, &slot, &common).is_err());
    }

    /// #1141 (#1126 F9): the registration (git admin dir) is swapped for a
    /// symlink to the operator checkout's git dir between validation and the
    /// destructive `checkout --force` / `clean`. Git must not follow it: the
    /// command is refused and the operator checkout (HEAD, index, tracked and
    /// untracked edits) is exactly as it was.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_registration_swapped_for_a_symlink_cannot_redirect_the_reset() {
        let (dir, _origin, work) = repos();
        let workspace = dir.path().join("queue");
        let slot = ensure_queue_worktree(&workspace, &work).unwrap();
        std::fs::write(slot.join("queue-scratch.txt"), "junk").unwrap();
        // The operator checkout moves ahead of the queue worktree's commit.
        sh(
            &work,
            "echo ahead > ahead.txt && git add ahead.txt && git commit -qm ahead",
        );
        let (before, note) = dirty_operator(&work);
        let operator_index = std::fs::read(work.join(".git/index")).unwrap();
        let operator_head = std::fs::read(work.join(".git/HEAD")).unwrap();
        let common = git(
            &work,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .unwrap()
        .trim()
        .to_string();
        let pinned = pin_queue_worktree(&workspace, &slot, &common).unwrap();
        // Between the back-pointer check and the destructive command the
        // registration is replaced by a symlink to the operator's git dir.
        let registration = pinned.admin.clone();
        let moved = dir.path().join("moved-registration");
        std::fs::rename(&registration, &moved).unwrap();
        std::os::unix::fs::symlink(work.join(".git"), &registration).unwrap();
        // The queue worktree is not at the tip the operator checkout is at, so a
        // redirected `checkout --force` would rewrite the operator's HEAD/index.
        let rev = git(&work, &["rev-parse", "HEAD~1"])
            .unwrap()
            .trim()
            .to_string();
        let error = reset_pinned_worktree(&pinned, &rev).unwrap_err();
        assert!(error.contains("registration"), "{error}");
        assert_operator_untouched(&work, &before, &note);
        assert_eq!(
            std::fs::read(work.join(".git/index")).unwrap(),
            operator_index
        );
        assert_eq!(
            std::fs::read(work.join(".git/HEAD")).unwrap(),
            operator_head
        );
        // The queue worktree was not reset either: the refusal came first.
        assert!(slot.join("queue-scratch.txt").exists());
        // A fresh validation refuses the swapped registration too.
        assert!(pin_queue_worktree(&workspace, &slot, &common).is_err());
    }

    /// #1141: a registration that is moved (not replaced) is also refused, and
    /// git is never run against the moved copy.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_moved_registration_is_refused_before_git_runs() {
        let (dir, _origin, work) = repos();
        let workspace = dir.path().join("queue");
        let slot = ensure_queue_worktree(&workspace, &work).unwrap();
        std::fs::write(slot.join("queue-scratch.txt"), "junk").unwrap();
        let common = git(
            &work,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .unwrap()
        .trim()
        .to_string();
        let pinned = pin_queue_worktree(&workspace, &slot, &common).unwrap();
        std::fs::rename(&pinned.admin, dir.path().join("moved-registration")).unwrap();
        let error = reset_pinned_worktree(&pinned, &head(&work)).unwrap_err();
        assert!(error.contains("registration"), "{error}");
        assert!(slot.join("queue-scratch.txt").exists());
    }

    fn common_dir(work: &Path) -> String {
        git(
            work,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .unwrap()
        .trim()
        .to_string()
    }

    /// `(asleep, voluntary context switches)` of the process whose environment
    /// carries `marker`.
    fn probe_marked_process(marker: &str) -> Option<(bool, u64)> {
        let entries = std::fs::read_dir("/proc").ok()?;
        entries.flatten().find_map(|entry| {
            let dir = entry.path();
            let environ = std::fs::read(dir.join("environ")).ok()?;
            if !String::from_utf8_lossy(&environ).contains(marker) {
                return None;
            }
            let stat = std::fs::read_to_string(dir.join("stat")).ok()?;
            let asleep = stat
                .rsplit(')')
                .next()
                .and_then(|rest| rest.split_whitespace().next())
                == Some("S");
            let status = std::fs::read_to_string(dir.join("status")).ok()?;
            let switches = status
                .lines()
                .find_map(|line| line.strip_prefix("voluntary_ctxt_switches:"))?
                .trim()
                .parse()
                .ok()?;
            Some((asleep, switches))
        })
    }

    /// Reset `pinned` to `rev` and park the `target` git subcommand on a FIFO
    /// that is its global config (git reads it before it looks at its cwd, again
    /// after it has normalized the work tree to an absolute path and before it
    /// enters that path, and again after). At the second read the queue slot is
    /// renamed away and replaced by a symlink to `decoy`, so git's `chdir` by
    /// pathname lands in `decoy` (#1160).
    fn reset_with_slot_swapped_in_flight(
        pinned: &PinnedWorktree,
        rev: &str,
        target: &str,
        slot: &Path,
        moved: &Path,
        decoy: &Path,
    ) -> std::result::Result<(), String> {
        use std::os::unix::fs::OpenOptionsExt;
        let fifo = moved.with_file_name(format!("config-{}.fifo", Uuid::new_v4()));
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();
        let marker = fifo.display().to_string();
        // A write open succeeds only while git has the FIFO open (or is blocked
        // opening it) for reading; closing it again hands git an empty config.
        let release = || {
            let opened = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(nix::libc::O_NONBLOCK)
                .open(&fifo);
            opened.is_ok()
        };
        // Asleep in a blocking read-open for several polls, past `after`.
        let park = |after: u64| -> u64 {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
            let mut stable = 0;
            loop {
                assert!(
                    std::time::Instant::now() < deadline,
                    "git never parked on the config fifo"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
                match probe_marked_process(&marker) {
                    Some((true, switches)) if switches > after => stable += 1,
                    _ => stable = 0,
                }
                if stable >= 5 {
                    return probe_marked_process(&marker).map_or(after, |probed| probed.1);
                }
            }
        };
        std::thread::scope(|scope| {
            let runner = scope.spawn(|| {
                reset_pinned_worktree_with_env(
                    pinned,
                    rev,
                    &|command| match command == target {
                        true => vec![("GIT_CONFIG_GLOBAL".to_string(), marker.clone())],
                        false => Vec::new(),
                    },
                    &|_, _| {},
                )
            });
            let first = park(0);
            while !release() {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            park(first);
            std::fs::rename(slot, moved).unwrap();
            std::os::unix::fs::symlink(decoy, slot).unwrap();
            while !runner.is_finished() {
                release();
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            runner.join().unwrap()
        })
    }

    /// Mode and modification time: what a hook, a filter or a redirected git
    /// could change without touching a file's bytes.
    fn mode_and_mtime(path: &Path) -> (u32, std::time::SystemTime) {
        use std::os::unix::fs::MetadataExt;
        let meta =
            std::fs::metadata(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        (meta.mode(), meta.modified().unwrap())
    }

    /// #1160 major 1: git normalizes its work tree to an absolute path and later
    /// enters it by that pathname, so a slot swapped for the operator checkout
    /// inside git's own process redirects the destructive step. Whichever step
    /// is redirected (the worktree rewrite or `clean`), the operator checkout's
    /// tracked edits, untracked files, index, HEAD, modes and timestamps must be
    /// exactly as they were, and the reset must fail closed.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_slot_swapped_inside_gits_own_process_cannot_redirect_the_reset() {
        for target in ["read-tree", "clean"] {
            let (dir, _origin, work) = repos();
            let workspace = dir.path().join("queue");
            let slot = ensure_queue_worktree(&workspace, &work).unwrap();
            let (before, note) = dirty_operator(&work);
            let tracked = work.join("crates/rsid-store/src/store/mod.rs");
            let operator_index = std::fs::read(work.join(".git/index")).unwrap();
            let operator_head = std::fs::read(work.join(".git/HEAD")).unwrap();
            let metadata = (mode_and_mtime(&tracked), mode_and_mtime(&note));
            let pinned = pin_queue_worktree(&workspace, &slot, &common_dir(&work)).unwrap();
            let moved = dir.path().join("moved-queue-worktree");
            let outcome =
                reset_with_slot_swapped_in_flight(&pinned, &before, target, &slot, &moved, &work);
            assert_operator_untouched(&work, &before, &note);
            assert!(
                std::fs::read(work.join(".git/index")).unwrap() == operator_index,
                "the operator index was rewritten"
            );
            assert!(
                std::fs::read(work.join(".git/HEAD")).unwrap() == operator_head,
                "the operator HEAD was rewritten"
            );
            assert!(
                (mode_and_mtime(&tracked), mode_and_mtime(&note)) == metadata,
                "an operator file's mode or timestamp changed"
            );
            assert!(outcome.is_err(), "{target} must fail closed: {outcome:?}");
        }
    }

    /// #1160 major 2: the registration directory keeps its identity when one of
    /// its entries is replaced by a symlink. The reset never follows it: it
    /// fails closed and the operator's index stays byte-identical.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_registration_entry_swapped_for_a_symlink_cannot_rewrite_the_operator_index() {
        let (dir, _origin, work) = repos();
        let workspace = dir.path().join("queue");
        let slot = ensure_queue_worktree(&workspace, &work).unwrap();
        std::fs::write(slot.join("queue-scratch.txt"), "junk").unwrap();
        sh(
            &work,
            "echo ahead > ahead.txt && git add ahead.txt && git commit -qm ahead",
        );
        let (before, note) = dirty_operator(&work);
        let operator_index = std::fs::read(work.join(".git/index")).unwrap();
        let operator_head = std::fs::read(work.join(".git/HEAD")).unwrap();
        let pinned = pin_queue_worktree(&workspace, &slot, &common_dir(&work)).unwrap();
        // The directory (device, inode, location) is unchanged; only its index
        // entry now names the operator's index.
        let index = pinned.admin.join("index");
        std::fs::remove_file(&index).unwrap();
        std::os::unix::fs::symlink(work.join(".git/index"), &index).unwrap();
        let rev = git(&work, &["rev-parse", "HEAD~1"])
            .unwrap()
            .trim()
            .to_string();
        let outcome = reset_pinned_worktree(&pinned, &rev);
        assert!(
            std::fs::read(work.join(".git/index")).unwrap() == operator_index,
            "the operator index was rewritten"
        );
        assert!(
            std::fs::read(work.join(".git/HEAD")).unwrap() == operator_head,
            "the operator HEAD was rewritten"
        );
        assert_operator_untouched(&work, &before, &note);
        assert!(outcome.is_err(), "must fail closed: {outcome:?}");
    }

    /// #1160 blocker: a directory grant covers a hard link planted under it, so
    /// the reset must never write in place to a pre-existing file there. The
    /// registration's `logs/HEAD` (which git appends to), `index` and `ORIG_HEAD`
    /// are hard links to operator files, and so are a tracked and an untracked
    /// file in the queue worktree. The reset succeeds and every operator file
    /// keeps its bytes, mode and modification time.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn hard_links_to_operator_files_are_never_written_through() {
        let (dir, _origin, work) = repos();
        let workspace = dir.path().join("queue");
        let slot = ensure_queue_worktree(&workspace, &work).unwrap();
        sh(
            &work,
            "echo ahead > ahead.txt && git add ahead.txt && git commit -qm ahead \
             && echo secret-a > operator-a.txt && echo secret-b > operator-b.txt",
        );
        let ahead = head(&work);
        let operator_files = [
            work.join(".git/index"),
            work.join(".git/config"),
            work.join(".git/description"),
            work.join("operator-a.txt"),
            work.join("operator-b.txt"),
        ];
        let before: Vec<_> = operator_files
            .iter()
            .map(|file| (std::fs::read(file).unwrap(), mode_and_mtime(file)))
            .collect();
        let pinned = pin_queue_worktree(&workspace, &slot, &common_dir(&work)).unwrap();
        let alias = |operator: &Path, name: &Path| {
            let _ = std::fs::remove_file(name);
            std::fs::create_dir_all(name.parent().unwrap()).unwrap();
            std::fs::hard_link(operator, name).unwrap();
        };
        alias(&operator_files[0], &pinned.admin.join("logs/HEAD"));
        alias(&operator_files[1], &pinned.admin.join("index"));
        alias(&operator_files[2], &pinned.admin.join("ORIG_HEAD"));
        // A tracked file the reset rewrites, and an untracked file `clean` removes.
        alias(
            &operator_files[3],
            &slot.join("crates/rsid-store/src/store/mod.rs"),
        );
        alias(&operator_files[4], &slot.join("untracked-link.txt"));
        reset_pinned_worktree(&pinned, &ahead).unwrap();
        for (file, (bytes, metadata)) in operator_files.iter().zip(&before) {
            assert!(
                &std::fs::read(file).unwrap() == bytes,
                "{} was written through a hard link",
                file.display()
            );
            assert!(
                &mode_and_mtime(file) == metadata,
                "{} changed mode or timestamp",
                file.display()
            );
        }
        // The reset itself worked: the slot is at the requested commit.
        assert_eq!(
            std::fs::read_to_string(pinned.admin.join("HEAD"))
                .unwrap()
                .trim(),
            ahead
        );
        assert_eq!(
            std::fs::read_to_string(slot.join("crates/rsid-store/src/store/mod.rs")).unwrap(),
            "pub const LATEST_SCHEMA_VERSION: i32 = 900;\n"
        );
        assert!(slot.join("ahead.txt").is_file());
        assert!(slot.join("untracked-link.txt").symlink_metadata().is_err());
    }

    /// #1160 major 3: Landlock does not stop chmod or utimes, so no program the
    /// operator's repository configures may run during the reset. A smudge
    /// filter and a `post-index-change` hook in the operator repo each chmod an
    /// operator file; neither may run, and the checked-out bytes are unfiltered.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_operators_hooks_and_filters_do_not_run_during_the_reset() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, _origin, work) = repos();
        let workspace = dir.path().join("queue");
        let slot = ensure_queue_worktree(&workspace, &work).unwrap();
        let victim = work.join("operator-victim.txt");
        std::fs::write(&victim, "victim\n").unwrap();
        let metadata = mode_and_mtime(&victim);
        let script = |name: &str, body: &str| {
            let path = dir.path().join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        let chmod = format!("chmod 000 {}", victim.display());
        let smudge = script("smudge.sh", &format!("{chmod}\nsed 's/^/FILTERED:/'"));
        let hook = script("hook.sh", &chmod);
        sh(
            &work,
            "printf '*.evil filter=evil\\n' > .gitattributes && echo payload > a.evil \
             && git add .gitattributes a.evil && git commit -qm filtered",
        );
        // Armed after the operator's own commit, so only the queue triggers them.
        std::fs::copy(&hook, work.join(".git/hooks/post-index-change")).unwrap();
        sh(
            &work,
            &format!("git config filter.evil.smudge {}", smudge.display()),
        );
        let pinned = pin_queue_worktree(&workspace, &slot, &common_dir(&work)).unwrap();
        reset_pinned_worktree(&pinned, &head(&work)).unwrap();
        assert_eq!(
            std::fs::read_to_string(slot.join("a.evil")).unwrap(),
            "payload\n"
        );
        assert_eq!(mode_and_mtime(&victim), metadata);
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "victim\n");
    }

    /// #1160: ABI 1 and 2 cannot deny truncation, so the version policy refuses
    /// them (and a missing Landlock) with the typed unsupported-fence code.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_fence_version_policy_requires_truncation_control() {
        for abi in [0, 1, 2] {
            let error = write_fence::require_abi(Ok(abi)).unwrap_err();
            assert!(error.starts_with(write_fence::UNSUPPORTED_CODE), "{error}");
        }
        let missing = std::io::Error::from_raw_os_error(nix::libc::ENOSYS);
        let error = write_fence::require_abi(Err(missing)).unwrap_err();
        assert!(error.starts_with(write_fence::UNSUPPORTED_CODE), "{error}");
        assert_eq!(write_fence::require_abi(Ok(3)).unwrap(), 3);
        assert_eq!(write_fence::require_abi(Ok(7)).unwrap(), 7);
    }

    /// #1160: a host whose Landlock is too old settles the batch with the typed
    /// refusal before any worktree is created or reset; the operator checkout is
    /// untouched.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_host_without_the_write_fence_settles_with_the_typed_refusal() {
        let (dir, _origin, work) = repos();
        let sha = commit_file(&work, "a", "a", "a");
        let (before, note) = dirty_operator(&work);
        let log = dir.path().join("lander.log");
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let mut launcher = recording_lander(dir.path(), &log, &sha, &dir.path().join("queue"));
        launcher.fence_abi = || Ok(2);
        queued(&store, &work, &sha, "k").await;
        let (_, result) = run_next(&store, &launcher).await.unwrap().unwrap();
        assert_eq!(result.state, RollingQueueEntryState::Failed);
        assert_eq!(
            result.outcome.refusal.as_deref(),
            Some(write_fence::UNSUPPORTED_CODE)
        );
        let detail = result.outcome.detail.unwrap_or_default();
        assert!(detail.contains("ABI 2"), "{detail}");
        assert!(
            !dir.path()
                .join("queue/worktrees")
                .read_dir()
                .unwrap()
                .any(|_| true)
        );
        assert_operator_untouched(&work, &before, &note);
    }

    /// A hard link to `operator` named `link` (an alias a granted directory
    /// would let a write pass through).
    fn alias_file(operator: &Path, link: &Path) {
        let _ = std::fs::remove_file(link);
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::fs::hard_link(operator, link).unwrap();
    }

    /// Operator state whose bytes, mode and timestamps a test pins.
    fn pin_files(files: &[PathBuf]) -> Vec<(Vec<u8>, (u32, std::time::SystemTime))> {
        files
            .iter()
            .map(|file| (std::fs::read(file).unwrap(), mode_and_mtime(file)))
            .collect()
    }

    fn assert_files_unchanged(
        files: &[PathBuf],
        pinned: &[(Vec<u8>, (u32, std::time::SystemTime))],
    ) {
        for (file, (bytes, metadata)) in files.iter().zip(pinned) {
            assert!(
                &std::fs::read(file).unwrap() == bytes,
                "{} was written",
                file.display()
            );
            assert!(
                &mode_and_mtime(file) == metadata,
                "{} changed mode or timestamp",
                file.display()
            );
        }
    }

    /// A `#!/bin/sh` script under `dir` that chmods `victim` to 000.
    fn chmod_script(dir: &Path, name: &str, victim: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(
            &path,
            format!("#!/bin/sh\nchmod 000 {}\n", victim.display()),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// #1160 (first use): the queue worktree is first populated through the
    /// same pinned, fenced runner as every later reset. A slot swapped for the
    /// operator checkout while the FIRST population's git is in flight is
    /// refused and leaves the operator checkout untouched.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_slot_swapped_during_the_first_population_cannot_redirect_it() {
        for target in ["read-tree", "clean"] {
            let (dir, _origin, work) = repos();
            let (before, note) = dirty_operator(&work);
            let tracked = work.join("crates/rsid-store/src/store/mod.rs");
            let operator_index = std::fs::read(work.join(".git/index")).unwrap();
            let metadata = (mode_and_mtime(&tracked), mode_and_mtime(&note));
            let workspace = dir.path().join("queue");
            prepare_queue_workspace(&workspace).unwrap();
            let slot = worktree_dir(&workspace, &work);
            // The registration exists, nothing is checked out yet.
            create_queue_registration(&work, &slot, &before).unwrap();
            assert!(
                slot.join("crates").symlink_metadata().is_err(),
                "the registration step must not check anything out"
            );
            let pinned = pin_queue_worktree(&workspace, &slot, &common_dir(&work)).unwrap();
            let moved = dir.path().join("moved-queue-worktree");
            let outcome =
                reset_with_slot_swapped_in_flight(&pinned, &before, target, &slot, &moved, &work);
            assert_operator_untouched(&work, &before, &note);
            assert!(
                std::fs::read(work.join(".git/index")).unwrap() == operator_index,
                "the operator index was rewritten"
            );
            assert!(
                (mode_and_mtime(&tracked), mode_and_mtime(&note)) == metadata,
                "an operator file's mode or timestamp changed"
            );
            assert!(outcome.is_err(), "{target} must fail closed: {outcome:?}");
        }
    }

    /// #1160 (first use): creating the worktree must not run the operator's
    /// hooks or filters either (`git worktree add` would run a checkout, its
    /// filters and `post-checkout`).
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_first_creation_runs_no_operator_hook_or_filter() {
        let (dir, _origin, work) = repos();
        let victim = work.join("operator-victim.txt");
        std::fs::write(&victim, "victim\n").unwrap();
        let metadata = mode_and_mtime(&victim);
        let smudge = dir.path().join("smudge.sh");
        std::fs::write(
            &smudge,
            format!(
                "#!/bin/sh\nchmod 000 {}\nsed 's/^/FILTERED:/'\n",
                victim.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&smudge, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        sh(
            &work,
            "printf '*.evil filter=evil\\n' > .gitattributes && echo payload > a.evil \
             && git add .gitattributes a.evil && git commit -qm filtered && git push -q origin rolling",
        );
        let hook = chmod_script(dir.path(), "hook.sh", &victim);
        for name in ["post-checkout", "post-index-change"] {
            std::fs::copy(&hook, work.join(".git/hooks").join(name)).unwrap();
        }
        sh(
            &work,
            &format!("git config filter.evil.smudge {}", smudge.display()),
        );
        let workspace = dir.path().join("queue");
        let slot = ensure_queue_worktree(&workspace, &work).unwrap();
        assert_eq!(
            std::fs::read_to_string(slot.join("a.evil")).unwrap(),
            "payload\n"
        );
        assert_eq!(mode_and_mtime(&victim), metadata);
    }

    /// #1170 (R2): the source fetch runs no hook of the source repository, no
    /// automatic gc or maintenance and writes no FETCH_HEAD, while still
    /// advancing `origin/rolling`.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_source_fetch_runs_no_hook_or_maintenance_and_writes_no_fetch_head() {
        let (dir, origin, work) = repos();
        let marker = dir.path().join("hook-ran");
        let hook = work.join(".git/hooks/reference-transaction");
        std::fs::write(
            &hook,
            format!("#!/bin/sh\necho ran >> {}\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        // Automatic gc would run (and pack the loose objects) after any fetch.
        sh(
            &work,
            "git config gc.auto 1 && git config gc.autoDetach false",
        );
        sh(&work, "git config maintenance.autoDetach false");
        // Origin moves ahead through a second clone.
        let other = dir.path().join("other");
        sh(
            dir.path(),
            &format!(
                "git clone -q {} {} && cd {} && echo n > n.txt && git add n.txt \
                 && git commit -qm n && git push -q origin HEAD:rolling",
                origin.display(),
                other.display(),
                other.display()
            ),
        );
        let packs = || {
            let mut names: Vec<_> = std::fs::read_dir(work.join(".git/objects/pack"))
                .unwrap()
                .flatten()
                .map(|entry| entry.file_name())
                .collect();
            names.sort();
            names
        };
        let fetch_head = || std::fs::read(work.join(".git/FETCH_HEAD")).ok();
        let (packs_before, fetch_head_before) = (packs(), fetch_head());
        fetch_rolling(&work).unwrap();
        assert_eq!(
            git(&work, &["rev-parse", "origin/rolling"]).unwrap(),
            head(&other) + "\n",
            "the fetch itself still happens"
        );
        assert_eq!(std::fs::read_to_string(&marker).ok(), None, "no hook ran");
        assert_eq!(packs(), packs_before, "no automatic gc repacked the store");
        assert_eq!(fetch_head(), fetch_head_before, "FETCH_HEAD untouched");
    }

    /// The first population, with `plant` run just before git subcommand
    /// `target` with the private git directory's path (as a racing same-uid
    /// process would). Returns the slot.
    fn populate_with_plant(
        dir: &Path,
        work: &Path,
        target: &str,
        plant: &dyn Fn(&Path),
    ) -> PathBuf {
        let workspace = dir.join("queue");
        let slot = ensure_queue_worktree(&workspace, work).unwrap();
        let pinned = pin_queue_worktree(&workspace, &slot, &common_dir(work)).unwrap();
        let rev = git(work, &["rev-parse", "origin/rolling"]).unwrap();
        reset_pinned_worktree_with_env(
            &pinned,
            rev.trim(),
            &|_| Vec::new(),
            &|command, private| {
                if command == target {
                    plant(private);
                }
            },
        )
        .unwrap();
        slot
    }

    /// #1170 (R3): a `commondir` file planted in the private git directory does
    /// not redirect git's common directory (and with it `info/exclude`) to a
    /// directory of the planter's choosing: `clean` still removes the file a
    /// planted exclude would have protected.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_commondir_planted_in_the_private_git_dir_is_ignored() {
        let (dir, _origin, work) = repos();
        let decoy = dir.path().join("decoy-common");
        sh(
            dir.path(),
            &format!("git init -q --bare {}", decoy.display()),
        );
        std::fs::create_dir_all(decoy.join("info")).unwrap();
        std::fs::write(decoy.join("info/exclude"), "junk.txt\n").unwrap();
        let workspace = dir.path().join("queue");
        let slot = ensure_queue_worktree(&workspace, &work).unwrap();
        std::fs::write(slot.join("junk.txt"), "junk").unwrap();
        let pinned = pin_queue_worktree(&workspace, &slot, &common_dir(&work)).unwrap();
        let rev = git(&work, &["rev-parse", "origin/rolling"]).unwrap();
        reset_pinned_worktree_with_env(
            &pinned,
            rev.trim(),
            &|_| Vec::new(),
            &|command, private| {
                if command == "clean" {
                    std::fs::write(private.join("commondir"), format!("{}\n", decoy.display()))
                        .unwrap();
                }
            },
        )
        .unwrap();
        assert!(
            slot.join("junk.txt").symlink_metadata().is_err(),
            "clean honored an exclude from the planted common directory"
        );
    }

    /// #1170 (R3): a replace ref planted in the private git directory does not
    /// substitute the tree the reset checks out.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_replace_ref_planted_in_the_private_git_dir_is_ignored() {
        let (dir, _origin, work) = repos();
        // A commit with an empty tree: what a planted replace ref would
        // substitute for the real tip.
        let empty = git(&work, &["hash-object", "-t", "tree", "-w", "--stdin"]).unwrap();
        let alt = git(&work, &["commit-tree", empty.trim(), "-m", "alt"]).unwrap();
        let rev = git(&work, &["rev-parse", "origin/rolling"]).unwrap();
        let slot = populate_with_plant(dir.path(), &work, "read-tree", &|private| {
            std::fs::create_dir_all(private.join("refs/replace")).unwrap();
            std::fs::write(
                private.join("refs/replace").join(rev.trim()),
                format!("{}\n", alt.trim()),
            )
            .unwrap();
        });
        assert!(
            slot.join("crates/rsid-store/src/store/mod.rs").is_file(),
            "the checked-out tree was replaced by a planted replace ref"
        );
    }

    /// #1170 (R3): `info` is a regular file, so a directory of that name (where
    /// `attributes` would name a filter driver) cannot be added beside the files
    /// the daemon creates exclusively.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn an_info_directory_cannot_be_planted_in_the_private_git_dir() {
        let (dir, _origin, work) = repos();
        let refused = std::cell::Cell::new(false);
        populate_with_plant(dir.path(), &work, "rev-parse", &|private| {
            refused.set(std::fs::create_dir(private.join("info")).is_err());
        });
        assert!(refused.get(), "an info directory was planted");
    }

    /// #1160: the child gets a clean environment. An inherited `GIT_TRACE`,
    /// `GIT_TRACE2_EVENT` or `GIT_TRACE_SETUP` naming a hard link to an operator
    /// file is never opened for append, and the unfenced runners drop them too.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn an_inherited_git_trace_cannot_write_through_a_hard_link() {
        let (dir, _origin, work) = repos();
        let workspace = dir.path().join("queue");
        let slot = ensure_queue_worktree(&workspace, &work).unwrap();
        let operator = work.join("operator-trace-target.txt");
        std::fs::write(&operator, "operator\n").unwrap();
        let files = [operator.clone()];
        let pinned_state = pin_files(&files);
        let trace = slot.join("trace-alias.log");
        alias_file(&operator, &trace);
        let mut pinned = pin_queue_worktree(&workspace, &slot, &common_dir(&work)).unwrap();
        let var = |name: &str, value: &Path| {
            (
                std::ffi::OsString::from(name),
                std::ffi::OsString::from(value),
            )
        };
        pinned.inherited_env = Some(vec![
            var("PATH", Path::new(&std::env::var_os("PATH").unwrap())),
            var("GIT_TRACE", &trace),
            var("GIT_TRACE_SETUP", &trace),
            var("GIT_TRACE_PERFORMANCE", &trace),
            var("GIT_TRACE2", &trace),
            var("GIT_TRACE2_EVENT", &trace),
        ]);
        reset_pinned_worktree(&pinned, &head(&work)).unwrap();
        assert_files_unchanged(&files, &pinned_state);
        // The unfenced runners keep transport settings and drop the rest.
        let kept = inherited_git_env([
            (
                std::ffi::OsString::from("SSH_AUTH_SOCK"),
                std::ffi::OsString::from("/sock"),
            ),
            (
                std::ffi::OsString::from("LC_ALL"),
                std::ffi::OsString::from("C"),
            ),
            (
                std::ffi::OsString::from("GIT_TRACE"),
                std::ffi::OsString::from("/x"),
            ),
            (
                std::ffi::OsString::from("GIT_TEST_SPLIT_INDEX"),
                std::ffi::OsString::from("1"),
            ),
            (
                std::ffi::OsString::from("GIT_INDEX_FILE"),
                std::ffi::OsString::from("/x"),
            ),
        ]);
        let names: Vec<String> = kept
            .iter()
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["SSH_AUTH_SOCK", "LC_ALL"]);
    }

    /// #1160: a split registration index (its shared half beside it) is not
    /// copied as a broken half; the reset publishes a full index, and a second
    /// and third reset (each refreshing it) keep working.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_split_registration_index_is_normalized_and_survives_repeated_resets() {
        let (dir, _origin, work) = repos();
        let workspace = dir.path().join("queue");
        let slot = ensure_queue_worktree(&workspace, &work).unwrap();
        sh(&slot, "git update-index --split-index");
        let pinned = pin_queue_worktree(&workspace, &slot, &common_dir(&work)).unwrap();
        let shared = std::fs::read_dir(&pinned.admin)
            .unwrap()
            .flatten()
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("sharedindex.")
            });
        assert!(shared, "the fixture must hold a split index");
        for _ in 0..3 {
            std::fs::write(slot.join("queue-scratch.txt"), "junk").unwrap();
            reset_pinned_worktree(&pinned, &head(&work)).unwrap();
            assert!(slot.join("queue-scratch.txt").symlink_metadata().is_err());
            // The lander's own git, run in the slot by path, reads the index.
            sh(&slot, "git status --porcelain > /dev/null");
        }
    }

    /// #1160: a registration index replaced by a FIFO cannot block the reset.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_registration_index_fifo_is_refused_without_blocking() {
        let (dir, _origin, work) = repos();
        let workspace = dir.path().join("queue");
        let slot = ensure_queue_worktree(&workspace, &work).unwrap();
        let pinned = pin_queue_worktree(&workspace, &slot, &common_dir(&work)).unwrap();
        let index = pinned.admin.join("index");
        std::fs::remove_file(&index).unwrap();
        nix::unistd::mkfifo(&index, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let rev = head(&work);
        let pinned = std::sync::Arc::new(pinned);
        // Unscoped, so a regression that blocks leaves this thread behind and
        // the timeout below fails the test instead of hanging it.
        std::thread::spawn(move || {
            let _ = sender.send(reset_pinned_worktree(&pinned, &rev));
        });
        let outcome = receiver
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("the reset blocked on the FIFO");
        assert!(outcome.is_err(), "{outcome:?}");
    }

    /// Documented behaviour change (#1160): the queue's `clean -fd` no longer
    /// reads the operator's `.git/info/exclude`, so a file ignored only there
    /// is removed from the queue worktree like any other untracked file.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn clean_ignores_the_operators_private_excludes() {
        let (dir, _origin, work) = repos();
        let workspace = dir.path().join("queue");
        let slot = ensure_queue_worktree(&workspace, &work).unwrap();
        std::fs::write(
            work.join(".git/info/exclude"),
            "excluded-only-by-operator.tmp\n",
        )
        .unwrap();
        std::fs::write(slot.join("excluded-only-by-operator.tmp"), "x").unwrap();
        let pinned = pin_queue_worktree(&workspace, &slot, &common_dir(&work)).unwrap();
        reset_pinned_worktree(&pinned, &head(&work)).unwrap();
        assert!(
            slot.join("excluded-only-by-operator.tmp")
                .symlink_metadata()
                .is_err()
        );
    }

    /// #1160: a same-uid writer that plants files in the fresh private git dir
    /// after it exists and before git runs cannot enable a hook, replace the
    /// config to turn one on, or redirect an append: the settings that matter
    /// are on git's command line, `logs` is a file, and no ref is updated.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn files_planted_in_the_private_git_dir_in_flight_are_not_honored() {
        let (dir, _origin, work) = repos();
        let workspace = dir.path().join("queue");
        let slot = ensure_queue_worktree(&workspace, &work).unwrap();
        sh(
            &work,
            "echo ahead > ahead.txt && git add ahead.txt && git commit -qm ahead",
        );
        let victim = work.join("operator-victim.txt");
        std::fs::write(&victim, "victim\n").unwrap();
        let files = [
            victim.clone(),
            work.join(".git/index"),
            work.join(".git/HEAD"),
        ];
        let pinned_state = pin_files(&files);
        let hook = chmod_script(dir.path(), "plant.sh", &victim);
        let pinned = pin_queue_worktree(&workspace, &slot, &common_dir(&work)).unwrap();
        let planted = std::sync::atomic::AtomicBool::new(false);
        let plant = |_command: &str, scratch: &Path| {
            if planted.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            let hooks = scratch.join("hooks");
            std::fs::create_dir(&hooks).unwrap();
            for name in [
                "post-index-change",
                "post-checkout",
                "reference-transaction",
            ] {
                std::fs::copy(&hook, hooks.join(name)).unwrap();
            }
            // A replacement config that turns hooks, reflogs and a filter on.
            std::fs::remove_file(scratch.join("config")).unwrap();
            std::fs::write(
                scratch.join("config"),
                format!(
                    "[core]\n\trepositoryformatversion = 0\n\thooksPath = {}\n\tlogAllRefUpdates = true\n\
                     \tfsmonitor = {}\n[filter \"evil\"]\n\tsmudge = {}\n",
                    hooks.display(),
                    hook.display(),
                    hook.display()
                ),
            )
            .unwrap();
            // `logs` replaced by a directory whose HEAD aliases the operator index.
            let _ = std::fs::remove_file(scratch.join("logs"));
            alias_file(&work.join(".git/index"), &scratch.join("logs/HEAD"));
        };
        reset_pinned_worktree_with_env(&pinned, &head(&work), &|_| Vec::new(), &plant).unwrap();
        assert!(planted.load(std::sync::atomic::Ordering::SeqCst));
        assert_files_unchanged(&files, &pinned_state);
        assert!(slot.join("ahead.txt").is_file());
    }

    /// #1160: the fence permits writes beneath the handles it was built from
    /// and nothing else, including through a symlink aimed outside them.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_write_fence_allows_its_handles_and_refuses_every_other_write() {
        let dir = tempfile::tempdir().unwrap();
        let allowed = dir.path().join("allowed");
        let outside = dir.path().join("outside");
        std::fs::create_dir(&allowed).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("keep"), "keep").unwrap();
        std::os::unix::fs::symlink(outside.join("keep"), allowed.join("link")).unwrap();
        let handle = std::fs::File::open(&allowed).unwrap();
        let fence = WriteFence::new(&[&handle], WriteFence::kernel_abi).unwrap();
        let run = |script: &str| {
            let mut command = std::process::Command::new("sh");
            command
                .arg("-c")
                .arg(script)
                .current_dir(dir.path())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            fence.confine_on_exec(&mut command);
            command.status().unwrap().success()
        };
        assert!(run(
            "echo ok > allowed/new && mv allowed/new allowed/moved && rm allowed/moved"
        ));
        assert!(!run("echo no > outside/new"));
        assert!(!run("echo no > allowed/link"));
        assert!(!run("rm outside/keep"));
        assert!(!run("ln -s allowed outside/link"));
        assert_eq!(
            std::fs::read_to_string(outside.join("keep")).unwrap(),
            "keep"
        );
        assert!(!outside.join("new").exists() && !outside.join("link").exists());
    }

    /// #1141: the empty-slot fallback deletes relative to the pinned
    /// `worktrees` handle, so neither a symlinked slot nor a `worktrees`
    /// directory swapped for a symlink to a same-named empty directory
    /// elsewhere can redirect the removal.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_empty_slot_fallback_deletes_fd_relative_and_never_follows_a_swap() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("queue");
        prepare_queue_workspace(&workspace).unwrap();
        let slot = workspace.join("worktrees").join("slot");
        // A real empty slot is retired.
        std::fs::create_dir(&slot).unwrap();
        remove_empty_queue_dir(&workspace, &slot).unwrap();
        assert!(slot.symlink_metadata().is_err());
        // A non-empty slot is refused and kept.
        std::fs::create_dir(&slot).unwrap();
        std::fs::write(slot.join("keep"), "x").unwrap();
        assert!(remove_empty_queue_dir(&workspace, &slot).is_err());
        assert!(slot.join("keep").exists());
        std::fs::remove_file(slot.join("keep")).unwrap();
        std::fs::remove_dir(&slot).unwrap();
        // A slot swapped for a symlink to an empty directory is refused.
        let decoy = dir.path().join("decoy");
        std::fs::create_dir(&decoy).unwrap();
        std::os::unix::fs::symlink(&decoy, &slot).unwrap();
        assert!(remove_empty_queue_dir(&workspace, &slot).is_err());
        assert!(decoy.is_dir());
        std::fs::remove_file(&slot).unwrap();
        // `worktrees` swapped for a symlink to a directory holding an empty
        // same-named slot: the decoy slot survives.
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir_all(elsewhere.join("slot")).unwrap();
        std::fs::remove_dir(workspace.join("worktrees")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, workspace.join("worktrees")).unwrap();
        assert!(remove_empty_queue_dir(&workspace, &slot).is_err());
        assert!(elsewhere.join("slot").is_dir());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_symlinked_workspace_root_is_refused_and_nothing_is_created_through_it() {
        let (dir, _origin, work) = repos();
        let (before, note) = dirty_operator(&work);
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        let workspace = dir.path().join("queue");
        std::os::unix::fs::symlink(&elsewhere, &workspace).unwrap();
        let error = ensure_queue_worktree(&workspace, &work).unwrap_err();
        assert!(error.contains("symlink"), "{error}");
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
        // A symlinked `worktrees` directory is refused the same way.
        std::fs::remove_file(&workspace).unwrap();
        std::fs::create_dir(&workspace).unwrap();
        std::os::unix::fs::symlink(&elsewhere, workspace.join("worktrees")).unwrap();
        assert!(ensure_queue_worktree(&workspace, &work).is_err());
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
        assert_operator_untouched(&work, &before, &note);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_plain_non_worktree_directory_is_refused_and_kept() {
        let (dir, _origin, work) = repos();
        let (before, note) = dirty_operator(&work);
        let workspace = dir.path().join("queue");
        prepare_queue_workspace(&workspace).unwrap();
        let slot = worktree_dir(&workspace, &work);
        std::fs::create_dir(&slot).unwrap();
        std::fs::write(slot.join("precious.txt"), "keep").unwrap();
        let error = ensure_queue_worktree(&workspace, &work).unwrap_err();
        assert!(error.contains("not a registered worktree"), "{error}");
        assert_eq!(
            std::fs::read_to_string(slot.join("precious.txt")).unwrap(),
            "keep"
        );
        // A directory whose `.git` is a copy of the operator's pointer, or a
        // worktree registered elsewhere, is refused too.
        std::fs::write(
            slot.join(".git"),
            format!("gitdir: {}/.git\n", work.display()),
        )
        .unwrap();
        assert!(ensure_queue_worktree(&workspace, &work).is_err());
        assert!(slot.join("precious.txt").is_file());
        assert_operator_untouched(&work, &before, &note);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn an_empty_leftover_directory_is_recovered_into_a_real_worktree() {
        let (dir, _origin, work) = repos();
        let workspace = dir.path().join("queue");
        prepare_queue_workspace(&workspace).unwrap();
        let slot = worktree_dir(&workspace, &work);
        std::fs::create_dir(&slot).unwrap();
        let path = ensure_queue_worktree(&workspace, &work).unwrap();
        assert_eq!(path, slot);
        assert!(slot.join(".git").is_file());
        // And the healthy worktree is reused on the next call.
        assert_eq!(ensure_queue_worktree(&workspace, &work).unwrap(), slot);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_launcher_without_a_workspace_keeps_the_entry_checkout() {
        let (dir, _origin, work) = repos();
        let sha = commit_file(&work, "a", "a", "a");
        let log = dir.path().join("lander.log");
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let launcher = recording_lander(dir.path(), &log, &sha, dir.path());
        let launcher = LanderLauncher::new(launcher.binary().to_path_buf());
        queued(&store, &work, &sha, "k").await;
        run_next(&store, &launcher).await.unwrap().unwrap();
        assert_eq!(
            logged(&log, "cwd"),
            vec![work.canonicalize().unwrap().display().to_string()]
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn run_next_passes_the_source_filters_and_the_one_regate_budget() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("args.log");
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let launcher = fake_lander(
            dir.path(),
            &format!(
                "echo \"$@\" > {log}; echo \"$RSI_LANDER_MAX_REGATED_STALE_RETRIES\" >> {log}; \
                 echo \"exact=$RSI_LANDER_EXACT_GATE\" >> {log}; \
                 echo 'test rolling_queue::red ... FAILED'; exit 1",
                log = log.display()
            ),
        );
        let commit = "b".repeat(40);
        let id = queued(&store, dir.path(), &commit, "k").await;
        let (_, result) = run_next(&store, &launcher).await.unwrap().unwrap();
        assert_eq!(result.state, RollingQueueEntryState::Refused);
        assert_eq!(result.outcome.failing_tests, vec!["rolling_queue::red"]);
        let logged = std::fs::read_to_string(&log).unwrap();
        assert!(logged.contains(&format!("--accepted {commit}")), "{logged}");
        assert!(
            logged.contains("--test-filter rsid=rolling_queue"),
            "{logged}"
        );
        assert!(
            logged.lines().any(|line| line == "1") && logged.trim_end().ends_with("exact=1"),
            "the queue runs the lander with one re-gate and the exact gate: {logged}"
        );
        let entry = store
            .lock()
            .await
            .get_rolling_queue_entry(id)
            .unwrap()
            .unwrap();
        assert_eq!(entry.state, RollingQueueEntryState::Refused);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn an_unavailable_lander_fails_the_entry_and_wakes_the_owner() {
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let launcher = LanderLauncher::new(PathBuf::from("/nonexistent/rsi-rolling-land"));
        let dir = tempfile::tempdir().unwrap();
        queued(&store, dir.path(), &"c".repeat(40), "k").await;
        let (_, result) = run_next(&store, &launcher).await.unwrap().unwrap();
        assert_eq!(result.state, RollingQueueEntryState::Failed);
        assert_eq!(
            result.outcome.refusal.as_deref(),
            Some("lander_unavailable")
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test(flavor = "multi_thread")]
    async fn restart_reconcile_settles_a_landed_entry_once_and_requeues_the_rest() {
        let (_dir, _origin, work) = repos();
        let landed_commit = head(&work);
        sh(
            &work,
            "git checkout -q -b feature && echo x > x && git add x && git commit -qm x",
        );
        let pending_commit = head(&work);
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let landed = queued(&store, &work, &landed_commit, "1").await;
        let pending = queued(&store, &work, &pending_commit, "2").await;
        {
            let guard = store.lock().await;
            // Both were mid-run when the daemon stopped.
            for id in [landed, pending] {
                let batch = Uuid::new_v4().to_string();
                guard
                    .conn
                    .execute(
                        "INSERT INTO rolling_queue_batches(id,state,member_ids_json,member_count,created_at,row_version) \
                         VALUES (?1,'gating','[]',1,'2026-09-29T00:00:00.000000000Z',1)",
                        [&batch],
                    )
                    .unwrap();
                guard
                    .conn
                    .execute(
                        "UPDATE rolling_queue_entries SET state='gating', batch_id=?2 WHERE id=?1",
                        [id.to_string(), batch],
                    )
                    .unwrap();
            }
        }
        assert_eq!(reconcile_gating(&store).await.unwrap(), (1, 1));
        // A second boot finds nothing left to settle: the wake is not repeated.
        assert_eq!(reconcile_gating(&store).await.unwrap(), (0, 0));
        let guard = store.lock().await;
        assert_eq!(
            guard
                .get_rolling_queue_entry(landed)
                .unwrap()
                .unwrap()
                .state,
            RollingQueueEntryState::Published
        );
        assert_eq!(
            guard
                .get_rolling_queue_entry(pending)
                .unwrap()
                .unwrap()
                .state,
            RollingQueueEntryState::Queued
        );
        let wakes = guard
            .list_scheduled_jobs()
            .unwrap()
            .into_iter()
            .filter(|job| job.name.starts_with("merge-queue-"))
            .count();
        assert_eq!(wakes, 1);
    }

    fn member(id: u128, derived: Option<u32>, declared: bool) -> MigrationMember {
        MigrationMember {
            id: Uuid::from_u128(id),
            derived,
            declared,
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn declared_migrations_are_numbered_tip_plus_one_in_queue_order() {
        let plan = plan_migration_order(
            148,
            &[
                member(1, Some(149), true),
                member(2, None, false),
                member(3, Some(149), true),
            ],
        );
        assert_eq!(
            plan.assigned,
            vec![(Uuid::from_u128(1), 149), (Uuid::from_u128(3), 150)]
        );
        assert!(plan.out_of_order.is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_fixed_migration_number_taken_by_a_predecessor_is_out_of_order() {
        let plan = plan_migration_order(
            148,
            &[member(1, Some(149), false), member(2, Some(149), false)],
        );
        assert_eq!(plan.assigned, vec![(Uuid::from_u128(1), 149)]);
        assert_eq!(plan.out_of_order, vec![Uuid::from_u128(2)]);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_batch_gates_the_union_of_filters_and_a_rust_member_without_filters_widens_it() {
        let mut a = queued_entry_view("a", &["rsid=x", "rsi=y"]);
        let b = queued_entry_view("b", &["rsid=x", "rsi-common=z"]);
        assert_eq!(
            union_filters(&[a.clone(), b.clone()], |_| false),
            vec!["rsid=x", "rsi=y", "rsi-common=z"]
        );
        a.test_filters.clear();
        assert!(union_filters(&[a, b], |_| false).is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_rust_free_member_without_filters_does_not_widen_the_batch() {
        let a = queued_entry_view("a", &["rsid=x", "rsi=y"]);
        let script = queued_entry_view("b", &[]);
        let rust = queued_entry_view("c", &[]);
        let script_commit = script.source_commit.clone();
        let rust_free = |entry: &RollingQueueEntryV1| entry.source_commit == script_commit;
        assert_eq!(
            union_filters(&[a.clone(), script.clone()], rust_free),
            vec!["rsid=x", "rsi=y"]
        );
        // A member with Rust changes and no filters still widens the batch.
        assert!(union_filters(&[a, script, rust], rust_free).is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn only_rust_cargo_and_migration_paths_affect_the_gate() {
        for path in [
            "crates/rsid/src/lib.rs",
            "crates/rsid-store/src/store/migrations/v090.rs",
            "crates/rsid/build.rs",
            "Cargo.toml",
            "crates/rsid/Cargo.toml",
            "Cargo.lock",
            "rust-toolchain.toml",
            ".cargo/config.toml",
        ] {
            assert!(path_affects_rust_gate(path), "{path}");
        }
        for path in [
            "scripts/check-touched-shards",
            "docs/agents/routing.md",
            "thoughts/shared/notes/x.md",
            "tools/provisional-migrations/v091.json",
            ".claude/commands/foo.md",
        ] {
            assert!(!path_affects_rust_gate(path), "{path}");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn source_rust_freedom_reads_the_diff_against_rolling() {
        let (_dir, repo) = rust_free_repo();
        let git_in = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "{args:?}");
        };
        let head = |branch: &str| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["rev-parse", branch])
                .output()
                .unwrap();
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };
        git_in(&["checkout", "-q", "-b", "docs", "rolling"]);
        std::fs::create_dir_all(repo.join("scripts")).unwrap();
        std::fs::write(repo.join("scripts/tool.sh"), "echo hi\n").unwrap();
        git_in(&["add", "scripts/tool.sh"]);
        git_in(&["commit", "-q", "-m", "script"]);
        git_in(&["checkout", "-q", "-b", "code", "rolling"]);
        std::fs::write(repo.join("lib.rs"), "fn a() {}\n").unwrap();
        git_in(&["add", "lib.rs"]);
        git_in(&["commit", "-q", "-m", "code"]);
        assert!(source_is_rust_free(&repo, &head("docs")));
        assert!(!source_is_rust_free(&repo, &head("code")));
        // An unknown revision never narrows the gate.
        assert!(!source_is_rust_free(&repo, &"0".repeat(40)));
    }

    fn rust_free_repo() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().to_path_buf();
        for args in [
            vec!["init", "-q", "-b", "rolling"],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "user.name", "t"],
            vec!["config", "commit.gpgsign", "false"],
        ] {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(&args)
                .status()
                .unwrap();
            assert!(status.success());
        }
        std::fs::write(repo.join("README"), "x\n").unwrap();
        for args in [vec!["add", "README"], vec!["commit", "-q", "-m", "base"]] {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(&args)
                .status()
                .unwrap();
            assert!(status.success());
        }
        (dir, repo)
    }

    fn queued_entry_view(commit: &str, filters: &[&str]) -> RollingQueueEntryV1 {
        RollingQueueEntryV1 {
            sequence: 1,
            id: Uuid::new_v4(),
            source_commit: commit.repeat(40),
            source_session_id: Uuid::new_v4(),
            owner_epic_id: None,
            binding: RollingQueueBinding::Unbound,
            work_key: None,
            migration_version: None,
            hot_files: vec![],
            test_filters: filters.iter().map(|f| (*f).to_string()).collect(),
            state: RollingQueueEntryState::Queued,
            outcome: None,
            enqueued_at: String::new(),
            finished_at: None,
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_merge_conflict_is_typed_and_not_a_red_gate() {
        let result = classify_run(
            Some(1),
            "publication_status=not_published\n",
            "rsi-rolling-land: integration refused: Conflict { paths: [\"f\"] }",
            || None,
        );
        assert_eq!(result.state, RollingQueueEntryState::Refused);
        assert_eq!(
            result.outcome.refusal.as_deref(),
            Some(QUEUE_BATCH_MERGE_CONFLICT)
        );
        let policy = as_batch_result(classify_run(
            Some(8),
            "policy_fence=migration_number\n",
            "refused",
            || None,
        ));
        assert_eq!(
            policy.outcome.refusal.as_deref(),
            Some(QUEUE_BATCH_POLICY_REFUSED)
        );
        assert!(
            policy
                .outcome
                .detail
                .unwrap()
                .contains("policy_fence=migration_number")
        );
    }

    /// A fake lander that really merges its `--accepted` sources onto
    /// `origin/rolling` in `work` and pushes once, like the real one on green.
    /// `guard` is shell run first (it sees `"$@"`); `@SKIP@` names a source the
    /// lander reports landed without merging.
    fn merging_lander(dir: &Path, work: &Path, log: &Path, guard: &str) -> LanderLauncher {
        let body = r#"
echo "run $*" >> @LOG@
echo "regates $RSI_LANDER_MAX_REGATED_STALE_RETRIES" >> @LOG@
@GUARD@
cd @WORK@ || exit 9
git fetch -q origin rolling
git checkout -q -B land origin/rolling
fetched=$(git rev-parse HEAD)
while [ $# -gt 0 ]; do
  if [ "$1" = --accepted ]; then
    if [ "$2" != "$SKIP" ]; then
      git merge -q --no-edit "$2" >/dev/null 2>&1 || { git merge --abort; echo "integration refused: Conflict { paths: [\"f\"] }; conflict_source=$2" >&2; echo publication_status=not_published; exit 1; }
    fi
    shift
  fi
  shift
done
git push -q origin HEAD:rolling && echo push >> @LOG@
echo publication_status=published
echo fetched_target_id=$fetched
echo candidate_id=$(git rev-parse HEAD)
echo published_target_id=$(git rev-parse HEAD)
"#
        .replace("@LOG@", &log.display().to_string())
        .replace("@WORK@", &work.display().to_string())
        .replace("@GUARD@", guard);
        fake_lander(dir, &body)
    }

    fn commit_file(work: &Path, branch: &str, file: &str, content: &str) -> String {
        sh(
            work,
            &format!(
                "git checkout -q -B {branch} origin/rolling && printf '{content}\\n' > {file} \
                 && git add {file} && git commit -qm {branch}"
            ),
        );
        head(work)
    }

    async fn queue_all(
        store: &Arc<tokio::sync::Mutex<Store>>,
        work: &Path,
        shas: &[String],
        migration: Option<u32>,
    ) -> Vec<Uuid> {
        let mut ids = Vec::new();
        for (index, sha) in shas.iter().enumerate() {
            let id = queued(store, work, sha, &format!("k{index}")).await;
            if let Some(version) = migration {
                store
                    .lock()
                    .await
                    .conn
                    .execute(
                        "UPDATE rolling_queue_entries SET migration_version=?2 WHERE id=?1",
                        rusqlite::params![id.to_string(), version],
                    )
                    .unwrap();
            }
            ids.push(id);
        }
        ids
    }

    async fn entry_of(store: &Arc<tokio::sync::Mutex<Store>>, id: Uuid) -> RollingQueueEntryV1 {
        store
            .lock()
            .await
            .get_rolling_queue_entry(id)
            .unwrap()
            .unwrap()
    }

    fn log_lines(log: &Path) -> Vec<String> {
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn is_ancestor_of_origin(work: &Path, sha: &str) -> bool {
        let _ = git(work, &["fetch", "--quiet", "origin", "rolling"]);
        git(
            work,
            &["merge-base", "--is-ancestor", sha, "origin/rolling"],
        )
        .is_some()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_green_batch_publishes_every_source_with_one_push_and_verifies_ancestry() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b", "c", "d"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        let launcher = merging_lander(dir.path(), &work, &log, "");
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, &work, &shas, None).await;
        let results = run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_eq!(results.len(), 4);
        // FIFO: settled in enqueue order.
        assert_eq!(results.iter().map(|r| r.0).collect::<Vec<_>>(), ids);
        assert!(
            results
                .iter()
                .all(|(_, r)| r.state == RollingQueueEntryState::Published)
        );
        let lines = log_lines(&log);
        assert_eq!(
            lines.iter().filter(|l| l.starts_with("run ")).count(),
            1,
            "one lander run for the whole batch: {lines:?}"
        );
        assert_eq!(lines.iter().filter(|l| *l == "push").count(), 1);
        let run = &lines[0];
        for sha in &shas {
            assert!(run.contains(&format!("--accepted {sha}")), "{run}");
            assert!(is_ancestor_of_origin(&work, sha));
        }
        // The batch is one row; every owner is woken once.
        let guard = store.lock().await;
        let (state, members): (String, i64) = guard
            .conn
            .query_row(
                "SELECT state, member_count FROM rolling_queue_batches",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((state.as_str(), members), ("published", 4));
        let wakes = guard
            .list_scheduled_jobs()
            .unwrap()
            .into_iter()
            .filter(|job| job.name.starts_with("merge-queue-"))
            .count();
        assert_eq!(wakes, 4);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_green_batch_from_one_owner_wakes_that_owner_once_naming_every_entry() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b", "c"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        let launcher = merging_lander(dir.path(), &work, &log, "");
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let owner = Uuid::new_v4();
        let mut ids = Vec::new();
        for (index, sha) in shas.iter().enumerate() {
            ids.push(queued_by(&store, &work, sha, &format!("k{index}"), owner).await);
        }
        let results = run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_eq!(results.len(), 3);
        let wakes: Vec<_> = store
            .lock()
            .await
            .list_scheduled_jobs()
            .unwrap()
            .into_iter()
            .filter(|job| job.name.starts_with("merge-queue-"))
            .collect();
        assert_eq!(wakes.len(), 1, "one owner, one batch, one wake");
        assert_eq!(wakes[0].wake_session_id, Some(owner));
        for (sha, id) in shas.iter().zip(&ids) {
            assert!(wakes[0].message.contains(sha), "{}", wakes[0].message);
            assert!(wakes[0].message.contains(&id.to_string()));
            assert_eq!(
                entry_of(&store, *id).await.state,
                RollingQueueEntryState::Published
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_reported_source_missing_from_the_published_tip_fails_alone() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b", "c"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        let guard = format!("export SKIP={}", shas[1]);
        let launcher = merging_lander(dir.path(), &work, &log, &guard);
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, &work, &shas, None).await;
        run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_eq!(
            entry_of(&store, ids[0]).await.state,
            RollingQueueEntryState::Published
        );
        let lost = entry_of(&store, ids[1]).await;
        assert_eq!(lost.state, RollingQueueEntryState::Failed);
        assert_eq!(
            lost.outcome.unwrap().refusal.as_deref(),
            Some(QUEUE_BATCH_ANCESTRY_UNVERIFIED)
        );
        assert_eq!(
            entry_of(&store, ids[2]).await.state,
            RollingQueueEntryState::Published
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_merge_conflict_refuses_only_the_conflicting_entry() {
        let (dir, _origin, work) = repos();
        let a = commit_file(&work, "a", "shared", "one");
        let b = commit_file(&work, "b", "shared", "two");
        let c = commit_file(&work, "c", "other", "three");
        let shas = vec![a.clone(), b.clone(), c.clone()];
        let log = dir.path().join("lander.log");
        let launcher = merging_lander(dir.path(), &work, &log, "");
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, &work, &shas, None).await;
        run_next_batch(&store, &launcher, 4).await.unwrap();
        // The conflicting middle entry is refused; the batch retries without it
        // and publishes the other two with one push.
        assert_eq!(lander_runs(&log).len(), 2);
        assert_eq!(log_lines(&log).iter().filter(|l| *l == "push").count(), 1);
        let retry = lander_runs(&log).pop().unwrap();
        assert!(retry.contains(&a) && retry.contains(&c) && !retry.contains(&b));
        for id in [ids[0], ids[2]] {
            assert_eq!(
                entry_of(&store, id).await.state,
                RollingQueueEntryState::Published
            );
        }
        let refused = entry_of(&store, ids[1]).await;
        assert_eq!(refused.state, RollingQueueEntryState::Refused);
        let outcome = refused.outcome.unwrap();
        assert_eq!(outcome.refusal.as_deref(), Some(QUEUE_BATCH_MERGE_CONFLICT));
        assert!(outcome.detail.unwrap().contains("paths: [\"f\"]"));
        assert!(is_ancestor_of_origin(&work, &a));
        assert!(!is_ancestor_of_origin(&work, &b));
        assert!(is_ancestor_of_origin(&work, &c));
        assert_eq!(wake_count(&store).await, 3);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_lander_names_the_conflicting_source() {
        let marked = mark_conflict_source(
            "integration refused: Conflict { paths: [\"f\"] }".to_string(),
            "abc123",
        );
        assert_eq!(conflict_source(&marked).as_deref(), Some("abc123"));
        let other = mark_conflict_source("gate failed".to_string(), "abc123");
        assert_eq!(other, "gate failed");
        assert_eq!(conflict_source(&other), None);
    }

    /// Guard shell for the merging fake lander: any run whose `--accepted`
    /// list contains a `bad` source prints that source's failing test and
    /// exits red, like a red gate.
    fn red_when_any(bad: &[(&str, &str)]) -> String {
        let mut guard = String::from("red=0\n");
        for (sha, test) in bad {
            guard.push_str(&format!(
                "case \" $* \" in *\" {sha} \"*) echo 'test {test} ... FAILED' >&2; red=1;; esac\n"
            ));
        }
        guard.push_str("if [ $red = 1 ]; then echo publication_status=not_published; exit 1; fi");
        guard
    }

    fn lander_runs(log: &Path) -> Vec<String> {
        log_lines(log)
            .into_iter()
            .filter(|l| l.starts_with("run "))
            .collect()
    }

    async fn wake_count(store: &Arc<tokio::sync::Mutex<Store>>) -> usize {
        store
            .lock()
            .await
            .list_scheduled_jobs()
            .unwrap()
            .into_iter()
            .filter(|job| job.name.starts_with("merge-queue-"))
            .count()
    }

    /// A batch of `count` sources where `bad` (0-based position -> failing
    /// test) are red. Returns the shas, ids, runs and the work tree.
    async fn bisected(
        count: usize,
        bad: &[(usize, &str)],
    ) -> (
        Arc<tokio::sync::Mutex<Store>>,
        Vec<String>,
        Vec<Uuid>,
        Vec<String>,
        PathBuf,
        tempfile::TempDir,
    ) {
        let (dir, _origin, work) = repos();
        let names = ["a", "b", "c", "d", "e", "f", "g", "h"];
        let shas: Vec<String> = names[..count]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        let bad_shas: Vec<(&str, &str)> = bad
            .iter()
            .map(|(index, test)| (shas[*index].as_str(), *test))
            .collect();
        let launcher = merging_lander(dir.path(), &work, &log, &red_when_any(&bad_shas));
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, &work, &shas, None).await;
        let results = run_next_batch(&store, &launcher, count).await.unwrap();
        assert_eq!(results.len(), count, "every member settles exactly once");
        let runs = lander_runs(&log);
        (store, shas, ids, runs, work, dir)
    }

    async fn assert_isolated_red(store: &Arc<tokio::sync::Mutex<Store>>, id: Uuid, test: &str) {
        let entry = entry_of(store, id).await;
        assert_eq!(entry.state, RollingQueueEntryState::Refused);
        let outcome = entry.outcome.unwrap();
        assert_eq!(outcome.refusal.as_deref(), Some("gate_failed"));
        assert_eq!(outcome.failing_tests, vec![test.to_string()]);
        assert_eq!(outcome.landed_sha, None);
    }

    async fn assert_landed(
        store: &Arc<tokio::sync::Mutex<Store>>,
        work: &Path,
        id: Uuid,
        sha: &str,
    ) {
        let entry = entry_of(store, id).await;
        assert_eq!(entry.state, RollingQueueEntryState::Published);
        let landed = entry.outcome.unwrap().landed_sha.unwrap();
        assert_eq!(landed.len(), 40);
        assert!(is_ancestor_of_origin(work, sha));
    }

    fn events_of(store: &Store, kind: &str) -> Vec<serde_json::Value> {
        store
            .conn
            .prepare("SELECT detail_json FROM rolling_queue_events WHERE kind=?1 ORDER BY id")
            .unwrap()
            .query_map([kind], |row| row.get::<_, Option<String>>(0))
            .unwrap()
            .map(|detail| serde_json::from_str(&detail.unwrap().unwrap()).unwrap())
            .collect()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_red_gate_event_names_the_failing_tests_and_a_log_that_exists() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b", "c"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        let launcher = merging_lander(
            dir.path(),
            &work,
            &log,
            &red_when_any(&[(shas[1].as_str(), "q::red_b")]),
        )
        .with_workspace(dir.path().join("queue"));
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        queue_all(&store, &work, &shas, None).await;
        run_next_batch(&store, &launcher, 3).await.unwrap();
        let guard = store.lock().await;
        let mut details = events_of(&guard, "bisect_started");
        details.extend(events_of(&guard, "bisect_gate"));
        let red: Vec<&serde_json::Value> = details
            .iter()
            .filter(|detail| detail["outcome"] != "green")
            .collect();
        assert!(!red.is_empty(), "{details:?}");
        for detail in red {
            let cause = detail["cause"].as_str().unwrap();
            assert!(cause.contains("gate_failed"), "{cause}");
            assert!(cause.contains("failing_tests=q::red_b"), "{cause}");
            let path = detail["log"].as_str().unwrap();
            let text = std::fs::read_to_string(path).unwrap();
            assert!(text.contains("q::red_b ... FAILED"), "{text}");
        }
        // The settled entry points at a log too.
        let red_entry = guard
            .conn
            .query_row(
                "SELECT outcome_json FROM rolling_queue_entries WHERE state='refused'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        assert!(red_entry.contains("lander_log="), "{red_entry}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_conflict_refusal_event_names_the_conflicting_path() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let body = format!(
            "case \" $* \" in *\" {} \"*) echo 'integration refused: Conflict {{ paths: [\"src/x.rs\", \"y.rs\"] }}; conflict_source={}' >&2; exit 1;; esac\nexit 1",
            shas[0], shas[0]
        );
        let launcher = fake_lander(dir.path(), &body).with_workspace(dir.path().join("queue"));
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        queue_all(&store, &work, &shas, None).await;
        run_next_batch(&store, &launcher, 2).await.unwrap();
        let guard = store.lock().await;
        let events = events_of(&guard, "conflict_isolated");
        assert_eq!(events.len(), 1, "{events:?}");
        let cause = events[0]["cause"].as_str().unwrap();
        assert!(cause.contains("conflict_paths=src/x.rs,y.rs"), "{cause}");
        assert!(std::path::Path::new(events[0]["log"].as_str().unwrap()).exists());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_run_log_directory_keeps_only_the_newest_bounded_files() {
        let dir = tempfile::tempdir().unwrap();
        let big = "x".repeat(RUN_LOG_STREAM_BYTES + 4096);
        let batch = Uuid::new_v4();
        let mut last = None;
        for _ in 0..(RUN_LOG_KEEP_FILES + 7) {
            last = write_run_log(dir.path(), batch, "hdr", &big, "err");
        }
        let count = std::fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(count, RUN_LOG_KEEP_FILES);
        let last = last.unwrap();
        assert!(last.exists(), "the newest log survives rotation");
        let size = std::fs::metadata(&last).unwrap().len() as usize;
        assert!(size < RUN_LOG_STREAM_BYTES + 512, "{size}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_bad_first_source_is_isolated_in_two_extra_gates_and_the_innocents_publish() {
        let (store, shas, ids, runs, work, _dir) = bisected(4, &[(0, "q::red_a")]).await;
        // batch (red), [a,b] (red), [a] (red: isolated), then [b,c,d] (green).
        assert_eq!(runs.len(), 4, "{runs:?}");
        let counts: Vec<usize> = runs
            .iter()
            .map(|r| r.matches("--accepted").count())
            .collect();
        assert_eq!(counts, vec![4, 2, 1, 3]);
        assert_isolated_red(&store, ids[0], "q::red_a").await;
        for index in 1..4 {
            assert_landed(&store, &work, ids[index], &shas[index]).await;
        }
        assert!(!is_ancestor_of_origin(&work, &shas[0]));
        assert_eq!(wake_count(&store).await, 4);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_bad_last_source_is_isolated_in_two_extra_gates_and_the_innocents_publish() {
        let (store, shas, ids, runs, work, _dir) = bisected(4, &[(3, "q::red_d")]).await;
        // batch (red), [a,b] (green, published), [c] (green, published), d isolated.
        let counts: Vec<usize> = runs
            .iter()
            .map(|r| r.matches("--accepted").count())
            .collect();
        assert_eq!(counts, vec![4, 2, 1], "{runs:?}");
        assert_isolated_red(&store, ids[3], "q::red_d").await;
        for index in 0..3 {
            assert_landed(&store, &work, ids[index], &shas[index]).await;
        }
        assert!(!is_ancestor_of_origin(&work, &shas[3]));
        assert_eq!(wake_count(&store).await, 4);
        // The bisect trail is on the batch, in order.
        let guard = store.lock().await;
        let kinds: Vec<String> = guard
            .conn
            .prepare("SELECT kind FROM rolling_queue_events ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|kind| kind.unwrap())
            .collect();
        let trail: Vec<&str> = kinds
            .iter()
            .map(String::as_str)
            .filter(|kind| kind.starts_with("bisect"))
            .collect();
        assert_eq!(
            trail,
            vec![
                "bisect_started",
                "bisect_gate",
                "bisect_gate",
                "bisect_isolated"
            ]
        );
        let state: String = guard
            .conn
            .query_row("SELECT state FROM rolling_queue_batches", [], |r| r.get(0))
            .unwrap();
        assert_eq!(state, "published");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn two_bad_sources_in_a_batch_of_four_both_end_red_with_their_own_failing_tests() {
        let (store, shas, ids, _runs, work, _dir) =
            bisected(4, &[(0, "q::red_a"), (2, "q::red_c")]).await;
        assert_isolated_red(&store, ids[0], "q::red_a").await;
        assert_isolated_red(&store, ids[2], "q::red_c").await;
        assert_landed(&store, &work, ids[1], &shas[1]).await;
        assert_landed(&store, &work, ids[3], &shas[3]).await;
        assert!(!is_ancestor_of_origin(&work, &shas[0]));
        assert!(!is_ancestor_of_origin(&work, &shas[2]));
        assert_eq!(wake_count(&store).await, 4);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn an_all_red_batch_settles_every_source_red_and_never_loops() {
        let (store, _shas, ids, runs, _work, _dir) =
            bisected(3, &[(0, "q::r0"), (1, "q::r1"), (2, "q::r2")]).await;
        assert!(runs.len() <= 3 * 4 + 4, "{runs:?}");
        for (index, id) in ids.iter().enumerate() {
            assert_isolated_red(&store, *id, &format!("q::r{index}")).await;
        }
        assert_eq!(wake_count(&store).await, 3);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_second_settle_of_a_bisected_entry_writes_nothing_and_wakes_nobody() {
        let (store, _shas, ids, _runs, _work, _dir) = bisected(4, &[(3, "q::red_d")]).await;
        assert_eq!(wake_count(&store).await, 4);
        let again = RunResult {
            state: RollingQueueEntryState::Refused,
            outcome: RollingQueueOutcome {
                refusal: Some("gate_failed".into()),
                ..RollingQueueOutcome::default()
            },
        };
        for id in &ids {
            settle(&store, *id, &again).await.unwrap();
        }
        assert_eq!(wake_count(&store).await, 4);
        // The first outcome is untouched: the published entries stay published.
        assert_eq!(
            entry_of(&store, ids[0]).await.state,
            RollingQueueEntryState::Published
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_single_source_the_lander_reports_landed_but_the_tip_lacks_fails_typed() {
        let (dir, _origin, work) = repos();
        let sha = commit_file(&work, "a", "a", "a");
        let log = dir.path().join("lander.log");
        let launcher = merging_lander(dir.path(), &work, &log, &format!("export SKIP={sha}"));
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let id = queued(&store, &work, &sha, "k").await;
        run_next(&store, &launcher).await.unwrap().unwrap();
        let entry = entry_of(&store, id).await;
        assert_eq!(entry.state, RollingQueueEntryState::Failed);
        assert_eq!(
            entry.outcome.unwrap().refusal.as_deref(),
            Some(QUEUE_BATCH_ANCESTRY_UNVERIFIED)
        );
        assert_eq!(wake_count(&store).await, 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_bisect_segment_is_ancestry_verified_before_it_settles_published() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b", "c", "d"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        // d is red; the lander lies about b in every published segment.
        let guard = format!(
            "export SKIP={}\n{}",
            shas[1],
            red_when_any(&[(shas[3].as_str(), "q::red_d")])
        );
        let launcher = merging_lander(dir.path(), &work, &log, &guard);
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, &work, &shas, None).await;
        run_next_batch(&store, &launcher, 4).await.unwrap();
        let lost = entry_of(&store, ids[1]).await;
        assert_eq!(lost.state, RollingQueueEntryState::Failed);
        assert_eq!(
            lost.outcome.unwrap().refusal.as_deref(),
            Some(QUEUE_BATCH_ANCESTRY_UNVERIFIED)
        );
        for index in [0, 2] {
            assert_landed(&store, &work, ids[index], &shas[index]).await;
        }
        assert_isolated_red(&store, ids[3], "q::red_d").await;
        assert_eq!(wake_count(&store).await, 4);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_published_segment_records_the_lander_final_migration_number() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        // a is red; b is renumbered to 903 by the lander once a is out.
        let guard = format!(
            "echo migration_0_source_id={b}\necho migration_0_final_version=903\n{red}",
            b = shas[1],
            red = red_when_any(&[(shas[0].as_str(), "q::red_a")])
        );
        let launcher = merging_lander(dir.path(), &work, &log, &guard);
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, &work, &shas, None).await;
        run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_isolated_red(&store, ids[0], "q::red_a").await;
        let published = entry_of(&store, ids[1]).await;
        assert_eq!(published.state, RollingQueueEntryState::Published);
        assert_eq!(published.migration_version, Some(903));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn the_out_of_band_regate_budget_is_spent_once_across_the_whole_bisect() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b", "c", "d"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        // The tip keeps moving: every run reports one re-gated stale retry.
        let guard = format!(
            "echo stale_retry_1=aa..bb:regated\n{}",
            red_when_any(&[(shas[3].as_str(), "q::red_d")])
        );
        let launcher = merging_lander(dir.path(), &work, &log, &guard);
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        queue_all(&store, &work, &shas, None).await;
        run_next_batch(&store, &launcher, 4).await.unwrap();
        let budgets: Vec<String> = log_lines(&log)
            .into_iter()
            .filter_map(|line| line.strip_prefix("regates ").map(str::to_string))
            .collect();
        assert!(budgets.len() >= 3, "{budgets:?}");
        assert_eq!(budgets[0], "1");
        assert!(
            budgets[1..].iter().all(|budget| budget == "0"),
            "the batch has one re-gate in total: {budgets:?}"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test(flavor = "multi_thread")]
    async fn restart_in_the_middle_of_a_bisect_settles_each_entry_once_without_a_second_push() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b", "c", "d"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, &work, &shas, None).await;
        let claimed = store
            .lock()
            .await
            .claim_rolling_queue_batch(Utc::now(), 4)
            .unwrap()
            .unwrap();
        store
            .lock()
            .await
            .begin_rolling_queue_bisect(claimed.batch_id, "gate_failed", Utc::now())
            .unwrap();
        // The bisect had published [a,b] and settled a; the daemon died before
        // it could settle b, and c/d were still being bisected.
        sh(
            &work,
            &format!(
                "git checkout -q -B land origin/rolling && git merge -q --no-edit {} {} \
                 && git push -q origin HEAD:rolling",
                shas[0], shas[1]
            ),
        );
        let landed = RunResult {
            state: RollingQueueEntryState::Published,
            outcome: RollingQueueOutcome {
                landed_sha: Some(head(&work)),
                ..RollingQueueOutcome::default()
            },
        };
        settle(&store, ids[0], &landed).await.unwrap();
        assert_eq!(wake_count(&store).await, 1);
        // Boot: b landed (settled once), c and d go back to the queue.
        assert_eq!(reconcile_gating(&store).await.unwrap(), (1, 2));
        assert_eq!(reconcile_gating(&store).await.unwrap(), (0, 0));
        assert_eq!(wake_count(&store).await, 2);
        for id in &ids[..2] {
            assert_eq!(
                entry_of(&store, *id).await.state,
                RollingQueueEntryState::Published
            );
        }
        for id in &ids[2..] {
            assert_eq!(
                entry_of(&store, *id).await.state,
                RollingQueueEntryState::Queued
            );
        }
        // Re-driven: c and d publish once each, nothing republished.
        let log = dir.path().join("lander.log");
        let launcher = merging_lander(dir.path(), &work, &log, "");
        let results = run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(lander_runs(&log).len(), 1);
        for sha in &shas {
            assert!(is_ancestor_of_origin(&work, sha));
        }
        assert_eq!(wake_count(&store).await, 4);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_lander_policy_refusal_propagates_as_the_batch_policy_code() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        let launcher = merging_lander(
            dir.path(),
            &work,
            &log,
            "echo policy_fence=migration_number; echo 'migration must be tip + 1' >&2; exit 8",
        );
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, &work, &shas, None).await;
        run_next_batch(&store, &launcher, 4).await.unwrap();
        for id in ids {
            let entry = entry_of(&store, id).await;
            assert_eq!(entry.state, RollingQueueEntryState::Refused);
            let outcome = entry.outcome.unwrap();
            assert_eq!(outcome.refusal.as_deref(), Some(QUEUE_BATCH_POLICY_REFUSED));
            assert!(
                outcome
                    .detail
                    .unwrap()
                    .contains("policy_fence=migration_number")
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn the_batch_size_caps_the_claim_and_later_entries_run_next() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b", "c"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        let launcher = merging_lander(dir.path(), &work, &log, "");
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, &work, &shas, None).await;
        let first = run_next_batch(&store, &launcher, 2).await.unwrap();
        assert_eq!(first.iter().map(|r| r.0).collect::<Vec<_>>(), ids[..2]);
        assert_eq!(
            entry_of(&store, ids[2]).await.state,
            RollingQueueEntryState::Queued
        );
        let second = run_next_batch(&store, &launcher, 2).await.unwrap();
        assert_eq!(second.iter().map(|r| r.0).collect::<Vec<_>>(), ids[2..]);
        assert!(
            run_next_batch(&store, &launcher, 2)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_second_source_claiming_a_taken_migration_number_is_refused_in_order() {
        let (dir, _origin, work) = repos();
        // Both sources add migration 901 on top of tip 900, neither declares
        // a provisional migration: only the first can take 901.
        let shas: Vec<String> = ["a", "b"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        let launcher = merging_lander(dir.path(), &work, &log, "");
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, &work, &shas, Some(901)).await;
        run_next_batch(&store, &launcher, 4).await.unwrap();
        let first = entry_of(&store, ids[0]).await;
        assert_eq!(first.state, RollingQueueEntryState::Published);
        assert_eq!(first.migration_version, Some(901));
        let second = entry_of(&store, ids[1]).await;
        assert_eq!(second.state, RollingQueueEntryState::Refused);
        assert_eq!(
            second.outcome.unwrap().refusal.as_deref(),
            Some(QUEUE_MIGRATION_OUT_OF_ORDER)
        );
        let runs = log_lines(&log)
            .into_iter()
            .filter(|l| l.starts_with("run "))
            .collect::<Vec<_>>();
        assert_eq!(runs.len(), 1);
        assert!(runs[0].contains(&shas[0]) && !runs[0].contains(&shas[1]));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test(flavor = "multi_thread")]
    async fn restart_reconcile_settles_every_member_of_a_landed_batch_once() {
        let (_dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        sh(
            &work,
            &format!(
                "git checkout -q -B land origin/rolling && git merge -q --no-edit {} {} \
                 && git push -q origin HEAD:rolling",
                shas[0], shas[1]
            ),
        );
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, &work, &shas, None).await;
        // The daemon claimed the batch, the lander published, then the daemon died.
        store
            .lock()
            .await
            .claim_rolling_queue_batch(Utc::now(), 4)
            .unwrap()
            .unwrap();
        assert_eq!(reconcile_gating(&store).await.unwrap(), (2, 0));
        assert_eq!(reconcile_gating(&store).await.unwrap(), (0, 0));
        for id in ids {
            assert_eq!(
                entry_of(&store, id).await.state,
                RollingQueueEntryState::Published
            );
        }
        let wakes = store
            .lock()
            .await
            .list_scheduled_jobs()
            .unwrap()
            .into_iter()
            .filter(|job| job.name.starts_with("merge-queue-"))
            .count();
        assert_eq!(wakes, 2);
    }

    const EXTERNAL_PUSH: &str = "ext() { ( cd @WORKDIR@ && git fetch -q origin rolling \
        && git checkout -q -B ext origin/rolling && printf x > \"$1\" && git add \"$1\" \
        && git commit -qm \"$1\" && git push -q origin HEAD:rolling ) >/dev/null 2>&1; }\n\
        n=$(grep -c '^run ' @LOG@)\ngit fetch -q origin rolling\nred=0\n";

    fn external_push_guard(work: &Path, log: &Path, rest: &str) -> String {
        format!(
            "{}{rest}",
            EXTERNAL_PUSH
                .replace("@WORKDIR@", &work.display().to_string())
                .replace("@LOG@", &log.display().to_string())
        )
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_lander_receipt_failing_test_lines_beat_the_raw_libtest_lines() {
        let stdout = "publication_status=not_published\nfailing_test=q::a\nfailing_test=q::b\n";
        let stderr = "rsi-rolling-land: new test failures relative to rolling abc: q::a [new], q::b [new]; base reds: ";
        let result = classify_run(Some(1), stdout, stderr, || None);
        assert_eq!(result.state, RollingQueueEntryState::Refused);
        assert_eq!(result.outcome.failing_tests, vec!["q::a", "q::b"]);
        // Raw libtest lines still work when no receipt line exists.
        let raw = classify_run(Some(1), "", "test q::c ... FAILED", || None);
        assert_eq!(raw.outcome.failing_tests, vec!["q::c"]);
        // A receipt line is the authority even when raw lines are present.
        let both = classify_run(
            Some(1),
            "failing_test=q::d\n",
            "test q::e ... FAILED",
            || None,
        );
        assert_eq!(both.outcome.failing_tests, vec!["q::d"]);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_missing_source_with_migration_evidence_never_settles_published() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        // The lander reports b landed with migration evidence but never merged it.
        let guard = format!(
            "export SKIP={b}\necho migration_0_source_id={b}\necho migration_0_final_version=901\n",
            b = shas[1]
        );
        let launcher = merging_lander(dir.path(), &work, &log, &guard);
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, &work, &shas, None).await;
        run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_landed(&store, &work, ids[0], &shas[0]).await;
        let lost = entry_of(&store, ids[1]).await;
        assert_eq!(lost.state, RollingQueueEntryState::Failed);
        assert_eq!(
            lost.outcome.unwrap().refusal.as_deref(),
            Some(QUEUE_BATCH_ANCESTRY_UNVERIFIED)
        );
        assert_eq!(wake_count(&store).await, 2);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn an_external_fix_before_a_green_prefix_regates_the_now_green_remainder() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        // b is red until an external commit adds `fix`; the fix lands while the
        // [a] gate runs, so the [a,b] red is stale by the time b is next.
        let guard = external_push_guard(
            &work,
            &log,
            &format!(
                "if ! git cat-file -e origin/rolling:fix 2>/dev/null; then \
                 case \" $* \" in *\" {b} \"*) echo 'test q::b ... FAILED' >&2; red=1;; esac; fi\n\
                 if [ $n = 2 ]; then ext fix; fi\n\
                 if [ $red = 1 ]; then echo publication_status=not_published; exit 1; fi",
                b = shas[1]
            ),
        );
        let launcher = merging_lander(dir.path(), &work, &log, &guard);
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, &work, &shas, None).await;
        run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_landed(&store, &work, ids[0], &shas[0]).await;
        assert_landed(&store, &work, ids[1], &shas[1]).await;
        assert_eq!(lander_runs(&log).len(), 3, "batch, [a], then b re-gated");
        assert_eq!(wake_count(&store).await, 2);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn two_external_advances_between_runs_spend_one_regate_then_settle_exhausted() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b", "c", "d"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        // Every run is red and someone else pushes right after it, between runs.
        let guard = external_push_guard(
            &work,
            &log,
            &format!(
                "case \" $* \" in *\" {a} \"*|*\" {d} \"*) echo 'test q::x ... FAILED' >&2; red=1;; esac\n\
                 if [ $red = 1 ]; then echo fetched_target_id=$(git rev-parse origin/rolling); \
                 ext e$n; echo publication_status=not_published; exit 1; fi",
                a = shas[0],
                d = shas[3]
            ),
        );
        let launcher = merging_lander(dir.path(), &work, &log, &guard);
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, &work, &shas, None).await;
        let results = run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_eq!(results.len(), 4);
        assert_eq!(lander_runs(&log).len(), 2, "no unbounded loop");
        let budgets: Vec<String> = log_lines(&log)
            .into_iter()
            .filter_map(|line| line.strip_prefix("regates ").map(str::to_string))
            .collect();
        assert_eq!(budgets, vec!["1", "0"]);
        for id in ids {
            let entry = entry_of(&store, id).await;
            assert_eq!(entry.state, RollingQueueEntryState::Refused);
            assert_eq!(
                entry.outcome.unwrap().refusal.as_deref(),
                Some(QUEUE_REGATE_EXHAUSTED)
            );
        }
        assert_eq!(wake_count(&store).await, 4);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn an_interaction_only_red_ends_typed_and_never_publishes_a_red_tip() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        // a and b each pass alone; together they fail.
        let guard = format!(
            "red=0\ncase \" $* \" in *\" {a} \"*) case \" $* \" in *\" {b} \"*) \
             echo 'test q::interact ... FAILED' >&2; red=1;; esac;; esac\n\
             if [ $red = 1 ]; then echo publication_status=not_published; exit 1; fi",
            a = shas[0],
            b = shas[1]
        );
        let launcher = merging_lander(dir.path(), &work, &log, &guard);
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, &work, &shas, None).await;
        let results = run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_eq!(results.len(), 2);
        assert_landed(&store, &work, ids[0], &shas[0]).await;
        assert_isolated_red(&store, ids[1], "q::interact").await;
        assert!(!is_ancestor_of_origin(&work, &shas[1]));
        let pushes = log_lines(&log).iter().filter(|l| *l == "push").count();
        assert_eq!(pushes, 1, "only the green prefix was ever pushed");
        assert_eq!(wake_count(&store).await, 2);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn restart_after_a_bisected_member_published_restores_its_planned_migration() {
        let (dir, _origin, work) = repos();
        let a = commit_file(&work, "a", "a", "a");
        // b declares a provisional migration: the lander renumbers it to tip + 1
        // once a is out of its group.
        sh(
            &work,
            "git checkout -q -B b origin/rolling && mkdir -p tools/provisional-migrations \
             && printf '{}\\n' > tools/provisional-migrations/b.json && printf b > b \
             && git add -A && git commit -qm b",
        );
        let b = head(&work);
        let _ = dir;
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, &work, &[a.clone(), b.clone()], None).await;
        store
            .lock()
            .await
            .conn
            .execute(
                "UPDATE rolling_queue_entries SET migration_version=905 WHERE id=?1",
                rusqlite::params![ids[1].to_string()],
            )
            .unwrap();
        let claimed = store
            .lock()
            .await
            .claim_rolling_queue_batch(Utc::now(), 4)
            .unwrap()
            .unwrap();
        store
            .lock()
            .await
            .begin_rolling_queue_bisect(claimed.batch_id, "gate_failed", Utc::now())
            .unwrap();
        // The bisect is about to run [b] alone; the plan is durable first.
        let group: Vec<RollingQueueEntryV1> = vec![entry_of(&store, ids[1]).await];
        record_migration_plan(&store, &work, claimed.batch_id, &group)
            .await
            .unwrap();
        // The lander pushed b, then the daemon died before recording its report.
        sh(
            &work,
            &format!(
                "git checkout -q -B land origin/rolling && git merge -q --no-edit {b} \
                 && git push -q origin HEAD:rolling"
            ),
        );
        assert_eq!(entry_of(&store, ids[1]).await.migration_version, Some(905));
        assert_eq!(reconcile_gating(&store).await.unwrap(), (1, 1));
        let restored = entry_of(&store, ids[1]).await;
        assert_eq!(restored.state, RollingQueueEntryState::Published);
        assert_eq!(restored.migration_version, Some(901));
        assert_eq!(
            entry_of(&store, ids[0]).await.state,
            RollingQueueEntryState::Queued
        );
    }

    /// A lander that merges its sources, pushes once, and (`late`) reports an
    /// uncertain publication after an external commit landed on top of its
    /// push. It mirrors the real lander's `--expected-tip` handling.
    fn racing_lander(
        dir: &Path,
        work: &Path,
        log: &Path,
        pre_fetch: &str,
        red_sources: &str,
        late: &str,
    ) -> LanderLauncher {
        let body = r#"
echo "run $*" >> @LOG@
echo "regates $RSI_LANDER_MAX_REGATED_STALE_RETRIES" >> @LOG@
n=$(grep -c '^run ' @LOG@)
ext() { ( cd @WORK@ && git fetch -q origin rolling \
    && git checkout -q -B ext origin/rolling && printf x > "$1" && git add "$1" \
    && git commit -qm "$1" && git push -q origin HEAD:rolling ) >/dev/null 2>&1; }
expected=""; prev=""
for arg in "$@"; do
  if [ "$prev" = --expected-tip ]; then expected=$arg; echo "expected $arg" >> @LOG@; fi
  prev=$arg
done
@PRE_FETCH@
cd @WORK@ || exit 9
git fetch -q origin rolling
fetched=$(git rev-parse origin/rolling)
stale=""
if [ -n "$expected" ] && [ "$expected" != "$fetched" ]; then
  if [ "$RSI_LANDER_MAX_REGATED_STALE_RETRIES" = 0 ]; then
    echo 'lander: stale retries exhausted: advanced before the initial fetch' >&2
    echo publication_status=not_published
    echo fetched_target_id=$fetched
    exit 1
  fi
  stale="stale_retry_1=$expected..$fetched:regated"
fi
red=0
if ! git cat-file -e origin/rolling:fix 2>/dev/null; then
  for red_source in @RED@; do case " $* " in *" $red_source "*) red=1;; esac; done
fi
if [ $red = 1 ]; then
  echo 'test q::red ... FAILED' >&2
  echo publication_status=not_published
  echo fetched_target_id=$fetched
  [ -n "$stale" ] && echo "$stale"
  exit 1
fi
git checkout -q -B land origin/rolling
while [ $# -gt 0 ]; do
  if [ "$1" = --accepted ]; then
    git merge -q --no-edit "$2" >/dev/null 2>&1 || { git merge --abort; echo 'Conflict { paths: ["f"] }' >&2; echo publication_status=not_published; exit 1; }
    shift
  fi
  shift
done
git push -q origin HEAD:rolling && echo push >> @LOG@
cand=$(git rev-parse HEAD)
@LATE@
echo publication_status=published
echo fetched_target_id=$fetched
echo candidate_id=$cand
echo published_target_id=$cand
[ -n "$stale" ] && echo "$stale"
exit 0
"#
        .replace("@LOG@", &log.display().to_string())
        .replace("@WORK@", &work.display().to_string())
        .replace("@PRE_FETCH@", pre_fetch)
        .replace("@RED@", red_sources)
        .replace("@LATE@", late);
        fake_lander(dir, &body)
    }

    fn budgets(log: &Path) -> Vec<String> {
        log_lines(log)
            .into_iter()
            .filter_map(|line| line.strip_prefix("regates ").map(str::to_string))
            .collect()
    }

    /// #1007 S4 races, finding 1: the observed remote head after an uncertain
    /// publication is not the queue's own push when an external descendant of
    /// the published candidate sits on top of it. That external commit fixed
    /// the remaining red, so the red evidence is stale: the remainder is
    /// re-gated (charging the regate) and published, never refused.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn an_external_descendant_of_an_uncertain_publication_is_external_not_own() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        // Run 2 (`[a]`) pushes its candidate, then an external commit adding
        // `fix` lands on top before the lander reports; the report is uncertain.
        let launcher = racing_lander(
            dir.path(),
            &work,
            &log,
            "",
            &shas[1],
            "if [ $n = 2 ]; then ext fix; echo publication_status=unknown; \
             echo fetched_target_id=$fetched; echo candidate_id=$cand; \
             echo published_target_id=$cand; exit 1; fi",
        );
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, &work, &shas, None).await;
        run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_landed(&store, &work, ids[0], &shas[0]).await;
        assert_landed(&store, &work, ids[1], &shas[1]).await;
        assert_eq!(lander_runs(&log).len(), 3, "batch, [a], then b re-gated");
        assert_eq!(
            budgets(&log),
            vec!["1", "1", "0"],
            "the external descendant spent the batch's regate"
        );
        assert_eq!(wake_count(&store).await, 2);
    }

    /// Finding 3: a push after the queue's probe but before the lander's
    /// initial fetch is charged through `--expected-tip`. With the budget
    /// already spent the lander refuses, and the queue settles the remainder
    /// as `queue_out_of_band_regate_exhausted` instead of gating it.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_push_between_the_probe_and_the_lander_fetch_spends_the_batch_regate() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b", "c"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        // Run 2 is hit by a push right after the queue probed (charged, so the
        // budget is spent); run 4 (`[b, c]`) is hit by a second one.
        let launcher = racing_lander(
            dir.path(),
            &work,
            &log,
            "if [ $n = 2 ]; then ext e1; fi\n\
             if [ $n = 4 ]; then ext e2; fi",
            &shas[0],
            "",
        );
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, &work, &shas, None).await;
        run_next_batch(&store, &launcher, 4).await.unwrap();
        let runs = lander_runs(&log);
        assert_eq!(runs.len(), 4, "{runs:?}");
        assert!(!runs[0].contains("--expected-tip"), "{runs:?}");
        for run in &runs[1..] {
            assert!(run.contains("--expected-tip"), "{runs:?}");
        }
        assert_eq!(
            budgets(&log),
            vec!["1", "1", "0", "0"],
            "the probe-to-fetch push in run 2 spent the regate"
        );
        assert_isolated_red(&store, ids[0], "q::red").await;
        for id in &ids[1..] {
            let entry = entry_of(&store, *id).await;
            assert_eq!(entry.state, RollingQueueEntryState::Refused);
            assert_eq!(
                entry.outcome.unwrap().refusal.as_deref(),
                Some(QUEUE_REGATE_EXHAUSTED)
            );
        }
        assert!(!is_ancestor_of_origin(&work, &shas[1]));
    }

    /// A provisional source published while an external migration landed
    /// between the queue's plan fetch and the lander's fetch: the plan says
    /// 901, the published provisional commit carries 902.
    async fn crashed_provisional_run(
        work: &Path,
    ) -> (Arc<tokio::sync::Mutex<Store>>, Uuid, Uuid, String) {
        let a = commit_file(work, "a", "a", "a");
        sh(
            work,
            "git checkout -q -B b origin/rolling && mkdir -p tools/provisional-migrations \
             && printf '{}\\n' > tools/provisional-migrations/b.json && printf b > b \
             && git add -A && git commit -qm b",
        );
        let b = head(work);
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, work, &[a, b.clone()], None).await;
        store
            .lock()
            .await
            .conn
            .execute(
                "UPDATE rolling_queue_entries SET migration_version=905 WHERE id=?1",
                rusqlite::params![ids[1].to_string()],
            )
            .unwrap();
        let claimed = store
            .lock()
            .await
            .claim_rolling_queue_batch(Utc::now(), 4)
            .unwrap()
            .unwrap();
        store
            .lock()
            .await
            .begin_rolling_queue_bisect(claimed.batch_id, "gate_failed", Utc::now())
            .unwrap();
        let group: Vec<RollingQueueEntryV1> = vec![entry_of(&store, ids[1]).await];
        record_migration_plan(&store, work, claimed.batch_id, &group)
            .await
            .unwrap();
        // An external migration lands, then the lander publishes b's
        // provisional commit (parents: the tip and b) renumbered to 902.
        sh(
            work,
            &format!(
                "git checkout -q -B ext origin/rolling && mkdir -p crates/rsid-store/src/store/migrations \
                 && printf 'impl Store {{}}\\n' > crates/rsid-store/src/store/migrations/v901.rs \
                 && git add -A && git commit -qm ext901 && git push -q origin HEAD:rolling \
                 && git checkout -q -B land origin/rolling && git merge -q --no-commit --no-ff {b} \
                 && printf 'impl Store {{}}\\n' > crates/rsid-store/src/store/migrations/v902.rs \
                 && git add -A && git commit -qm provisional && git push -q origin HEAD:rolling"
            ),
        );
        (store, ids[1], claimed.batch_id, b)
    }

    /// Finding 2: the restart recovers the number from the published
    /// provisional commit (902), not the stale plan (901).
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn restart_recovers_the_actual_migration_number_from_the_published_commit() {
        let (_dir, _origin, work) = repos();
        let (store, id, _batch, b) = crashed_provisional_run(&work).await;
        assert_eq!(
            planned_number(&store, id).await,
            Some(901),
            "the plan was made before the external migration"
        );
        assert_eq!(published_migration_number(&work, &b), Some(902));
        assert_eq!(reconcile_gating(&store).await.unwrap(), (1, 1));
        let restored = entry_of(&store, id).await;
        assert_eq!(restored.state, RollingQueueEntryState::Published);
        assert_eq!(restored.migration_version, Some(902));
    }

    /// Finding 2: an actual receipt number saved before the crash is never
    /// overwritten on restart, by the plan or by the commit scan.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn restart_never_overwrites_a_saved_receipt_migration_number() {
        let (_dir, _origin, work) = repos();
        let (store, id, batch, _b) = crashed_provisional_run(&work).await;
        // The queue saved the lander's receipt (903), then died before settling.
        bisect_event(
            &store,
            batch,
            "migration_receipt",
            Some(id),
            serde_json::json!({ "version": 903 }),
        )
        .await
        .unwrap();
        store
            .lock()
            .await
            .record_rolling_queue_assigned_migration(id, 903)
            .unwrap();
        assert_eq!(reconcile_gating(&store).await.unwrap(), (1, 1));
        assert_eq!(entry_of(&store, id).await.migration_version, Some(903));
    }

    async fn planned_number(store: &Arc<tokio::sync::Mutex<Store>>, id: Uuid) -> Option<u32> {
        store
            .lock()
            .await
            .planned_rolling_queue_migration(id)
            .unwrap()
    }

    // #1208: a source already on rolling is published, never gated.

    async fn assert_published_out_of_band(
        store: &Arc<tokio::sync::Mutex<Store>>,
        work: &Path,
        id: Uuid,
        sha: &str,
    ) {
        let entry = entry_of(store, id).await;
        assert_eq!(entry.state, RollingQueueEntryState::Published, "{entry:?}");
        let outcome = entry.outcome.unwrap();
        assert_eq!(outcome.refusal, None);
        let tip = outcome.landed_sha.unwrap();
        assert!(
            git(work, &["merge-base", "--is-ancestor", sha, &tip]).is_some(),
            "the landed tip {tip} contains {sha}"
        );
        assert!(
            outcome
                .detail
                .unwrap_or_default()
                .contains("landed outside the queue")
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_member_already_on_rolling_is_published_without_a_gate() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b", "c"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        // The operator lands b directly, after it was enqueued.
        let log = dir.path().join("lander.log");
        let launcher = merging_lander(dir.path(), &work, &log, "");
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, &work, &shas, None).await;
        sh(
            &work,
            &format!("git push -q origin {}:refs/heads/rolling", shas[1]),
        );
        let results = run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_eq!(results.len(), 3, "every member settles exactly once");
        assert_published_out_of_band(&store, &work, ids[1], &shas[1]).await;
        let runs = lander_runs(&log);
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert!(
            !runs[0].contains(&format!("--accepted {}", shas[1])),
            "the landed source is not gated: {runs:?}"
        );
        for (id, sha) in [(ids[0], &shas[0]), (ids[2], &shas[2])] {
            assert_landed(&store, &work, id, sha).await;
        }
        let guard = store.lock().await;
        assert_eq!(events_of(&guard, "already_landed").len(), 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_batch_whose_sources_all_landed_runs_no_lander_and_settles_published() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        let launcher = merging_lander(dir.path(), &work, &log, "");
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, &work, &shas, None).await;
        sh(
            &work,
            &format!(
                "git checkout -q -B landed origin/rolling && git merge -q --no-edit {} {} \
                 && git push -q origin HEAD:rolling",
                shas[0], shas[1]
            ),
        );
        let results = run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_eq!(results.len(), 2);
        assert!(lander_runs(&log).is_empty(), "nothing is gated");
        for (id, sha) in ids.iter().zip(&shas) {
            assert_published_out_of_band(&store, &work, *id, sha).await;
        }
        let state: String = store
            .lock()
            .await
            .conn
            .query_row("SELECT state FROM rolling_queue_batches", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(state, "published");
        assert_eq!(wake_count(&store).await, 2);
    }

    /// Guard for the merging lander: the first run that carries `sha` lands it
    /// on rolling first (as an operator would) and then refuses it like the
    /// real lander does an integrated source.
    fn lands_then_refuses(work: &Path, marker: &Path, sha: &str) -> String {
        format!(
            "case \" $* \" in *\" {sha} \"*) if [ ! -f {marker} ]; then touch {marker}; \
             git -C {work} push -q origin {sha}:refs/heads/rolling; \
             echo 'rsi-rolling-land: accepted source {sha} is already integrated; supply \
             --accepted BASE:{sha} with its historical accepted base' >&2; \
             echo publication_status=not_published; exit 1; fi;; esac",
            marker = marker.display(),
            work = work.display(),
        )
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_source_the_lander_calls_already_integrated_settles_published_not_gate_failed() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b", "c"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        let guard = lands_then_refuses(&work, &dir.path().join("landed"), &shas[1]);
        let launcher = merging_lander(dir.path(), &work, &log, &guard);
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, &work, &shas, None).await;
        let results = run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_eq!(results.len(), 3);
        assert_published_out_of_band(&store, &work, ids[1], &shas[1]).await;
        let runs = lander_runs(&log);
        assert_eq!(runs.len(), 2, "{runs:?}");
        assert!(
            !runs[1].contains(&format!("--accepted {}", shas[1])),
            "the retry drops the landed source"
        );
        for (id, sha) in [(ids[0], &shas[0]), (ids[2], &shas[2])] {
            assert_landed(&store, &work, id, sha).await;
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_lone_source_the_lander_calls_already_integrated_settles_published() {
        let (dir, _origin, work) = repos();
        let sha = commit_file(&work, "a", "a", "a");
        let log = dir.path().join("lander.log");
        let guard = lands_then_refuses(&work, &dir.path().join("landed"), &sha);
        let launcher = merging_lander(dir.path(), &work, &log, &guard);
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let id = queued(&store, &work, &sha, "k").await;
        let (settled, result) = run_next(&store, &launcher).await.unwrap().unwrap();
        assert_eq!(settled, id);
        assert_eq!(result.state, RollingQueueEntryState::Published);
        assert_published_out_of_band(&store, &work, id, &sha).await;
        assert_eq!(lander_runs(&log).len(), 1);
        assert_eq!(wake_count(&store).await, 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn an_already_integrated_report_is_published_from_ancestry_or_typed_never_a_red_gate() {
        let stderr = "rsi-rolling-land: accepted source abc is already integrated; supply --accepted BASE:abc";
        let landed = classify_run(
            Some(1),
            "publication_status=not_published\n",
            stderr,
            || Some("f".repeat(40)),
        );
        assert_eq!(landed.state, RollingQueueEntryState::Published);
        assert_eq!(landed.outcome.landed_sha, Some("f".repeat(40)));
        let unconfirmed = classify_run(Some(1), "", stderr, || None);
        assert_eq!(unconfirmed.state, RollingQueueEntryState::Refused);
        assert_eq!(
            unconfirmed.outcome.refusal.as_deref(),
            Some(QUEUE_SOURCE_ALREADY_INTEGRATED)
        );
        assert!(!is_bisectable(&unconfirmed));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_member_landed_out_of_band_during_a_bisect_is_published_and_never_regated() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b", "c"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        // The first (whole-batch) run lands c out of band, then goes red on b.
        let marker = dir.path().join("landed");
        let guard = format!(
            "if [ ! -f {marker} ]; then touch {marker}; git -C {work} push -q origin {c}:refs/heads/rolling; fi\n{red}",
            marker = marker.display(),
            work = work.display(),
            c = shas[2],
            red = red_when_any(&[(shas[1].as_str(), "q::b_breaks")]),
        );
        let launcher = merging_lander(dir.path(), &work, &log, &guard);
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, &work, &shas, None).await;
        let results = run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_eq!(results.len(), 3, "every member settles exactly once");
        assert_published_out_of_band(&store, &work, ids[2], &shas[2]).await;
        assert_landed(&store, &work, ids[0], &shas[0]).await;
        assert_isolated_red(&store, ids[1], "q::b_breaks").await;
        let runs = lander_runs(&log);
        assert!(
            runs[1..]
                .iter()
                .all(|run| !run.contains(&format!("--accepted {}", shas[2]))),
            "the landed member is never gated again: {runs:?}"
        );
        let guard = store.lock().await;
        assert!(events_of(&guard, "regate_spent").is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_batch_past_its_gate_wall_time_is_refused_typed_and_the_lander_stopped() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        let pid_file = dir.path().join("sleeper.pid");
        let guard = format!("sleep 60 & echo $! > {pid}; wait", pid = pid_file.display());
        let launcher = merging_lander(dir.path(), &work, &log, &guard)
            .with_gate_timeout(Duration::from_secs(2));
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let ids = queue_all(&store, &work, &shas, None).await;
        let started = std::time::Instant::now();
        let results = run_next_batch(&store, &launcher, 4).await.unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "bounded by the budget"
        );
        assert_eq!(results.len(), 2);
        for id in &ids {
            let entry = entry_of(&store, *id).await;
            assert_eq!(entry.state, RollingQueueEntryState::Refused);
            let outcome = entry.outcome.unwrap();
            assert_eq!(outcome.refusal.as_deref(), Some(QUEUE_GATE_TIMEOUT));
            assert!(
                outcome
                    .detail
                    .unwrap()
                    .contains("rolling_queue_gate_timeout_mins")
            );
        }
        for sha in &shas {
            assert!(!is_ancestor_of_origin(&work, sha));
        }
        // The lander's process group, its children included, was stopped.
        let pid = std::fs::read_to_string(&pid_file).unwrap();
        let pid = nix::unistd::Pid::from_raw(pid.trim().parse().unwrap());
        let mut alive = true;
        for _ in 0..50 {
            alive = nix::sys::signal::kill(pid, None).is_ok();
            if !alive {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(!alive, "the lander's child outlived the timeout");
        let guard = store.lock().await;
        assert_eq!(events_of(&guard, "gate_timeout").len(), 1);
        drop(guard);
        assert_eq!(wake_count(&store).await, 2);
    }
}
