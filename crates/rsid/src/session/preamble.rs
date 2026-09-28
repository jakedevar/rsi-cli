//! Per-session-kind preamble loader.
//!
//! Resolves the preamble file for a given `SessionKind`, with graceful
//! fallback to the base file. Path discovery is cached for the daemon's
//! lifetime via `OnceLock` (no hot-reload — RSI-009 will add that).
//!
//! Discovery order for the harness root (the directory containing `.claude/`):
//!   1. `RSI_HARNESS_ROOT` env var (also accepts `MOTHERSHIP_HARNESS_ROOT`,
//!      `FLYWHEEL_HARNESS_ROOT` via `env_with_legacy`).
//!   2. Walk upward from `std::env::current_dir()` looking for `.claude/`.
//!   3. Walk upward from `std::env::current_exe()`'s parent looking for `.claude/`.
//!   4. `None` — logged once at warn level; preamble loading is silently disabled
//!      and launch continues without a preamble (graceful degradation).
//!
//! Read-failure modes (variant or base file):
//!   - `NotFound`: logged at debug; falls through to base (or returns `None` if base also missing).
//!   - other IO error: logged at warn; falls through to base (or returns `None`).
//!   - `None` is never an error — launch is never aborted by preamble loading.

use super::agent_verbs::agent_authority::AgentAuthorityProjection;
use crate::error::{DaemonError, Result};
use rsi_common::agent_control_schema::AgentControlVerbV1 as Verb;
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

