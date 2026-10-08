//! Per-session-kind preamble loader.
//!
//! A session's startup text has two parts:
//!
//!   * the RSI-generic part, compiled into the daemon ([`load_generic`]:
//!     agent-control frame, backend policy, daemon-message convention, and the
//!     thoughts commit policy when the tree has a `thoughts/` directory), and
//!   * the project part, read from the session's OWN working tree
//!     ([`load_project_preamble`], [`load_orchestration_router`]): its
//!     `.claude/commands/_shared/worker_preamble*.md` and
//!     `.claude/commands/orchestration_router.md`. A project without them
//!     contributes nothing; there is no fallback to the rsi repo's text, so the
//!     startup text never points at files the session's tree does not have.
//!
//! The session's tree is the sandbox root when sandboxed, else its
//! `working_dir`; callers pass that directory. The files are looked up in it
//! and in its ancestors up to and including the first directory holding a
//! `.git` (a session started in a repo subdirectory still finds the repo's
//! files; the search never climbs past the repository).
//!
//! Read-failure modes (variant or base file):
//!   - `NotFound`: logged at debug; falls through to base (or `None` if base also missing).
//!   - other IO error: logged at warn; falls through to base (or `None`).
//!   - `None` is never an error: launch is never aborted by preamble loading.
//!
//! [`harness_root`] (the daemon's own checkout, discovered once per process) no
//! longer feeds any session text; it remains for tools and tests that need the
//! rsi repo's files.

use super::agent_verbs::agent_authority::{AgentAuthorityProjection, is_baseline_verb};
use crate::error::{DaemonError, Result};
use rsi_common::agent_authority_catalog::{
    AGENT_AUTHORITY_CATALOG_SCHEMA_VERSION_V1, AgentAuthorityCatalogV1,
    AgentAuthorityControlDetailV1, AgentAuthorityControlV1, AgentAuthorityRefusalV1,
};
use rsi_common::agent_control_schema::{AgentControlDescriptorV1, AgentControlVerbV1 as Verb};
use rsi_common::types::{Session, SessionKind};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const SHARED_DIR: &str = ".claude/commands/_shared";
const BASE_FILENAME: &str = "worker_preamble.md";

/// Filename of the orchestration router skill, located one directory shallower
/// than the per-kind preambles (under `.claude/commands/`, NOT `_shared/`).
/// This is the "mouth of the pipeline" frame applied to every leaf-kind launch.
const ORCHESTRATION_ROUTER_FILENAME: &str = "orchestration_router.md";
const COMMANDS_DIR: &str = ".claude/commands";

/// The shipped contract is compiled into rsid. The source-tree files remain
/// available as optional project context but are not required at runtime.
pub(crate) const EMBEDDED_GUIDANCE_VERSION: u32 = 1;
const EMBEDDED_ROUTER: &str = include_str!("../../../../.claude/commands/orchestration_router.md");
const EMBEDDED_BASE: &str = include_str!("../../../../.claude/commands/_shared/worker_preamble.md");
const EMBEDDED_COMMON: &str = include_str!("guidance/common_v1.md");
const EMBEDDED_WORKER: &str = include_str!("guidance/worker_v1.md");
const EMBEDDED_EPIC_LEAD: &str = include_str!("guidance/epic_lead_v1.md");
const EMBEDDED_MANAGER: &str = include_str!("guidance/manager_v1.md");
const EMBEDDED_REVIEWER: &str = include_str!("guidance/assigned_reviewer_v1.md");
const EMBEDDED_GLOBAL_MANAGER: &str = include_str!("guidance/global_manager_v1.md");
const EMBEDDED_PORTFOLIO_MANAGER: &str = include_str!("guidance/portfolio_manager_v1.md");

fn embedded_kind_preamble(kind: SessionKind) -> Option<&'static str> {
    match kind {
        SessionKind::Bug => Some(include_str!(
            "../../../../.claude/commands/_shared/worker_preamble_bug.md"
        )),
        SessionKind::Feature => Some(include_str!(
            "../../../../.claude/commands/_shared/worker_preamble_feature.md"
        )),
        SessionKind::Refactor => Some(include_str!(
            "../../../../.claude/commands/_shared/worker_preamble_refactor.md"
        )),
        SessionKind::Research => Some(include_str!(
            "../../../../.claude/commands/_shared/worker_preamble_research.md"
        )),
        _ => None,
    }
}

/// Guidance ids must match the projection's role bits exactly; a mismatch is a
/// visible store error rather than silently wrong role text.
fn check_guidance_ids(projection: &AgentAuthorityProjection) -> Result<()> {
    let mut expected_ids = vec!["common", "worker"];
    if projection.is_lead {
        expected_ids.push("epic_lead");
    }
    if projection.is_manager {
        expected_ids.push("manager");
    }
    if projection.is_reviewer {
        expected_ids.push("assigned_reviewer");
    }
    if projection.is_global_manager {
        expected_ids.push("global_manager");
        expected_ids.push("portfolio_manager");
    }
    if projection.guidance_ids != expected_ids {
        return Err(DaemonError::Store(
            "inconsistent agent guidance projection".into(),
        ));
    }
    Ok(())
}

/// Build one versioned, role-filtered instruction snapshot from the durable
/// projection. Launch and rotation will consume this after authority publishes;
/// rendering itself never grants a server-side permission.
pub(crate) fn render_versioned_guidance(
    kind: SessionKind,
    projection: &AgentAuthorityProjection,
) -> Result<String> {
    if !rsi_common::is_leaf_kind(kind) {
        return Err(DaemonError::InvalidParam(
            "agent_guidance_requires_leaf_session".into(),
        ));
    }
    check_guidance_ids(projection)?;

    let mut sections = vec![
        format!(
            "## RSI embedded guidance v{EMBEDDED_GUIDANCE_VERSION}\n\nAuthority revision: `{}`. This text describes the current snapshot; the daemon checks every call again.",
            projection.revision
        ),
        EMBEDDED_ROUTER.trim().to_string(),
        EMBEDDED_BASE.trim().to_string(),
    ];
    if let Some(kind_text) = embedded_kind_preamble(kind) {
        sections.push(kind_text.trim().to_string());
    }
    sections.extend([
        EMBEDDED_COMMON.trim().to_string(),
        EMBEDDED_WORKER.trim().to_string(),
    ]);
    if projection.pending {
        sections.push(PENDING_PUBLICATION_NOTE.into());
    } else {
        for id in &projection.guidance_ids {
            if let Some(text) = role_guidance(id) {
                sections.push(text.trim().to_string());
            }
        }
    }
    sections.extend([
        THOUGHTS_COMMIT_POLICY.trim().to_string(),
        DAEMON_MESSAGE_CONVENTION.trim().to_string(),
    ]);

    let mut catalog = String::from(
        "## Controls in this authority snapshot\n\nUse a native tool when available; `rsi-rpc` supplies the transport token automatically. The token never belongs in request parameters.\n",
    );
    for verb in advertised_verbs(projection) {
        write_verb_entry(&mut catalog, verb.descriptor());
    }
    if !projection.pending {
        for (title, values) in [
            ("AgentManagerUpdate variants", &projection.update_variants),
            (
                "AgentManagerPrepareControl actions",
                &projection.prepared_actions,
            ),
            (
                "Delegated operator methods",
                &projection.delegated_operator_methods,
            ),
        ] {
            if !values.is_empty() {
                write!(catalog, "\n\n{title}: {}.", values.join(", "))
                    .expect("writing to a String cannot fail");
            }
        }
        if !projection.control_actions.is_empty() {
            let actions = manager_action_names(projection)?;
            write!(
                catalog,
                "\n\nAgentManagerControl actions: {}.",
                actions.join(", ")
            )
            .expect("writing to a String cannot fail");
        }
    }
    sections.push(catalog);
    Ok(sections.join("\n\n"))
}

/// Returns the variant filename for a kind. Kinds without a dedicated
/// variant file return `None`, which means "use base".
fn variant_filename(kind: SessionKind) -> Option<&'static str> {
    match kind {
        SessionKind::Bug => Some("worker_preamble_bug.md"),
        SessionKind::Feature => Some("worker_preamble_feature.md"),
        SessionKind::Refactor => Some("worker_preamble_refactor.md"),
        SessionKind::Research => Some("worker_preamble_research.md"),
        // Container kinds shouldn't reach this loader (they're gated at
        // `is_leaf_kind` before launch), but pattern-match exhaustively
        // anyway so future kinds force a recompile of this file.
        SessionKind::Standard
        | SessionKind::TaskRabbit
        | SessionKind::Story
        | SessionKind::Task
        | SessionKind::Group
        | SessionKind::Epic => None,
        _ => None,
    }
}

/// Returns the discovered harness root (the directory containing `.claude/`).
/// Cached for the daemon's lifetime via `OnceLock`.
pub fn harness_root() -> Option<&'static Path> {
    static ROOT: OnceLock<Option<PathBuf>> = OnceLock::new();
    ROOT.get_or_init(discover_harness_root).as_deref()
}

fn discover_harness_root() -> Option<PathBuf> {
    let env_root = rsi_common::identity::env_with_legacy(
        "RSI_HARNESS_ROOT",
        &["MOTHERSHIP_HARNESS_ROOT", "FLYWHEEL_HARNESS_ROOT"],
    )
    .ok()
    .map(PathBuf::from);
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    let found = discover_harness_root_from(
        env_root.as_deref(),
        std::env::current_dir().ok().as_deref(),
        exe_dir.as_deref(),
        Path::new(env!("CARGO_MANIFEST_DIR")),
        Some(env!("RSI_BUILD_MAIN_WORKTREE"))
            .filter(|main| !main.is_empty())
            .map(Path::new),
    );
    if found.is_none() {
        tracing::warn!("Unable to discover harness root containing .claude/; preamble disabled");
    }
    found
}

/// Ordered discovery of the daemon's own harness checkout: the operator's
/// `RSI_HARNESS_ROOT`, the daemon's cwd, its executable's directory, then the
/// repo this binary was built from (#1613). The last step keeps an installed
/// daemon (`~/.rsi/install/rsid`, started with an arbitrary cwd) resolving the
/// RSI project for `AgentCreateIssue {harness: true}` from any project; none of
/// it depends on a caller's working directory.
///
/// A build dir inside a git linked worktree (a manager sandbox) resolves to the
/// MAIN worktree first, so the root is the stable checkout a project registers
/// and survives the sandbox's reclaim: the build-time embedded `build_main`,
/// else the main worktree read from the build dir's own git metadata.
pub(crate) fn discover_harness_root_from(
    env_root: Option<&Path>,
    cwd: Option<&Path>,
    exe_dir: Option<&Path>,
    build_dir: &Path,
    build_main: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(p) = env_root {
        if p.join(SHARED_DIR).join(BASE_FILENAME).is_file() {
            return Some(p.to_path_buf());
        }
        tracing::warn!(
            root = %p.display(),
            "RSI_HARNESS_ROOT set but base preamble not found; falling through to discovery"
        );
    }
    cwd.into_iter()
        .chain(exe_dir)
        .chain(build_main)
        .find_map(walk_up_for_claude)
        .or_else(|| {
            // The build dir's own root, mapped to its main worktree when it is
            // a linked one and that checkout carries the harness too.
            let root = walk_up_for_claude(build_dir)?;
            Some(
                main_worktree_of(&root)
                    .filter(|main| main.join(SHARED_DIR).join(BASE_FILENAME).is_file())
                    .unwrap_or(root),
            )
        })
}

/// The main worktree of the git checkout rooted at `start` (a directory holding
/// `.git`, never an ancestor: a test or nested tree must not escape upward), mapped through a linked worktree's `.git` file
/// (`gitdir: <main>/.git/worktrees/<name>`, whose `commondir` names the main
/// `.git`) to the parent of the common dir. `None` outside a repo, for a bare
/// common dir, or when the metadata is unreadable. Pure file reads, so it is
/// portable and needs no git binary.
fn main_worktree_of(start: &Path) -> Option<PathBuf> {
    let top = start;
    let dot_git = top.join(".git");
    if dot_git.is_dir() {
        return Some(top.to_path_buf());
    }
    let text = std::fs::read_to_string(&dot_git).ok()?;
    let gitdir = Path::new(text.lines().find_map(|l| l.strip_prefix("gitdir:"))?.trim());
    let gitdir = top.join(gitdir);
    let common = match std::fs::read_to_string(gitdir.join("commondir")) {
        Ok(rel) => gitdir.join(rel.trim()),
        // `<main>/.git/worktrees/<name>` -> `<main>/.git`.
        Err(_) => gitdir.parent()?.parent()?.to_path_buf(),
    };
    let common = std::fs::canonicalize(&common).ok()?;
    if common.file_name().is_some_and(|name| name == ".git") {
        common.parent().map(Path::to_path_buf)
    } else {
        None
    }
}

/// Test fixture: a main checkout at `<root>/main` and a git linked worktree of
/// it at `<root>/sandbox` (git's own on-disk layout), both carrying the base
/// preamble. Returns `(main, build_dir)` with `build_dir` the worktree's
/// `crates/rsid`.
#[cfg(test)]
pub(crate) fn linked_worktree_fixture(root: &Path) -> (PathBuf, PathBuf) {
    let main = root.join("main");
    let sandbox = root.join("sandbox");
    let admin = main.join(".git/worktrees/sandbox");
    std::fs::create_dir_all(&admin).unwrap();
    std::fs::write(admin.join("commondir"), "../..\n").unwrap();
    std::fs::create_dir_all(&sandbox).unwrap();
    std::fs::write(
        sandbox.join(".git"),
        format!("gitdir: {}\n", admin.display()),
    )
    .unwrap();
    for checkout in [&main, &sandbox] {
        let shared = checkout.join(SHARED_DIR);
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(shared.join(BASE_FILENAME), "BASE").unwrap();
    }
    let build_dir = sandbox.join("crates/rsid");
    std::fs::create_dir_all(&build_dir).unwrap();
    (std::fs::canonicalize(&main).unwrap(), build_dir)
}

fn walk_up_for_claude(start: &Path) -> Option<PathBuf> {
    let mut cur: Option<&Path> = Some(start);
    while let Some(p) = cur {
        if p.join(SHARED_DIR).join(BASE_FILENAME).is_file() {
            return Some(p.to_path_buf());
        }
        cur = p.parent();
    }
    None
}

/// Load the preamble for a given kind. Returns `None` if neither the variant
/// nor the base file is readable. Never panics; never aborts launch.
/// Binary-embedded agent-control frame ([`agent_discovery_nudge`]).
///
/// Appended to every per-kind preamble by [`load`], so BOTH fresh launch
/// (`session::launch`) and context rotation (`session::rotation`) - which each
/// consume `load()` - advertise the daemon's `Agent*` control verbs to the
/// worker. This is embedded in the binary (not `.claude/` disk discovery) so it
/// travels to any working directory, and it advertises ONLY the `Agent*` verbs
/// plus the `rsi-rpc` token convention - never the generic RPC passthrough.
const AGENT_CONTROL_FRAME: &str = "\
## Agent control (rsi)

You are an rsi-managed agent session. Start here: call \
`AgentGetAuthorityCatalog` with `{}` (native tool \
`rsi_control_authority_catalog`; Claude lists it as \
`mcp__rsi-agent__rsi_control_authority_catalog`; CLI fallback \
`rsi-rpc AgentGetAuthorityCatalog`). It is your operator's manual: your current \
roles, the operating rules for those roles, and exactly the controls you may \
call now. Pass `{\"verb\": \"<name>\"}` for one control alone: its schema, a \
minimal example request and its refusal codes. Call it again after a role \
change or an authority refusal. The catalog never grants authority: the \
daemon checks every call.

Prefer the typed `rsi_control_*` tools when your provider exposes them. \
`rsi-rpc <Verb>` is the validated compatible fallback; its authority token is \
supplied automatically via `$RSI_SESSION_TOKEN` (transport-only - NEVER pass it \
in `--params`). Do not invoke any other RPC method as an agent.

`AgentScheduleWake`: `mode` is required. Use `resume` for same-session \
continuation (never `fresh` on your own session), `fresh` only for a \
best-effort post-terminal root launch that carries no lead authority, \
`on_terminal` for a child watch, `program_guard` for the unattended-program \
sentinel, or `when` for a wait the daemon evaluates itself (`when.jobs_terminal` \
job ids you own, or `when.sha_on_rolling`; optional `timeout_seconds`): submit a \
batch of `AgentSubmitJob` jobs with `wake:\"none\"` and arm ONE `when` wake, so you \
are resumed once when the last job ends instead of once per job. An explicit \
`name` replaces your earlier enabled wake of that name.

Manager appointment and scope changes are operator-only. A title, prompt, or \
tool registration grants no authority; existing child-control permissions stay \
unchanged. Direct operator instructions take precedence: surface conflicts \
before acting. Human approvals remain operator-owned; manager mail cannot \
answer or clear them.";

/// Write one catalog entry: method, native tool name, and the canonical
/// descriptor text (the same text `rsi-rpc agent` and MCP `tools/list` show).
fn write_verb_entry(out: &mut String, descriptor: &AgentControlDescriptorV1) {
    write!(out, "\n- `{}`", descriptor.method).expect("writing to a String cannot fail");
    if let Some(tool) = descriptor.native_tool {
        write!(out, " / `{}`", tool.name()).expect("writing to a String cannot fail");
    }
    write!(out, ": {}", descriptor.description).expect("writing to a String cannot fail");
}

/// The agent-control frame every RSI-managed launch and rotation carries.
///
/// Deliberately short and role-independent: it sends every agent to
/// `AgentGetAuthorityCatalog`, which serves role guidance and the permitted
/// controls from the daemon's current authority projection
/// ([`render_authority_catalog`]), so startup text can neither drift from nor
/// overstate what the caller may do. It advertises ONLY the `Agent*` surface
/// plus the `rsi-rpc` token convention - never the generic RPC passthrough.
pub fn agent_discovery_nudge() -> &'static str {
    AGENT_CONTROL_FRAME
}