/// A pending initial publication may advertise only the ordinary worker
/// controls, even if the caller's snapshot was concurrently superseded.
const PENDING_WORKER_VERBS: &[Verb] = &[
    Verb::GetProgress,
    Verb::SendMessage,
    Verb::GetStatus,
    Verb::Halt,
    Verb::ContinueChild,
    Verb::ScheduleWake,
    Verb::CreateIssue,
];

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
    if projection.guidance_ids != expected_ids {
        return Err(DaemonError::Store(
            "inconsistent agent guidance projection".into(),
        ));
    }

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
        sections.push("Authority publication is pending. Continue with the worker baseline and wait for a refreshed projection before using a role-specific control.".into());
    } else {
        for id in &projection.guidance_ids {
            let text = match *id {
                "epic_lead" => Some(EMBEDDED_EPIC_LEAD),
                "manager" => Some(EMBEDDED_MANAGER),
                "assigned_reviewer" => Some(EMBEDDED_REVIEWER),
                _ => None,
            };
            if let Some(text) = text {
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
    for verb in &projection.verbs {
        if projection.pending && !PENDING_WORKER_VERBS.contains(verb) {
            continue;
        }
        let entry = verb.descriptor();
        write!(catalog, "\n- `{}`", entry.method).expect("writing to a String cannot fail");
        if let Some(tool) = entry.native_tool {
            write!(catalog, " / `{}`", tool.name()).expect("writing to a String cannot fail");
        }
        write!(catalog, ": {}", entry.description).expect("writing to a String cannot fail");
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
            let actions = projection
                .control_actions
                .iter()
                .map(|action| {
                    serde_json::to_value(action)?
                        .as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| DaemonError::Store("invalid manager action name".into()))
                })
                .collect::<Result<Vec<_>>>()?;
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
    if let Ok(s) = rsi_common::identity::env_with_legacy(
        "RSI_HARNESS_ROOT",
        &["MOTHERSHIP_HARNESS_ROOT", "FLYWHEEL_HARNESS_ROOT"],
    ) {
        let p = PathBuf::from(s);
        if p.join(SHARED_DIR).join(BASE_FILENAME).is_file() {
            return Some(p);
        }
        tracing::warn!(
            root = %p.display(),
            "RSI_HARNESS_ROOT set but base preamble not found; falling through to discovery"
        );
    }
    if let Ok(cwd) = std::env::current_dir() {
        if let Some(found) = walk_up_for_claude(&cwd) {
            return Some(found);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            if let Some(found) = walk_up_for_claude(parent) {
                return Some(found);
            }
        }
    }
    tracing::warn!("Unable to discover harness root containing .claude/; preamble disabled");
    None
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
/// Compact, binary-embedded agent-control discovery nudge.
///
/// Appended to every per-kind preamble by [`load`], so BOTH fresh launch
/// (`session::launch`) and context rotation (`session::rotation`) - which each
/// consume `load()` - advertise the daemon's `Agent*` control verbs to the
/// worker. This is embedded in the binary (not `.claude/` disk discovery) so it
/// travels to any working directory, and it advertises ONLY the `Agent*` verbs
/// plus the `rsi-rpc` token convention - never the generic RPC passthrough.
pub const AGENT_DISCOVERY_NUDGE: &str = "\
## Agent control (rsi)

You are an rsi-managed agent session. Prefer the typed `rsi_control_*` tools \
when your provider exposes them. `rsi-rpc <Verb>` is the validated compatible \
fallback; its authority token is supplied automatically via \
`$RSI_SESSION_TOKEN` (transport-only - NEVER pass it in `--params`). The only \
control verbs available to you are:

- `AgentSpawnChild` - spawn a child agent session under you.
- `AgentReserveSuccessor` - reserve one daemon-authored same-Epic master successor; use this instead of Fresh for authority turnover.
- `AgentGetProgress` - read one durable progress snapshot for your child cohort.
- `AgentSendMessage` - queue attributed mail within your existing child-control scope.
- `AgentGetStatus` - report status of you and your children.
- `AgentHalt` - halt a running child agent.
- `AgentContinueChild` - continue an exact child with a new prompt; supply the observed continuation cursor as a staleness fence. Continuing a running child interrupts its active turn, and delivery is not deduplicated.
- `AgentArchiveChild` - archive a terminal child of the Epic you currently lead; supply the observed continuation cursor as a staleness fence. Current Epic lead only.
- `AgentScheduleWake` - schedule a future wake/callback; `mode` is required. Use \
  `resume` for same-session continuation, `fresh` only for a best-effort \
  post-terminal root launch that carries no lead authority, `on_terminal` for a child watch, or `program_guard` for the \
  unattended-program sentinel.
- `AgentCreateIssue` - create a durable attributed issue follow-up.
- `AgentListIssues`, `AgentGetIssue`, `AgentUpdateIssue`, `AgentUpdateIssueStatus`, \
  `AgentArchiveIssue`, `AgentRestoreIssue`, and `AgentListIssueEvents` - guarded \
  project Issue controls for the current owning-Epic lead or the current \
  appointed manager with the `IssueCoordinate` grant. `AgentCreateIssue` \
  remains available to ordinary project workers for follow-ups.
- `AgentManagerProgress` - read bounded progress for your operator-appointed manager scope.
- `AgentManagerInbox` - retrieve durable requests or replies in your live manager or feature-lead scope.
- `AgentManagerSend` - queue a request to a selected Epic's current lead.
- `AgentManagerReply` - record an explicit reply by request ID as the current feature lead.
- `AgentManagerNotify` - send one unsolicited notice to your current appointed manager as the current feature lead.
- `AgentManagerInspect` - page scoped workers, work, requests, decisions, topology, resources, actions and events.
- `AgentManagerUpdate` - submit a typed ledger change with observed scope/policy versions and an idempotency key; lead request acknowledgement remains lead-bound.
- `AgentSubmitReviewReceipt` - submit one immutable exact-source receipt as the live assigned reviewer; the daemon binds reviewer identity, invocation and custody.
- `AgentManagerControl` - queue an explicitly granted lifecycle/topology action with exact target fences; inspect its receipt for execution outcome.
- `AgentManagerPrepareControl` - preflight one supported semantic manager action against daemon-resolved live fences without queueing an effect.
- `AgentManagerCommitPreparedControl` - commit an exact prepared ID/digest with an idempotency key; the daemon rechecks mutable authority and target state atomically.
- `AgentManagerGetAction` - read one action receipt within the authenticated current manager's project and logical-manager scope.
- `AgentManagerWorkView` - read your Epic's live work and granted file ownership as a manager-created worker (read-only).
- `AgentTopologyUpsert`, `AgentTopologyList`, `AgentTopologyExecute`, `AgentTopologyGetExecution`, `AgentTopologyInterrupt`, and `AgentTopologyResolveAttempt` - author and run daemon-executed deterministic topologies as the current manager with `Automation` or an Epic lead within its Epic.

`AgentSpawnChild` and `rsi_control_spawn` accept optional `provider`, `model`, \
and `effort` fields. Set `provider` to launch a child on a different backend; \
omitting it preserves the caller's provider, model, and effort defaults.

`AgentSendMessage` accepts mail into a queue. A `queued` receipt does not prove \
provider delivery; `failed` or `expired` is final mailbox settlement, not \
proof of provider effect. Omitted or null `expires_at` gets a deadline 30 \
minutes after first acceptance; exact retries keep that original deadline. \
The deadline keeps running while the delivery Session is missing, recovery \
is pending, or dispatch is denied; held queued mail can expire before \
recovery or an idle boundary. This finite deadline is intentional. \
Claude and Codex CLI workers have no authenticated mid-turn delivery and \
acknowledgment path here. Use `AgentContinueChild` only when deliberately \
interrupting and replacing a running turn.

Run `rsi-rpc agent` to list these verbs. Do not invoke any other RPC method as \
an agent.

## Harness manager requests and replies

Manager appointment and scope changes are operator-only. A title, prompt, or \
tool registration grants no authority; existing child-control permissions stay unchanged.
An appointed manager reads `AgentManagerProgress` with `{}`, follows \
`next_after_epic_id` using `after_epic_id` until null, then uses `AgentManagerSend` with `epic_id`, `message`, and `idempotency_key` for selected \
feature Epics. Native equivalents are `rsi_control_manager_progress` and \
`rsi_control_manager_send`. Reuse a write key only for identical content.

Feature leads read `AgentManagerInbox` (native `rsi_control_manager_inbox`) with \
`{}` or `after_sequence`, `limit` (1..32, default 32), and optional `request_id`. \
Reply explicitly using `AgentManagerReply` (native `rsi_control_manager_reply`) \
with the recorded request ID, `message`, and `idempotency_key`; include evidence \
or a blocker. Send unsolicited status or blockers with `AgentManagerNotify` \
(native `rsi_control_manager_notify`) using only `message` and `idempotency_key`; \
the daemon derives your Epic and manager. A notice is never a request, approval \
or acceptance. The manager reads the same inbox to collect replies and reports \
what the evidence establishes. Send receipts mean queued, and an explicit reply \
means replied; neither proves acceptance or completed implementation.

Inbox retrieval is a durable tool read. Existing watches deliver coalesced \
notices to read it once the sender or recipient is idle; do not assume provider \
injection or interrupt an active turn to deliver manager mail. Live scope and \
current leadership are rechecked, including after rotation or revocation. \
Direct operator instructions take precedence: surface conflicts before acting. \
Human approvals remain operator-owned; manager mail cannot answer or clear them.

V2 policy is a separate operator opt-in. Use `AgentManagerInspect` (native \
`rsi_control_manager_inspect`) with `{}` or a section and returned cursor. \
Partial traversal, reported progress, committed source, accepted work and \
integrated delivery are separate facts; unknown evidence stays unknown.
`AgentManagerUpdate` / `rsi_control_manager_update` record work, stage evidence, \
dependencies, ownership, migration reservations, request lifecycle, decisions \
and handoff. `AgentManagerControl` / `rsi_control_manager_control` queue only \
explicitly granted exact-fence legacy actions. Prefer \
`AgentManagerPrepareControl` / `rsi_control_manager_prepare_control`, then commit \
the returned ID and digest with `AgentManagerCommitPreparedControl` / \
`rsi_control_manager_commit_prepared_control`; inspect the durable result with \
`AgentManagerGetAction` / `rsi_control_manager_get_action`. Read exact nested \
schemas with `rsi-rpc <Verb> --schema`.
A session the current manager created reads its Epic's live work, `mine`, \
active file ownership, pause and unanswered-request delivery state with \
`AgentManagerWorkView` / `rsi_control_manager_work_view` (`{}` or `work_key`, \
`after_work_key`, `limit` 1..32). Granted ownership there is authoritative \
without a relay turn; the view is read-only and grants no write, mail or \
continuation authority.
Deterministic topologies: `AgentTopologyUpsert` / `rsi_control_topology_upsert` \
(`validate_only` reports diagnostics), then `AgentTopologyExecute` with the \
returned `definition_digest`; read with `AgentTopologyGetExecution`, stop with \
`AgentTopologyInterrupt`, and settle preserved work with \
`AgentTopologyResolveAttempt`. Only the current manager holding `Automation` \
(in-scope Epics) or an Epic lead (its own Epic; never `discard`) may call them. \
Every session node names an explicit operator-allowed provider/model/effort.
The manager may request DB-native review with the typed `request_review` update. \
The assigned reviewer submits `AgentSubmitReviewReceipt` / \
`rsi_control_submit_review_receipt` during the bound invocation. Do not create a \
review artifact or evidence commit for that assignment.
Mutations carry observed `fence` scope/policy versions and an identical-retry \
`idempotency_key`; lead actions also carry observed lead/event/custody fences. \
Never supply caller identity or permissions. Refresh after stale errors; do not \
guess a new target or turn an uncertain receipt into another launch.
Root `succeed_manager` requires explicit SelfSuccession and the current parentless \
Standard manager. Read Overview's `manager_control` epoch/custody observation; \
supply `expected`, `launch` and committed `handoff` (source_commit, relative_path, \
blob_oid). Receive the queued receipt, then finish your turn. The daemon waits \
for predecessor settlement and establishes distinct custody before publishing \
authority. Logical work/mail/policy and accounting persist. Explicit succession \
charges session creation, not automatic recovery; preserve actual human gates. \
Status/monitor/execute intent, pauses, creation quotas, concurrency, allowed \
provider/model/effort choices, retry limits and spend caps are persisted policy. \
Unfinished authorized work remains an obligation after a provider turn ends. \
Respect existing retry owners, operator pauses and exact human gates. Only the \
operator answers consolidated decisions; manager requests cannot grant approval. \
ProgramRun methods remain operator-only and are not manager tools.";

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

1. RSI-native in-process control tools when present: `rsi_control_spawn`, `rsi_control_reserve_successor`, \
`rsi_control_progress`, `rsi_control_send_message`, `rsi_control_status`, \
`rsi_control_halt`, `rsi_control_create_issue`, `rsi_control_list_issues`, \
`rsi_control_get_issue`, `rsi_control_update_issue`, `rsi_control_update_issue_status`, \
`rsi_control_archive_issue`, `rsi_control_restore_issue`, `rsi_control_list_issue_events`, \
`rsi_control_manager_progress`, `rsi_control_manager_inbox`, \
`rsi_control_manager_send`, `rsi_control_manager_reply`, `rsi_control_manager_notify`, \
`rsi_control_manager_inspect`, `rsi_control_manager_update`, \
`rsi_control_submit_review_receipt`, `rsi_control_manager_control`, \
`rsi_control_manager_prepare_control`, `rsi_control_manager_commit_prepared_control`, \
`rsi_control_manager_get_action`, `rsi_control_manager_work_view`, \
`rsi_control_topology_upsert`, `rsi_control_topology_list`, \
`rsi_control_topology_execute`, `rsi_control_topology_get_execution`, \
`rsi_control_topology_interrupt`, `rsi_control_topology_resolve_attempt`, \
`schedule_wake`.
2. RSI agent RPC verbs via `rsi-rpc`: `AgentSpawnChild`, `AgentReserveSuccessor`, `AgentGetProgress`, \
`AgentSendMessage`, `AgentGetStatus`, `AgentHalt`, `AgentContinueChild`, `AgentArchiveChild`, `AgentScheduleWake`, \
`AgentCreateIssue`, \
`AgentListIssues`, `AgentGetIssue`, `AgentUpdateIssue`, `AgentUpdateIssueStatus`, \
`AgentArchiveIssue`, `AgentRestoreIssue`, `AgentListIssueEvents`, \
`AgentManagerProgress`, `AgentManagerInbox`, `AgentManagerSend`, `AgentManagerReply`, `AgentManagerNotify`, \
`AgentManagerInspect`, `AgentManagerUpdate`, `AgentSubmitReviewReceipt`, `AgentManagerControl`, \
`AgentManagerPrepareControl`, `AgentManagerCommitPreparedControl`, `AgentManagerGetAction`, \
`AgentManagerWorkView`, `AgentTopologyUpsert`, `AgentTopologyList`, `AgentTopologyExecute`, \
`AgentTopologyGetExecution`, `AgentTopologyInterrupt`, `AgentTopologyResolveAttempt`.
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

/// Load the per-kind worker preamble the launch/rotation paths inject into the
/// system prompt. Returns the on-disk kind preamble (or base) with the
/// binary-embedded [`AGENT_DISCOVERY_NUDGE`] and [`RSI_BACKEND_POLICY`]
/// appended. Because these layers are embedded, this always returns `Some(_)`
/// even when no harness root / preamble file can be discovered - the
/// agent-control frame is never lost to a missing disk file, and rotation
/// (which calls this same loader) keeps it.
pub fn load(kind: SessionKind) -> Option<String> {
    let disk = load_disk(kind);
    let embedded = format!(
        "{AGENT_DISCOVERY_NUDGE}\n\n{THOUGHTS_COMMIT_POLICY}\n\n{RSI_BACKEND_POLICY}\n\n{DAEMON_MESSAGE_CONVENTION}"
    );
    Some(match disk {
        Some(preamble) => format!("{preamble}\n\n{embedded}"),
        None => embedded,
    })
}

/// Disk-backed portion of [`load`]: composes the base preamble before a readable
/// kind variant. Missing or unreadable variants fall back to the base; a
/// readable variant still degrades gracefully if the base becomes unreadable
/// after harness-root discovery. `None` only when no harness root is discovered
/// or every applicable file read fails.
fn load_disk(kind: SessionKind) -> Option<String> {
    let root = harness_root()?;
    load_disk_from_root(root, kind)
}

fn load_disk_from_root(root: &Path, kind: SessionKind) -> Option<String> {
    let base_path = root.join(SHARED_DIR).join(BASE_FILENAME);
    let base = match std::fs::read_to_string(&base_path) {
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
        match std::fs::read_to_string(&path) {
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

/// Load the orchestration router skill body — the "mouth of the pipeline"
/// frame injected at the start of every leaf-kind session's system prompt.
///
/// Routes to `<harness_root>/.claude/commands/orchestration_router.md`,
/// reusing `harness_root()`'s OnceLock-cached discovery. Returns `None`
/// when no harness root could be discovered or the router file is missing
/// — launch is never aborted by router-loading failure (graceful
/// degradation, parallel to `load(kind)`).
///
/// This loader is invoked from `super::launch::launch_session` BEFORE the
/// per-kind preamble is pushed onto the system_prompt parts vector, so the
/// router occupies the outermost frame for every spawnable session
/// (Standard, TaskRabbit, Bug, Story, Task, Feature, Refactor, Research).
/// Container kinds (Group/Epic) are gated out before this loader runs and
/// therefore never receive the router.
pub fn load_orchestration_router() -> Option<String> {
    let root = harness_root()?;
    let path = root.join(COMMANDS_DIR).join(ORCHESTRATION_ROUTER_FILENAME);
    match std::fs::read_to_string(&path) {
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

    fn worker_projection() -> AgentAuthorityProjection {
        AgentAuthorityProjection {
            revision: "sha256:worker-revision".into(),
            pending: false,
            is_lead: false,
            is_manager: false,
            is_reviewer: false,
            verbs: vec![Verb::GetStatus, Verb::CreateIssue],
            update_variants: Vec::new(),
            control_actions: Vec::new(),
            prepared_actions: Vec::new(),
            delegated_operator_methods: Vec::new(),
            guidance_ids: vec!["common", "worker"],
        }
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
            let preamble = load(kind).expect("load() always returns the embedded nudge");
            assert!(
                preamble.contains(AGENT_DISCOVERY_NUDGE),
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
        let rotated = load(SessionKind::Task).expect("rotation loader returns the nudge");
        assert!(
            rotated.contains(AGENT_DISCOVERY_NUDGE),
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
    fn agent_discovery_nudge_advertises_only_agent_verbs() {
        // Advertises the closed Agent* control surface and the tokened
        // rsi-rpc convention — never the generic RPC passthrough.
        for descriptor in rsi_common::agent_control_schema::agent_control_catalog_v1() {
            let verb = descriptor.method;
            assert!(
                AGENT_DISCOVERY_NUDGE.contains(verb),
                "nudge must advertise {verb}"
            );
        }
        assert!(AGENT_DISCOVERY_NUDGE.contains("rsi-rpc"));
        assert!(AGENT_DISCOVERY_NUDGE.contains("RSI_SESSION_TOKEN"));
        assert!(AGENT_DISCOVERY_NUDGE.contains("`mode` is required"));
        assert!(AGENT_DISCOVERY_NUDGE.contains("same-session continuation"));
        assert!(AGENT_DISCOVERY_NUDGE.contains("optional `provider`"));
        assert!(AGENT_DISCOVERY_NUDGE.contains("different backend"));
        // The guarded Issue controls admit both the owning-Epic lead and the
        // IssueCoordinate manager (K3/K11); this must stay a positive claim.
        assert!(AGENT_DISCOVERY_NUDGE.contains(
            "project Issue controls for the current owning-Epic lead or the \
  current appointed manager with the `IssueCoordinate` grant"
        ));
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
                !AGENT_DISCOVERY_NUDGE.contains(forbidden),
                "nudge must not advertise generic surface `{forbidden}`"
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn manager_preamble_preserves_operator_authority_and_inbox_semantics() {
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
                AGENT_DISCOVERY_NUDGE.contains(requirement),
                "missing manager instruction: {requirement}"
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
}