/// Binary-shipped role section for one projection guidance id.
fn role_guidance(id: &str) -> Option<&'static str> {
    match id {
        "epic_lead" => Some(EMBEDDED_EPIC_LEAD),
        "manager" => Some(EMBEDDED_MANAGER),
        "assigned_reviewer" => Some(EMBEDDED_REVIEWER),
        "global_manager" => Some(EMBEDDED_GLOBAL_MANAGER),
        "portfolio_manager" => Some(EMBEDDED_PORTFOLIO_MANAGER),
        _ => None,
    }
}

const PENDING_PUBLICATION_NOTE: &str = "Authority publication is pending. Continue with the worker baseline and wait for a refreshed projection before using a role-specific control.";

fn manager_action_names(projection: &AgentAuthorityProjection) -> Result<Vec<String>> {
    projection
        .control_actions
        .iter()
        .map(|action| {
            serde_json::to_value(action)?
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| DaemonError::Store("invalid manager action name".into()))
        })
        .collect()
}

/// Controls a snapshot advertises: a pending publication keeps the worker
/// baseline even if the caller's projection was concurrently superseded.
fn advertised_verbs(projection: &AgentAuthorityProjection) -> impl Iterator<Item = Verb> + '_ {
    projection
        .verbs
        .iter()
        .copied()
        .filter(|verb| !projection.pending || is_baseline_verb(*verb))
}

/// Build the caller's operator's manual (`AgentGetAuthorityCatalog`) from one
/// durable projection: binary-shipped common, worker and committed role
/// guidance, plus exactly the advertised controls. A pending publication gets
/// the worker baseline and a refresh instruction. Never grants authority.
pub(crate) fn render_authority_catalog(
    session_id: uuid::Uuid,
    projection: &AgentAuthorityProjection,
    requested: Option<Verb>,
) -> Result<AgentAuthorityCatalogV1> {
    check_guidance_ids(projection)?;
    let mut guidance = vec![EMBEDDED_COMMON.trim(), EMBEDDED_WORKER.trim()];
    let mut roles = vec!["worker".to_string()];
    if projection.pending {
        guidance.push(PENDING_PUBLICATION_NOTE);
    } else {
        for id in &projection.guidance_ids {
            if let Some(text) = role_guidance(id) {
                guidance.push(text.trim());
                roles.push((*id).to_string());
            }
        }
    }
    let controls: Vec<AgentAuthorityControlV1> = advertised_verbs(projection)
        .map(|verb| {
            let descriptor = verb.descriptor();
            AgentAuthorityControlV1 {
                method: descriptor.method.to_string(),
                native_tool: descriptor.native_tool.map(|tool| tool.name().to_string()),
                description: descriptor.description.to_string(),
            }
        })
        .collect();
    let control = requested.map(|verb| {
        let descriptor = verb.descriptor();
        AgentAuthorityControlDetailV1 {
            method: descriptor.method.to_string(),
            native_tool: descriptor.native_tool.map(|tool| tool.name().to_string()),
            description: descriptor.description.to_string(),
            permitted: advertised_verbs(projection).any(|advertised| advertised == verb),
            parameters: descriptor.parameters(),
            example: verb.example(),
            refusals: verb
                .refusals()
                .iter()
                .map(|refusal| AgentAuthorityRefusalV1 {
                    code: refusal.code.to_string(),
                    next_action: refusal.next_action.to_string(),
                })
                .collect(),
        }
    });
    let owned = |values: &[&'static str]| values.iter().map(|v| (*v).to_string()).collect();
    // A `verb` request gets only the compact envelope and that one control's
    // detail; the full list, guidance and manager lists come without `verb`.
    let committed = !projection.pending && control.is_none();
    Ok(AgentAuthorityCatalogV1 {
        schema_version: AGENT_AUTHORITY_CATALOG_SCHEMA_VERSION_V1,
        session_id,
        authority_revision: projection.revision.clone(),
        pending: projection.pending,
        roles,
        guidance: if control.is_some() {
            String::new()
        } else {
            guidance.join("\n\n")
        },
        controls: if control.is_some() {
            Vec::new()
        } else {
            controls
        },
        manager_update_variants: if committed {
            owned(&projection.update_variants)
        } else {
            Vec::new()
        },
        manager_control_actions: if committed {
            manager_action_names(projection)?
        } else {
            Vec::new()
        },
        manager_prepared_actions: if committed {
            owned(&projection.prepared_actions)
        } else {
            Vec::new()
        },
        delegated_operator_methods: if committed {
            owned(&projection.delegated_operator_methods)
        } else {
            Vec::new()
        },
        control,
    })
}

/// Commit policy for durable `thoughts/` artifacts.
///
/// This is binary-embedded so it reaches every RSI-managed provider. Providers
/// with developer instructions receive it through [`load`]; Codex CLI receives
/// it in its first-turn stdin payload because it has no system-prompt channel.
pub const THOUGHTS_COMMIT_POLICY: &str = "\
## Thoughts artifact commit policy (HARD)

Whenever you create or modify a file under `thoughts/`, commit the relevant \
`thoughts/` paths before you report the work complete or return to the master. \
Do not leave newly created thoughts artifacts as untracked or dirty files. \
Stage only the thoughts files produced by this task (plus directly related task \
files when they belong in the same commit); never absorb unrelated user edits. \
If a commit is blocked, report the exact blocker and do not claim completion.";

/// RSI-owned backend policy for orchestration commands.
///
/// This is binary-injected for every RSI-managed provider launch/rotation via
/// [`load`], so orchestration commands remain portable: repo-local command text
/// can define the general lifecycle while this daemon-owned layer preserves
/// RSI authority, session scoping, wake, halt, and fallback semantics on any
/// computer/project.
pub const RSI_BACKEND_POLICY: &str = "\
## RSI orchestration backend policy

When an orchestration command wants to dispatch, inspect, halt, or wake child \
work from inside this RSI-managed session, use only RSI-owned control surfaces.
Provider-native delegation surfaces are not interchangeable with RSI child \
sessions.

Allowed worker-control surfaces, in order:

1. RSI-native control tools when present: the `rsi_control_*` tools (for \
example `rsi_control_spawn` and `rsi_control_reserve_successor`) and \
`schedule_wake`; Claude lists them as `mcp__rsi-agent__*`.
2. RSI agent RPC verbs via `rsi-rpc` (for example `AgentSpawnChild` and \
`AgentReserveSuccessor`). `AgentGetAuthorityCatalog` lists the controls you may \
call.
3. The documented RSI directive fallback (`<docregblock>/spawn_child ...</docregblock>` \
or `<docregblock>/halt</docregblock>`) when neither native tools nor `rsi-rpc` \
are available.

Forbidden:

- provider-native subagent, task, background-worker, or parallel-agent tools \
that do not route through RSI authority and session control;
- generic harness delegation features from Claude Code, Codex, Antigravity, or \
any other provider when they bypass RSI's agent/session model;
- claiming a worker ran when only a provider-native delegation surface was \
available.

If no RSI-controlled worker mechanism is available, treat worker spawn as \
unavailable and compile the next worker prompt for the user instead.";

/// Daemon-injected message convention.
///
/// Headless provider CLIs have exactly one inbound text channel (the user
/// turn), so every daemon-side notification — terminal-watch fires, scheduled
/// wakes, stall nudges — reaches the model wearing the user role. The
/// injection sites wrap those payloads in `<rsid-daemon-message>` envelopes
/// (`rsi_common::daemon_message::wrap`); this standing layer teaches the
/// convention up front so the model never mistakes daemon traffic for the
/// human user.
pub const DAEMON_MESSAGE_CONVENTION: &str = "\
## Daemon message convention

Mid-session messages wrapped in a `<rsid-daemon-message source=\"...\">` block \
are automated rsid daemon notifications (terminal watches, scheduled wakes, \
stall nudges). They arrive on the user turn because headless provider CLIs \
have no other inbound channel, but they are NOT from the human user. Act on \
them and continue your work; do not address the human as though they sent \
one, and do not wait for the human to clarify one.";

/// Render the session-specific instruction that preserves RSI's ownership of a
/// git-worktree sandbox. This is intentionally generated from the authenticated
/// custody tuple instead of being a static repository instruction: the assigned
/// path and branch are part of the session's allocation contract.
pub(crate) fn sandbox_custody_instruction(sandbox_root: &Path, branch: &str) -> String {
    format!(
        "\
## RSI sandbox custody (HARD)

This worktree is RSI-owned. Your sandbox root is `{}` and its assigned branch is `{branch}`.

Do not run `git checkout`, `git switch`, or any command that creates or changes a branch in this worktree. Commit, push, and open a PR from the assigned branch instead. If a named branch is required, stop and request a session allocated on that branch; do not mutate this sandbox's branch.",
        sandbox_root.display()
    )
}

/// Return the ownership instruction only for a durably sandboxed session.
/// Ordinary sessions deliberately receive no custody warning.
pub(super) fn sandbox_custody_instruction_for_session(session: &Session) -> Option<String> {
    Some(sandbox_custody_instruction(
        session.sandbox_root.as_deref()?,
        session.sandbox_branch.as_deref()?,
    ))
}

pub(super) fn prepend_sandbox_custody_instruction(
    prompt: Option<String>,
    instruction: Option<String>,
) -> Option<String> {
    match instruction {
        Some(instruction) => Some(match prompt {
            Some(prompt) => format!("{instruction}\n\n{prompt}"),
            None => instruction,
        }),
        None => prompt,
    }
}

/// Directories searched for project-owned startup files: `start` and its
/// ancestors, up to and including the first one holding a `.git` entry. When no
/// `.git` is found on the way up, only `start` itself is searched, so a bare
/// directory never adopts files from an unrelated parent.
fn project_search_dirs(start: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut found_git = false;
    for dir in start.ancestors() {
        dirs.push(dir.to_path_buf());
        if dir.join(".git").exists() {
            found_git = true;
            break;
        }
    }
    if !found_git {
        dirs.truncate(1);
    }
    dirs
}

/// The project directory whose `.claude/commands` supplies the session's
/// project startup text: the first searched directory holding the base
/// preamble or the orchestration router. `None` means the project has neither.
fn project_commands_root(start: &Path) -> Option<PathBuf> {
    project_search_dirs(start).into_iter().find(|dir| {
        dir.join(SHARED_DIR).join(BASE_FILENAME).is_file()
            || dir
                .join(COMMANDS_DIR)
                .join(ORCHESTRATION_ROUTER_FILENAME)
                .is_file()
    })
}

/// Whether the session's tree has a `thoughts/` directory the commit policy can
/// refer to.
fn project_has_thoughts_dir(start: &Path) -> bool {
    project_search_dirs(start)
        .iter()
        .any(|dir| dir.join("thoughts").is_dir())
}

/// RSI-generic part of the startup text, identical for every provider and
/// project: the agent-control frame, the thoughts commit policy (only when
/// `project_dir` has a `thoughts/` directory, since the policy names that
/// path), the backend policy and the daemon-message convention. Compiled into
/// the daemon; reads nothing but a directory probe.
pub fn load_generic(project_dir: Option<&Path>) -> String {
    let mut parts = vec![agent_discovery_nudge()];
    if project_dir.is_some_and(project_has_thoughts_dir) {
        parts.push(THOUGHTS_COMMIT_POLICY);
    }
    parts.push(RSI_BACKEND_POLICY);
    parts.push(DAEMON_MESSAGE_CONVENTION);
    parts.join("\n\n")
}

/// Project part of the startup text: the base preamble followed by the kind
/// variant, read from the session's own tree (see the module docs). `None` when
/// the project ships no preamble files.
pub fn load_project_preamble(project_dir: &Path, kind: SessionKind) -> Option<String> {
    let root = project_commands_root(project_dir)?;
    load_disk_from_root(&root, kind)
}

/// Load the startup text the launch/rotation paths inject into the system
/// prompt: the session's project preamble (if its tree has one) followed by the
/// RSI-generic part ([`load_generic`]). Always `Some(_)`: the generic part is
/// compiled in, so the agent-control frame is never lost, and rotation (which
/// calls this same loader) keeps it.
pub fn load(kind: SessionKind, project_dir: &Path) -> Option<String> {
    let generic = load_generic(Some(project_dir));
    Some(match load_project_preamble(project_dir, kind) {
        Some(preamble) => format!("{preamble}\n\n{generic}"),
        None => generic,
    })
}

/// The one read path for project-part startup text. Canonicalizes the project
/// root and the candidate file and reads only when the canonical file lies
/// inside the canonical root, so a symlink or `..` cannot pull another
/// project's text (or any readable file) into this project's prompts. A file
/// that escapes is treated like a missing one: a warning is logged and the
/// caller gets an error it already handles by omitting the part, never a launch
/// failure. A path that does not exist reports `NotFound`.
fn read_project_file(root: &Path, path: &Path) -> std::io::Result<String> {
    let canonical_root = root.canonicalize()?;
    let canonical_file = path.canonicalize()?;
    if !canonical_file.starts_with(&canonical_root) {
        tracing::warn!(
            path = %path.display(),
            root = %root.display(),
            "Project preamble file resolves outside the project tree; omitting it"
        );
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "project preamble file resolves outside the project tree",
        ));
    }
    std::fs::read_to_string(canonical_file)
}

/// Disk-backed project portion of [`load`]: composes the base preamble before a
/// readable kind variant in `root`. Missing or unreadable variants fall back to
/// the base; a readable variant still degrades gracefully if the base becomes
/// unreadable. `None` only when every applicable file read fails.
fn load_disk_from_root(root: &Path, kind: SessionKind) -> Option<String> {
    let base_path = root.join(SHARED_DIR).join(BASE_FILENAME);
    let base = match read_project_file(root, &base_path) {
        Ok(content) => Some(content),
        Err(e) => {
            tracing::warn!(
                path = %base_path.display(),
                error = %e,
                "Base preamble read failed; continuing with any readable kind variant"
            );
            None
        }
    };

    let variant = variant_filename(kind).and_then(|filename| {
        let path = root.join(SHARED_DIR).join(filename);
        match read_project_file(root, &path) {
            Ok(content) => Some(content),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(
                    path = %path.display(),
                    kind = ?kind,
                    "Variant preamble missing; falling back to base"
                );
                None
            }
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "Variant preamble read error; falling back to base"
                );
                None
            }
        }
    });

    match (base, variant) {
        (Some(base), Some(variant)) => Some(format!("{base}\n\n{variant}")),
        (Some(base), None) => Some(base),
        (None, Some(variant)) => Some(variant),
        (None, None) => None,
    }
}

/// Load the project's orchestration router skill body, the "mouth of the
/// pipeline" frame placed first in a leaf-kind session's system prompt.
///
/// Read from `<project>/.claude/commands/orchestration_router.md` in the
/// session's own tree. Returns `None` when the project has no router (there is
/// no fallback to the rsi repo's router) or the read fails; launch is never
/// aborted by router-loading failure.
///
/// Container kinds (Group/Epic) are gated out before this loader runs and
/// therefore never receive the router.
pub fn load_orchestration_router(project_dir: &Path) -> Option<String> {
    let root = project_commands_root(project_dir)?;
    let path = root.join(COMMANDS_DIR).join(ORCHESTRATION_ROUTER_FILENAME);
    match read_project_file(&root, &path) {
        Ok(content) => Some(content),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!(
                path = %path.display(),
                "Orchestration router skill missing; router disabled for this launch"
            );
            None
        }
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "Orchestration router read error; router disabled for this launch"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::harness_manager_v2::ManagerActionKindV2;

    /// The rsi repository checkout this crate is built from: a session whose
    /// tree is this directory is a session "in the rsi repo".
    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("repo root")
    }

    fn worker_projection() -> AgentAuthorityProjection {
        AgentAuthorityProjection {
            revision: "sha256:worker-revision".into(),
            pending: false,
            is_lead: false,
            is_manager: false,
            is_reviewer: false,
            is_global_manager: false,
            verbs: vec![Verb::GetStatus, Verb::CreateIssue],
            update_variants: Vec::new(),
            control_actions: Vec::new(),
            prepared_actions: Vec::new(),
            delegated_operator_methods: Vec::new(),
            guidance_ids: vec!["common", "worker"],
        }
    }

    fn all_roles_projection() -> AgentAuthorityProjection {
        AgentAuthorityProjection {
            revision: "sha256:all-roles".into(),
            pending: false,
            is_lead: true,
            is_manager: true,
            is_reviewer: true,
            is_global_manager: false,
            verbs: rsi_common::agent_control_schema::agent_control_catalog_v1()
                .iter()
                .map(|descriptor| descriptor.verb)
                .collect(),
            update_variants: vec!["stage", "handoff"],
            control_actions: vec![ManagerActionKindV2::CreateSession],
            prepared_actions: vec!["resume_lead"],
            delegated_operator_methods: vec!["GetSession"],
            guidance_ids: vec![
                "common",
                "worker",
                "epic_lead",
                "manager",
                "assigned_reviewer",
            ],
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn authority_catalog_lists_only_projected_worker_controls() {
        let caller = uuid::Uuid::new_v4();
        let mut projection = worker_projection();
        projection.verbs.insert(0, Verb::GetAuthorityCatalog);
        let catalog = render_authority_catalog(caller, &projection, None).unwrap();
        assert_eq!(catalog.session_id, caller);
        assert_eq!(catalog.authority_revision, "sha256:worker-revision");
        assert!(!catalog.pending);
        assert_eq!(catalog.roles, ["worker"]);
        assert_eq!(
            catalog
                .controls
                .iter()
                .map(|control| control.method.as_str())
                .collect::<Vec<_>>(),
            [
                "AgentGetAuthorityCatalog",
                "AgentGetStatus",
                "AgentCreateIssue"
            ]
        );
        assert_eq!(
            catalog.controls[1].native_tool.as_deref(),
            Some("rsi_control_status")
        );
        assert!(
            catalog
                .guidance
                .starts_with("## RSI authority and transport")
        );
        assert!(catalog.guidance.contains("## RSI worker baseline"));
        assert!(catalog.manager_update_variants.is_empty());
        assert!(catalog.control.is_none());
        // A requested control carries its schema and whether this snapshot
        // lists it: a worker may read AgentSpawnChild's shape but is told it
        // is not one of its controls.
        let detail = render_authority_catalog(caller, &projection, Some(Verb::SpawnChild))
            .unwrap()
            .control
            .expect("requested control detail");
        assert_eq!(detail.method, "AgentSpawnChild");
        assert!(!detail.permitted);
        assert_eq!(
            detail.parameters,
            Verb::SpawnChild.descriptor().parameters()
        );
        let permitted = render_authority_catalog(caller, &projection, Some(Verb::GetStatus))
            .unwrap()
            .control
            .expect("requested control detail");
        assert!(permitted.permitted);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn authority_catalog_verb_response_is_the_compact_envelope_and_one_detail() {
        let caller = uuid::Uuid::new_v4();
        let projection = all_roles_projection();
        let full = render_authority_catalog(caller, &projection, None).unwrap();
        assert!(!full.controls.is_empty() && !full.guidance.is_empty());

        let compact =
            render_authority_catalog(caller, &projection, Some(Verb::ContinueChild)).unwrap();
        assert_eq!(compact.session_id, caller);
        assert_eq!(compact.authority_revision, full.authority_revision);
        assert_eq!(compact.roles, full.roles);
        assert!(!compact.pending);
        let json = serde_json::to_value(&compact).unwrap();
        let mut keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "authority_revision",
                "control",
                "pending",
                "roles",
                "schema_version",
                "session_id"
            ],
            "a verb request carries no controls list, guidance or manager lists"
        );
        let detail = &json["control"];
        assert_eq!(detail["method"], "AgentContinueChild");
        assert_eq!(detail["permitted"], true);
        assert_eq!(detail["example"], Verb::ContinueChild.example());
        let codes: Vec<&str> = detail["refusals"]
            .as_array()
            .unwrap()
            .iter()
            .map(|refusal| refusal["code"].as_str().unwrap())
            .collect();
        assert!(codes.contains(&"agent_continue_stale_cursor"));
        assert!(detail["refusals"][0]["next_action"].as_str().unwrap().len() > 1);
        // The verb response is far smaller than the manual it replaces.
        assert!(
            serde_json::to_string(&json).unwrap().len() * 4
                < serde_json::to_string(&full).unwrap().len()
        );
    }

    /// Operating rules that used to live only in the rsi-agent-control skill
    /// (#1055): the catalog guidance now carries them, so a session outside
    /// this repository still gets them.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn catalog_guidance_pins_the_mechanics_moved_out_of_the_skill() {
        let guidance =
            render_authority_catalog(uuid::Uuid::new_v4(), &all_roles_projection(), None)
                .unwrap()
                .guidance;
        for phrase in [
            // worker baseline: continuation fence and wake discipline
            "never send `expected_row_version` for a session",
            "not an idempotency key",
            "`git diff <base_commit>..<child-ref>`",
            "Continue yourself only through `AgentScheduleWake` with `mode:\"resume\"`",
            "at most one same-session `mode:\"resume\"` wake of at most 3600 s",
            "never `fresh` on your own session",
            // bound worker: the binding limits only AgentUpdateIssue (#1602)
            "it never restricts `AgentCreateIssue`",
            "explicit job or wake template",
            // epic lead: topology policy and Issue lifecycle
            "`policy_refused`",
            "archive is terminal-only and restore keeps the terminal status",
            // manager: reach, deploy, landing queue, succession
            "`manager_v2_leaf_required`",
            "you cannot halt an Epic lead",
            "before `succeed_manager`, `pause_lead` or lead replacement",
            "`AgentRequestDeploy`",
            "`AgentEnqueueLandingSource`",
            "Batch waits, never landings",
            "scripts/check-touched-shards --base origin/rolling",
            "there are no hot-file claims or seals",
            "`topology_bulk_fanout_min_openrouter`",
            // manager: runaway runs and the load rule (#1337)
            "Runaway runs (#1337), every wake: read the top CPU consumers",
            "`runaway_process` notice",
            "over about 20 minutes, or one that dominates a host load over 40",
            "`AgentHalt`, then `AgentContinueChild` with a scoped instruction",
            "Report every run you stopped in your handoff",
            "`job_timed_out`",
            "check `uptime` before you launch build or test work",
            "queue it while the 1-minute load is above 40",
            "at most 5 concurrent build-heavy sessions per project",
        ] {
            assert!(guidance.contains(phrase), "guidance lost: {phrase}");
        }
    }

    /// #1337: the portfolio manager's guidance carries the same runaway-run
    /// duty and load rule for the projects it manages.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn portfolio_guidance_pins_the_runaway_run_duty_and_load_rule() {
        let mut projection = worker_projection();
        projection.is_global_manager = true;
        projection.guidance_ids = vec!["common", "worker", "global_manager", "portfolio_manager"];
        let guidance = render_authority_catalog(uuid::Uuid::new_v4(), &projection, None)
            .unwrap()
            .guidance;
        for phrase in [
            "## Portfolio manager",
            "Runaway runs (#1337): on every wake read the top CPU consumers",
            "over about 20 minutes, or one that dominates a host load over 40",
            "(`AgentHalt`, then `AgentContinueChild` with a scoped instruction)",
            "check `uptime` before launching build or test work",
            "queue it while the 1-minute load is above 40",
            "at most 5 concurrent build-heavy sessions per project",
            "Report every run stopped in your report up and handoff",
        ] {
            assert!(
                guidance.contains(phrase),
                "portfolio guidance lost: {phrase}"
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn authority_catalog_serves_each_committed_role() {
        let catalog =
            render_authority_catalog(uuid::Uuid::new_v4(), &all_roles_projection(), None).unwrap();
        assert_eq!(
            catalog.roles,
            ["worker", "epic_lead", "manager", "assigned_reviewer"]
        );
        for section in [
            "## RSI authority and transport",
            "## RSI worker baseline",
            "## Current Epic lead",
            "## Appointed harness manager",
            "## Active assigned reviewer",
        ] {
            assert!(catalog.guidance.contains(section), "missing {section}");
        }
        assert_eq!(
            catalog.controls.len(),
            rsi_common::agent_control_schema::agent_control_catalog_v1().len()
        );
        assert_eq!(catalog.manager_update_variants, ["stage", "handoff"]);
        assert_eq!(catalog.manager_control_actions, ["create_session"]);
        assert_eq!(catalog.manager_prepared_actions, ["resume_lead"]);
        assert_eq!(catalog.delegated_operator_methods, ["GetSession"]);
        let json = serde_json::to_value(&catalog).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert!(json.get("control").is_none());

        // #1332: every role's catalog carries the improvement duty and the
        // Friction field, plus its own kaizen duty.
        let single_role = |role: &'static str| {
            let mut projection = worker_projection();
            match role {
                "epic_lead" => projection.is_lead = true,
                "manager" => projection.is_manager = true,
                "assigned_reviewer" => projection.is_reviewer = true,
                _ => {}
            }
            if role != "worker" {
                projection.guidance_ids.push(role);
            }
            render_authority_catalog(uuid::Uuid::new_v4(), &projection, None).unwrap()
        };
        for (role, duty) in [
            ("worker", "End every handoff with `Friction:"),
            (
                "assigned_reviewer",
                "file process findings about the review itself",
            ),
            ("epic_lead", "Kaizen triage for your Epic"),
            (
                "manager",
                "keep at least one worker on the highest-value open kaizen",
            ),
        ] {
            let catalog = single_role(role);
            assert_eq!(catalog.roles.last().map(String::as_str), Some(role));
            for phrase in [
                "## Improve the line",
                "file one kaizen Issue, then keep working",
                "`AgentCreateIssue` with label `kaizen`",
                "search with `title_contains` first",
                "`Friction: none | #N[, #M] | <one line, not filed because ...>`",
                duty,
            ] {
                assert!(catalog.guidance.contains(phrase), "{role} lacks {phrase}");
            }
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn pending_authority_catalog_keeps_worker_baseline_and_says_refresh() {
        let mut projection = all_roles_projection();
        projection.pending = true;
        let catalog = render_authority_catalog(uuid::Uuid::new_v4(), &projection, None).unwrap();
        assert!(catalog.pending);
        assert_eq!(catalog.roles, ["worker"]);
        assert!(
            catalog
                .guidance
                .contains("Authority publication is pending")
        );
        assert!(
            catalog
                .guidance
                .contains("call `AgentGetAuthorityCatalog` again shortly")
        );
        let methods = catalog
            .controls
            .iter()
            .map(|control| control.method.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            methods,
            rsi_common::agent_control_schema::agent_control_catalog_v1()
                .iter()
                .filter(|descriptor| is_baseline_verb(descriptor.verb))
                .map(|descriptor| descriptor.method)
                .collect::<Vec<_>>()
        );
        assert!(methods.contains(&"AgentGetAuthorityCatalog"));
        assert!(methods.contains(&"AgentSubmitJob"));
        assert!(catalog.manager_control_actions.is_empty());
        assert!(catalog.delegated_operator_methods.is_empty());

        projection.guidance_ids = vec!["common", "worker"];
        assert!(
            render_authority_catalog(uuid::Uuid::new_v4(), &projection, None)
                .unwrap_err()
                .to_string()
                .contains("inconsistent agent guidance projection")
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn versioned_guidance_embeds_kind_contract_and_worker_catalog() {
        let guidance =
            render_versioned_guidance(SessionKind::Feature, &worker_projection()).unwrap();
        assert!(guidance.contains("RSI embedded guidance v1"));
        assert!(guidance.contains("sha256:worker-revision"));
        assert!(guidance.contains("# Orchestration Router"));
        assert!(guidance.contains("# Worker preamble — Feature"));
        assert!(guidance.contains("### Inputs"));
        assert!(guidance.contains("### Verify"));
        assert!(guidance.contains("## RSI worker baseline"));
        assert!(guidance.contains("`AgentGetStatus` / `rsi_control_status`"));
        assert!(guidance.contains("`AgentCreateIssue` / `rsi_control_create_issue`"));
        assert!(!guidance.contains("`AgentManagerControl`"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn versioned_guidance_lists_only_projected_manager_variants() {
        let mut projection = worker_projection();
        projection.is_manager = true;
        projection.guidance_ids.push("manager");
        projection
            .verbs
            .extend([Verb::ManagerUpdate, Verb::ManagerControl]);
        projection.update_variants.push("stage");
        projection
            .control_actions
            .push(ManagerActionKindV2::CreateSession);
        let guidance = render_versioned_guidance(SessionKind::Standard, &projection).unwrap();
        assert!(guidance.contains("## Appointed harness manager"));
        assert!(guidance.contains("`AgentManagerUpdate` / `rsi_control_manager_update`"));
        assert!(guidance.contains("AgentManagerUpdate variants: stage."));
        assert!(guidance.contains("AgentManagerControl actions: create_session."));
        assert!(!guidance.contains("AgentManagerPrepareControl actions:"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn versioned_guidance_embeds_committed_lead_and_active_reviewer_roles() {
        let mut projection = worker_projection();
        projection.is_lead = true;
        projection.is_reviewer = true;
        projection
            .guidance_ids
            .extend(["epic_lead", "assigned_reviewer"]);
        projection
            .verbs
            .extend([Verb::SpawnChild, Verb::SubmitReviewReceipt]);
        let guidance = render_versioned_guidance(SessionKind::Research, &projection).unwrap();
        assert!(guidance.contains("# Worker preamble — Research"));
        assert!(guidance.contains("## Current Epic lead"));
        assert!(guidance.contains("## Active assigned reviewer"));
        assert!(guidance.contains("never set a reviewer target directory under `/tmp`"));
        assert!(guidance.contains("`AgentSpawnChild` / `rsi_control_spawn`"));
        assert!(
            guidance.contains("`AgentSubmitReviewReceipt` / `rsi_control_submit_review_receipt`")
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn pending_publication_renders_positive_worker_baseline() {
        let mut projection = worker_projection();
        projection.pending = true;
        projection.is_manager = true;
        projection.guidance_ids.push("manager");
        projection.verbs.push(Verb::ManagerControl);
        projection
            .control_actions
            .push(ManagerActionKindV2::CreateSession);
        let guidance = render_versioned_guidance(SessionKind::Standard, &projection).unwrap();
        assert!(guidance.contains("Authority publication is pending"));
        assert!(guidance.contains("## RSI worker baseline"));
        assert!(guidance.contains("`AgentGetStatus` / `rsi_control_status`"));
        assert!(!guidance.contains("## Appointed harness manager"));
        assert!(!guidance.contains("`AgentManagerControl`"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn harness_root_falls_back_to_the_build_repo_when_cwd_and_exe_are_elsewhere() {
        let build = tempfile::tempdir().expect("build repo");
        let shared = build.path().join(SHARED_DIR);
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(shared.join(BASE_FILENAME), "BASE").unwrap();
        let crate_dir = build.path().join("crates/rsid");
        std::fs::create_dir_all(&crate_dir).unwrap();
        // Fixed paths: the test TMPDIR may itself sit inside a repo checkout.
        let cwd = Path::new("/rsi-test-no-such-daemon-cwd");
        let install = Path::new("/rsi-test-no-such-install");
        let found = discover_harness_root_from(
            Some(Path::new("/rsi-test-no-such-env-root")),
            Some(cwd),
            Some(install),
            &crate_dir,
            None,
        );
        assert_eq!(found.as_deref(), Some(build.path()));
        // An operator-set root still wins over every fallback.
        let operator = tempfile::tempdir().expect("operator root");
        let op_shared = operator.path().join(SHARED_DIR);
        std::fs::create_dir_all(&op_shared).unwrap();
        std::fs::write(op_shared.join(BASE_FILENAME), "BASE").unwrap();
        assert_eq!(
            discover_harness_root_from(Some(operator.path()), Some(cwd), None, &crate_dir, None)
                .as_deref(),
            Some(operator.path())
        );
        // Nothing discoverable stays unresolved (typed refusal downstream).
        assert_eq!(
            discover_harness_root_from(None, Some(cwd), None, install, None),
            None
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn harness_root_resolves_a_linked_worktree_build_dir_to_its_main_worktree() {
        let tmp = tempfile::tempdir().expect("fixture root");
        let (main, build_dir) = linked_worktree_fixture(tmp.path());
        let cwd = Path::new("/rsi-test-no-such-daemon-cwd");
        // Runtime resolution from the sandbox build dir alone.
        assert_eq!(
            discover_harness_root_from(None, Some(cwd), None, &build_dir, None).as_deref(),
            Some(main.as_path())
        );
        // The build-time embedded main worktree wins, and survives the
        // sandbox being reclaimed.
        std::fs::remove_dir_all(tmp.path().join("sandbox")).unwrap();
        assert_eq!(
            discover_harness_root_from(None, Some(cwd), None, &build_dir, Some(&main)).as_deref(),
            Some(main.as_path())
        );
        // A plain (non-linked) checkout keeps its own root.
        assert_eq!(main_worktree_of(&main).as_deref(), Some(main.as_path()));
        // Outside any repo nothing resolves.
        assert_eq!(main_worktree_of(Path::new("/rsi-test-no-such-dir")), None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn incomplete_or_unknown_guidance_projection_fails_visibly() {
        let mut projection = worker_projection();
        projection.guidance_ids = vec!["common"];
        assert!(
            render_versioned_guidance(SessionKind::Task, &projection)
                .unwrap_err()
                .to_string()
                .contains("inconsistent agent guidance projection")
        );
        projection.guidance_ids = vec!["common", "worker", "unknown_role"];
        assert!(
            render_versioned_guidance(SessionKind::Task, &projection)
                .unwrap_err()
                .to_string()
                .contains("inconsistent agent guidance projection")
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn disk_loader_composes_base_before_variant_and_preserves_variant_only_fallback() {
        let root = tempfile::tempdir().expect("temporary harness root");
        let shared = root.path().join(SHARED_DIR);
        std::fs::create_dir_all(&shared).expect("create shared command directory");
        let base = shared.join(BASE_FILENAME);
        let variant = shared.join("worker_preamble_bug.md");
        std::fs::write(&base, "BASE CONTRACT").expect("write base preamble");
        std::fs::write(&variant, "BUG VARIANT").expect("write bug preamble");

        assert_eq!(
            load_disk_from_root(root.path(), SessionKind::Bug).as_deref(),
            Some("BASE CONTRACT\n\nBUG VARIANT")
        );

        std::fs::remove_file(&base).expect("remove base after root establishment");
        assert_eq!(
            load_disk_from_root(root.path(), SessionKind::Bug).as_deref(),
            Some("BUG VARIANT"),
            "a readable variant remains a graceful fallback after a base read failure"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn project_reads_omit_symlink_escapes_and_follow_inside_links() {
        let outside = tempfile::tempdir().expect("outside tree");
        std::fs::write(outside.path().join("other_base.md"), "OTHER PROJECT BASE").expect("other");
        std::fs::write(outside.path().join("other_router.md"), "OTHER ROUTER").expect("other");

        let project = tempfile::tempdir().expect("project");
        let commands = project.path().join(COMMANDS_DIR);
        let shared = commands.join("_shared");
        std::fs::create_dir_all(&shared).expect("shared dir");
        std::fs::write(project.path().join("real_variant.md"), "INSIDE VARIANT").expect("real");
        std::os::unix::fs::symlink(
            outside.path().join("other_base.md"),
            shared.join(BASE_FILENAME),
        )
        .expect("escaping base link");
        std::os::unix::fs::symlink(
            outside.path().join("other_router.md"),
            commands.join(ORCHESTRATION_ROUTER_FILENAME),
        )
        .expect("escaping router link");
        std::os::unix::fs::symlink(
            project.path().join("real_variant.md"),
            shared.join("worker_preamble_bug.md"),
        )
        .expect("inside variant link");

        // Escaping base is omitted; the in-tree symlinked variant is followed.
        assert_eq!(
            load_disk_from_root(project.path(), SessionKind::Bug).as_deref(),
            Some("INSIDE VARIANT")
        );
        // Escaping router is omitted.
        assert_eq!(load_orchestration_router(project.path()), None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn project_reads_omit_dotdot_paths_that_leave_the_root() {
        let parent = tempfile::tempdir().expect("parent");
        std::fs::write(parent.path().join("secret.md"), "SECRET").expect("secret");
        let root = parent.path().join("project");
        std::fs::create_dir_all(&root).expect("root");
        std::fs::write(root.join("inside.md"), "INSIDE").expect("inside");

        let escaping = root.join("sub/../../secret.md");
        std::fs::create_dir_all(root.join("sub")).expect("sub");
        let err = read_project_file(&root, &escaping).expect_err("`..` escape must not read");
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        // A `..` that stays inside the root is fine.
        assert_eq!(
            read_project_file(&root, &root.join("sub/../inside.md"))
                .as_deref()
                .ok(),
            Some("INSIDE")
        );
        let missing = read_project_file(&root, &root.join("nope.md")).expect_err("missing");
        assert_eq!(missing.kind(), std::io::ErrorKind::NotFound);
    }

    /// A directory tree that is not the rsi repo: a bare project with a
    /// `.git` marker and, optionally, nothing else.
    fn bare_project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp project");
        std::fs::create_dir(dir.path().join(".git")).expect("git marker");
        dir
    }

    /// Path-like tokens (`dir/file.ext` or `dir/`) in `text` that name rsi-repo
    /// files, for checking a composed prompt against a tree.
    fn repo_path_references(text: &str) -> Vec<String> {
        let mut out = Vec::new();
        for token in text.split(|c: char| {
            c.is_whitespace() || matches!(c, '`' | '(' | ')' | ',' | ';' | '"' | '\'' | '<' | '>')
        }) {
            let token = token.trim_end_matches(['.', ':']);
            if [
                "AGENTS.md",
                "docs/",
                ".agents/",
                ".claude/",
                "thoughts/",
                "scripts/",
                "crates/",
            ]
            .iter()
            .any(|needle| token.contains(needle))
            {
                out.push(token.to_string());
            }
        }
        out
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn project_without_claude_dir_gets_no_reference_to_missing_paths() {
        let project = bare_project();
        for kind in [
            SessionKind::Standard,
            SessionKind::Task,
            SessionKind::Bug,
            SessionKind::Feature,
            SessionKind::Research,
        ] {
            let text = load(kind, project.path()).expect("generic part always loads");
            assert!(text.contains(agent_discovery_nudge()));
            assert!(text.contains(RSI_BACKEND_POLICY));
            assert!(text.contains(DAEMON_MESSAGE_CONVENTION));
            assert_eq!(text, load_generic(Some(project.path())));
            for reference in repo_path_references(&text) {
                assert!(
                    project.path().join(&reference).exists(),
                    "{kind:?} startup text names `{reference}`, which the project tree lacks"
                );
            }
            assert_eq!(load_project_preamble(project.path(), kind), None);
        }
        assert_eq!(load_orchestration_router(project.path()), None);
        // No `thoughts/` directory, so no thoughts commit policy either.
        assert!(!load_generic(Some(project.path())).contains(THOUGHTS_COMMIT_POLICY));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn project_files_are_read_from_the_session_tree_not_the_daemon_checkout() {
        let project = bare_project();
        let commands = project.path().join(COMMANDS_DIR);
        std::fs::create_dir_all(commands.join("_shared")).expect("commands dir");
        std::fs::write(commands.join("_shared/worker_preamble.md"), "PROJECT BASE").expect("base");
        std::fs::write(
            commands.join("_shared/worker_preamble_bug.md"),
            "PROJECT BUG",
        )
        .expect("variant");
        std::fs::write(commands.join("orchestration_router.md"), "PROJECT ROUTER").expect("router");
        std::fs::create_dir(project.path().join("thoughts")).expect("thoughts dir");

        let bug = load(SessionKind::Bug, project.path()).expect("loads");
        assert!(bug.starts_with("PROJECT BASE\n\nPROJECT BUG\n\n"));
        assert_eq!(
            bug,
            format!(
                "PROJECT BASE\n\nPROJECT BUG\n\n{}",
                load_generic(Some(project.path()))
            )
        );
        assert!(bug.contains(THOUGHTS_COMMIT_POLICY));
        assert!(!bug.contains("# RPI Worker Preamble"));
        assert_eq!(
            load(SessionKind::Task, project.path()).map(|t| t.starts_with("PROJECT BASE\n\n")),
            Some(true)
        );
        assert_eq!(
            load_orchestration_router(project.path()).as_deref(),
            Some("PROJECT ROUTER")
        );

        // A session started in a subdirectory of the repo resolves the same files.
        let sub = project.path().join("crates/inner");
        std::fs::create_dir_all(&sub).expect("subdir");
        assert_eq!(load(SessionKind::Bug, &sub), Some(bug));
        assert_eq!(
            load_orchestration_router(&sub).as_deref(),
            Some("PROJECT ROUTER")
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn project_search_never_climbs_past_the_repository() {
        // Parent has the files but is not the session's repository.
        let parent = tempfile::tempdir().expect("parent");
        let commands = parent.path().join(COMMANDS_DIR);
        std::fs::create_dir_all(commands.join("_shared")).expect("commands dir");
        std::fs::write(commands.join("_shared/worker_preamble.md"), "PARENT BASE").expect("base");
        std::fs::write(commands.join("orchestration_router.md"), "PARENT ROUTER").expect("router");

        // Child repo (has .git) under the parent: the parent is out of bounds.
        let repo = parent.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).expect("repo");
        assert_eq!(load_project_preamble(&repo, SessionKind::Task), None);
        assert_eq!(load_orchestration_router(&repo), None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn rsi_repo_composition_is_the_legacy_router_base_variant_then_generic() {
        let root = repo_root();
        let read = |rel: &str| std::fs::read_to_string(root.join(rel)).expect(rel);
        let router = read(".claude/commands/orchestration_router.md");
        let base = read(".claude/commands/_shared/worker_preamble.md");
        let legacy_embedded = format!(
            "{}\n\n{THOUGHTS_COMMIT_POLICY}\n\n{RSI_BACKEND_POLICY}\n\n{DAEMON_MESSAGE_CONVENTION}",
            agent_discovery_nudge()
        );
        assert_eq!(
            load_orchestration_router(&root).as_deref(),
            Some(router.as_str())
        );
        for (kind, variant) in [
            (SessionKind::Standard, None),
            (SessionKind::Task, None),
            (SessionKind::Bug, Some("bug")),
            (SessionKind::Feature, Some("feature")),
            (SessionKind::Refactor, Some("refactor")),
            (SessionKind::Research, Some("research")),
        ] {
            let disk = match variant {
                Some(name) => format!(
                    "{base}\n\n{}",
                    read(&format!(
                        ".claude/commands/_shared/worker_preamble_{name}.md"
                    ))
                ),
                None => base.clone(),
            };
            assert_eq!(
                load(kind, &root),
                Some(format!("{disk}\n\n{legacy_embedded}")),
                "rsi-repo composition changed for {kind:?}"
            );
        }
        // A sandbox-style subdirectory of the repo resolves the same text.
        assert_eq!(
            load(SessionKind::Bug, &root.join("crates/rsid")),
            load(SessionKind::Bug, &root)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn loaded_preamble_carries_agent_discovery_nudge() {
        // The launch path (`session::launch`) pushes `load(kind)` into the
        // system-prompt parts for every launch-system-prompt provider, so the
        // nudge riding `load()` reaches Claude/Local/Antigravity/Harness/
        // CodexAppServer alike.
        for kind in [
            SessionKind::Standard,
            SessionKind::Task,
            SessionKind::Feature,
            SessionKind::Bug,
            SessionKind::Research,
        ] {
            let preamble =
                load(kind, &repo_root()).expect("load() always returns the embedded nudge");
            assert!(
                preamble.contains(agent_discovery_nudge()),
                "kind {kind:?} preamble must carry the agent-discovery nudge"
            );
            assert!(
                preamble.contains(RSI_BACKEND_POLICY),
                "kind {kind:?} preamble must carry the RSI backend policy"
            );
            assert!(
                preamble.contains(THOUGHTS_COMMIT_POLICY),
                "kind {kind:?} preamble must carry the thoughts commit policy"
            );
            assert!(
                preamble.contains(DAEMON_MESSAGE_CONVENTION),
                "kind {kind:?} preamble must carry the daemon message convention"
            );
        }
    }

    /// Every CLI the embedded startup text tells agents to run must be built
    /// and linked onto PATH by `make release-install`.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn every_cli_named_in_startup_guidance_is_installed() {
        let install = include_str!("../../../../scripts/install-release.sh");
        let startup = format!("{EMBEDDED_BASE}\n{}", agent_discovery_nudge());
        for cli in ["rsi-rpc", "rsi-agent-mcp", "rsi-contract-validate"] {
            if cli != "rsi-agent-mcp" {
                assert!(startup.contains(cli), "startup text names {cli}");
            }
            assert!(
                install.contains(&format!("--bin {cli}")),
                "install-release.sh builds {cli}"
            );
            assert!(
                install.contains(&format!("\"$BIN_DIR/{cli}\"")),
                "install-release.sh links {cli} onto PATH"
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn daemon_message_convention_names_shared_tag() {
        // The convention text and the rsi-common envelope helper must agree
        // on the tag name — the model is taught the exact tag the injection
        // sites emit.
        assert!(
            DAEMON_MESSAGE_CONVENTION.contains(rsi_common::daemon_message::DAEMON_MESSAGE_TAG),
            "convention must name the shared envelope tag"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn sandbox_custody_instruction_names_the_authenticated_root_and_branch() {
        let root = Path::new("/tmp/rsi-sandboxes/43ae018b");
        let instruction = sandbox_custody_instruction(root, "rsi/43ae018b");

        assert!(instruction.contains("/tmp/rsi-sandboxes/43ae018b"));
        assert!(instruction.contains("rsi/43ae018b"));
        assert!(instruction.contains("RSI-owned"));
        assert!(instruction.contains("git checkout"));
        assert!(instruction.contains("git switch"));
        assert!(instruction.contains("creates or changes a branch"));
        assert!(instruction.contains("Commit, push, and open a PR"));
        assert!(instruction.contains("request a session allocated on that branch"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn sandbox_custody_instruction_prepends_without_changing_ordinary_prompts() {
        let caller_prompt = Some("caller instructions stay intact".to_string());
        assert_eq!(
            prepend_sandbox_custody_instruction(caller_prompt.clone(), None),
            caller_prompt,
            "ordinary launches must not receive a sandbox warning"
        );

        let instruction = sandbox_custody_instruction(Path::new("/tmp/sandbox"), "rsi/test");
        let combined = prepend_sandbox_custody_instruction(
            Some("caller instructions stay intact".to_string()),
            Some(instruction.clone()),
        )
        .expect("sandbox launch has an instruction");
        assert!(combined.starts_with(&instruction));
        assert!(combined.ends_with("caller instructions stay intact"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn rotation_preamble_path_keeps_nudge() {
        // Context rotation (`session::rotation`) rebuilds the child system
        // prompt from `preamble::load(child.session_kind)` — the same loader —
        // so a rotated session still carries the nudge (rotation-parity).
        let rotated =
            load(SessionKind::Task, &repo_root()).expect("rotation loader returns the nudge");
        assert!(
            rotated.contains(agent_discovery_nudge()),
            "rotated session preamble must retain the agent-discovery nudge"
        );
        assert!(
            rotated.contains(RSI_BACKEND_POLICY),
            "rotated session preamble must retain the RSI backend policy"
        );
        assert!(
            rotated.contains(THOUGHTS_COMMIT_POLICY),
            "rotated session preamble must retain the thoughts commit policy"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn agent_discovery_nudge_sends_agents_to_the_authority_catalog() {
        // The startup frame is short and role-independent: it names the one
        // verb that serves role guidance and the permitted controls, plus the
        // tokened rsi-rpc convention.
        let nudge = agent_discovery_nudge();
        for required in [
            "Start here: call `AgentGetAuthorityCatalog` with `{}`",
            "rsi_control_authority_catalog",
            "mcp__rsi-agent__rsi_control_authority_catalog",
            "rsi-rpc AgentGetAuthorityCatalog",
            "{\"verb\": \"<name>\"}",
            "RSI_SESSION_TOKEN",
            "`mode` is required",
            "same-session continuation",
            "never `fresh` on your own session",
        ] {
            assert!(nudge.contains(required), "nudge must carry `{required}`");
        }
        assert!(
            rsi_common::agent_control_schema::AgentControlVerbV1::from_method_name(
                "AgentGetAuthorityCatalog"
            )
            .is_some(),
            "the frame's entry point must be a catalog verb"
        );
        // The generic passthrough / read verbs are never advertised to agents.
        for forbidden in [
            "GetHealthStatus",
            "ListSessions",
            "LaunchSession",
            "GetHarnessManager",
            "ConfigureHarnessManager",
            "GetHarnessManagerPolicy",
            "ConfigureHarnessManagerPolicy",
            "GetHarnessManagerState",
            "AnswerHarnessManagerDecision",
        ] {
            assert!(
                !agent_discovery_nudge().contains(forbidden),
                "nudge must not advertise generic surface `{forbidden}`"
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn manager_preamble_preserves_operator_authority_and_inbox_semantics() {
        // The startup frame keeps the role-independent operator rules; the
        // manager, lead, managed-worker and reviewer rules now ride the
        // authority catalog for exactly those roles.
        let projection = all_roles_projection();
        let catalog = render_authority_catalog(uuid::Uuid::new_v4(), &projection, None).unwrap();
        let native = catalog
            .controls
            .iter()
            .filter_map(|control| control.native_tool.as_deref())
            .collect::<Vec<_>>()
            .join(" ");
        let combined = format!(
            "{}\n{}\n{native}",
            agent_discovery_nudge(),
            catalog.guidance
        );
        for requirement in [
            "scope changes are operator-only",
            "existing child-control permissions stay unchanged",
            "rsi_control_manager_progress",
            "rsi_control_manager_inbox",
            "rsi_control_manager_send",
            "rsi_control_manager_reply",
            "rsi_control_manager_notify",
            "Send unsolicited status or blockers with `AgentManagerNotify`",
            "the daemon derives your Epic and manager",
            "A notice is never a request, approval or acceptance",
            "rsi_control_manager_inspect",
            "rsi_control_manager_update",
            "rsi_control_submit_review_receipt",
            "rsi_control_manager_control",
            "rsi_control_manager_prepare_control",
            "rsi_control_manager_commit_prepared_control",
            "rsi_control_manager_get_action",
            "rsi_control_manager_work_view",
            "rsi_control_topology_upsert",
            "Only the current manager holding `Automation`",
            "an Epic lead (its own Epic; never `discard`)",
            "Granted ownership there is authoritative",
            "V2 policy is a separate operator opt-in",
            "unknown evidence stays unknown",
            "operator answers consolidated decisions",
            "Reply explicitly",
            "evidence or a blocker",
            "Send receipts mean queued",
            "an explicit reply means replied",
            "Inbox retrieval is a durable tool read",
            "once the sender or recipient is idle",
            "Direct operator instructions take precedence",
            "Human approvals remain operator-owned",
        ] {
            assert!(
                combined.contains(requirement),
                "missing manager instruction: {requirement}"
            );
        }
        for requirement in [
            "optional `provider`",
            "different backend",
            "The assigned reviewer answers with a receipt, not an artifact",
            "Do not create a review artifact or evidence commit",
        ] {
            assert!(
                combined
                    .to_lowercase()
                    .contains(&requirement.to_lowercase()),
                "missing role instruction: {requirement}"
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn rsi_backend_policy_forbids_provider_native_delegation() {
        assert!(RSI_BACKEND_POLICY.contains("rsi_control_spawn"));
        assert!(RSI_BACKEND_POLICY.contains("rsi_control_reserve_successor"));
        assert!(RSI_BACKEND_POLICY.contains("AgentSpawnChild"));
        assert!(RSI_BACKEND_POLICY.contains("AgentReserveSuccessor"));
        assert!(RSI_BACKEND_POLICY.contains("<docregblock>/spawn_child"));
        assert!(RSI_BACKEND_POLICY.contains("provider-native"));
        assert!(RSI_BACKEND_POLICY.contains("compile the next worker prompt"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn variant_filename_covers_target_kinds() {
        assert_eq!(
            variant_filename(SessionKind::Bug),
            Some("worker_preamble_bug.md")
        );
        assert_eq!(
            variant_filename(SessionKind::Feature),
            Some("worker_preamble_feature.md")
        );
        assert_eq!(
            variant_filename(SessionKind::Refactor),
            Some("worker_preamble_refactor.md")
        );
        assert_eq!(
            variant_filename(SessionKind::Research),
            Some("worker_preamble_research.md")
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn variant_filename_none_for_kinds_using_base() {
        for k in [
            SessionKind::Standard,
            SessionKind::TaskRabbit,
            SessionKind::Story,
            SessionKind::Task,
            SessionKind::Group,
            SessionKind::Epic,
        ] {
            assert!(
                variant_filename(k).is_none(),
                "kind {:?} should fall through to base",
                k
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn walk_up_finds_repo_root_from_crate_dir() {
        // CARGO_MANIFEST_DIR is crates/rsid; repo root is two levels up.
        let crate_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let found = walk_up_for_claude(crate_dir);
        assert!(
            found.is_some(),
            "should walk up from {} to find .claude/",
            crate_dir.display()
        );
        let found = found.unwrap();
        assert!(found.join(SHARED_DIR).join(BASE_FILENAME).is_file());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn orchestration_router_constants_point_to_canonical_paths() {
        // Anchors the contract: router file lives one directory shallower
        // than the per-kind preambles. Bumping the path silently would
        // detach the loader from the on-disk skill.
        assert_eq!(COMMANDS_DIR, ".claude/commands");
        assert_eq!(ORCHESTRATION_ROUTER_FILENAME, "orchestration_router.md");
        // SHARED_DIR sits inside COMMANDS_DIR, never the other way around.
        assert!(SHARED_DIR.starts_with(COMMANDS_DIR));
    }

    /// #1532: a bound reviewer reads only its own review Issue, so the manager
    /// guidance must tell the launcher to put the implementer handoff there.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn manager_guidance_requires_handoff_in_bound_review_issue() {
        let guidance = role_guidance("manager").expect("manager guidance");
        assert!(guidance.contains(
            "put the implementer's handoff, the acceptance criteria and the exact source SHA in the review Issue body"
        ));
    }
}
