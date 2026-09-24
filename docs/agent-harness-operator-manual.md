# RSI Operator Manual: AI-Agent Harness

**Audience:** people operating RSI to run, supervise, and review AI coding
agents.

**Scope:** current operator workflows and their limits. This is not a source
code reference, a generic prompt-engineering guide, or a claim that every
feature visible in the repository is enabled in every installation.

## Read this first

### Guarded local Issue controls (V97)

Harness and CodexAppServer receive the same construction-bound native Issue
tools as tokened CLI providers: `rsi_control_list_issues`,
`rsi_control_get_issue`, `rsi_control_update_issue`,
`rsi_control_update_issue_status`, `rsi_control_archive_issue`,
`rsi_control_restore_issue`, and `rsi_control_list_issue_events`. They share
the RPC DTO validation and `AgentControlHandle`; caller identity is never a
tool argument. The native tools are registered uniformly for every
construction-bound session so fresh and rotated rosters match; registration
does not grant authority. Only the current persisted owning-Epic lead, or the
current appointed manager holding the V2 `IssueCoordinate` grant, may execute
the seven project-wide tools. `rsi_control_create_issue` remains
executable by ordinary project workers for follow-up filing. See
[Agent control](agent-control.md#guarded-issue-control-v95) for each verb's
payload/result, CAS/replay behavior, and redacted error examples.

RSI is an **AI-agent harness**: it gives you a durable control plane around
coding agents. It records work as sessions, starts provider processes, keeps
their conversations and status, organizes work by project, and can isolate
changes in Git worktrees. The TUI is the operator console; the daemon is the
runtime and persistence owner; providers are the agent integrations.

The normal operating loop is:

```text
choose project → launch a leaf session → give it a bounded objective
       ↓                                      ↓
inspect status/transcript ← daemon persists ← provider works in its sandbox
       ↓
answer, redirect, interrupt, rotate, or archive → review the resulting change
```

You do not need to learn the advanced graph, program, or agent-control systems
to use RSI productively. Start with one project and one session. Add hierarchy
or orchestration only when a single session stops being the right unit of work.

## Contents

1. [System model and terms](#system-model-and-terms)
2. [Capability levels and source of truth](#capability-levels-and-source-of-truth)
3. [First-use procedure](#first-use-procedure)
4. [Everyday session operations](#everyday-session-operations)
5. [Projects, working directories, and sandboxes](#projects-working-directories-and-sandboxes)
6. [Context, memory, and rotation](#context-memory-and-rotation)
7. [Structured work: hierarchy and topology](#structured-work-hierarchy-and-topology)
8. [Agent control and scheduled continuity](#agent-control-and-scheduled-continuity)
9. [Settings, diagnostics, and recovery](#settings-diagnostics-and-recovery)
10. [Performance and evaluation](#performance-and-evaluation)
11. [Command reference and glossary](#command-reference-and-glossary)

## System model and terms

### The control-plane model

```text
Operator
  │ keyboard, commands, review decisions
  ▼
rsi TUI ───── Unix socket ───── rsid daemon ───── provider process / API
                                     │                     │
                                     │                     ├─ Claude / Codex / Pioneer / Local /
                                     │                     │  Antigravity / CodexAppServer /
                                     │                     │  Harness, when configured
                                     ▼
                              durable session state
                              projects, jobs, events
                                     │
                                     ▼
                              working directory or
                              per-session Git worktree
```

The TUI does not directly own the agent process. The daemon does. That is why a
session can remain visible, retain a transcript, report a terminal state, or be
continued after the TUI has been closed and reopened.

### Canonical terminology

| Term | Meaning | Do not confuse it with |
|---|---|---|
| **Operator** | The person using the TUI and making authority decisions. | A provider-side agent. |
| **Daemon (`rsid`)** | The persistent service that owns sessions, lifecycle, storage, jobs, and provider processes. | The TUI process. |
| **Session** | A durable unit of agent work: prompt, transcript, provider configuration, status, and workspace context. | A terminal tab or a temporary chat. |
| **Provider** | The integration class used to execute a session. | The model selected within it. |
| **Model** | The specific model identifier requested from a provider. | Provider availability or credentials. |
| **Effort** | A provider/model-specific reasoning or compute setting, when supported. | A universal quality setting. |
| **Project** | An operator-owned grouping for sessions and project-scoped context/memory. | A Git branch or a directory. |
| **Working directory** | The repository or directory a session is launched against. It is required for a session. | The agent's sandbox root. |
| **Sandbox** | The execution isolation requested for a session; commonly a Git worktree. | A provider permission policy. |
| **Session kind** | The role/shape of a session in the hierarchy. | Its runtime status. |
| **Status** | The lifecycle state of a session, such as `Running` or `Completed`. | The quality of its output. |
| **Container** | A non-spawnable organizational session: `Group` or `Epic`. | A parent process. |
| **Leaf** | A spawnable session that can run a provider. | A container or a continuation record. |
| **Topology** | A named dependency graph attached to an Epic for structured child execution. | The general session tree. |
| **Lead** | A direct leaf child selected to coordinate an Epic under the daemon's rules. | An unrestricted administrator. |
| **Scheduled job / wake** | A durable future continuation or trigger. | A background process that is guaranteed to make progress forever. |

### Provider classes

The current provider type system supports these classes:

- **Claude** — Claude CLI integration.
- **Codex** — Codex CLI integration.
- **Pioneer** — Pioneer inference through the Codex CLI transport.
- **Local** — Ollama or another local OpenAI-compatible server.
- **Antigravity** — Google Antigravity CLI integration; `Gemini` is accepted as
  a compatibility alias in stored data.
- **CodexAppServer** — Codex app-server integration for structured approvals
  and multi-turn continuation.
- **Harness** — a direct API harness that owns its own conversation loop,
  tools, and compaction.

Supported is not the same as available. A choice may be absent or may fail to
launch if its binary, account, credentials, API endpoint, model, or feature
configuration is not present on your installation. Check **Settings**, the
session creation form, and `F3` session information before treating a provider
as operational.

#### Pioneer setup and model refresh

`rsid` loads daemon secrets from `~/.rsi/.env` at process startup. It does not
load a repository `.env`. Configure one supported Pioneer variable there,
preferably the primary name:

```dotenv
PIONEER_AI_INFERENCE=your_pioneer_key
# fallback name: PIONEER_API_KEY=your_pioneer_key
```

Restart `rsid` after adding or changing the value. Pioneer is shown as
available only when the Codex CLI is installed and one of those variables is
nonblank; `PIONEER_AI_INFERENCE` wins when both are present.

Every explicit Pioneer model refresh performs an authenticated request to
`https://api.pioneer.ai/v1/models` and uses only its top-level `models` catalog.
RSI places exactly one `pioneer/auto` router first, then atomically replaces
`~/.rsi/model-catalogs/pioneer.json` with the current catalog. If refresh fails,
the TUI has only the minimal `pioneer/auto` fallback; it does not present a
baked vendor list as current account data.

The credential value stays in the daemon environment and authenticated request
header. RSI never places it in Codex argv, catalog cache, logs, errors, or
documentation. Codex receives only the selected environment-variable name.

### Session kinds and status

Session kinds define structure. Status defines lifecycle.

```text
root
├── Standard                         spawnable leaf
└── Group                            container
    ├── Standard                     spawnable leaf
    └── Epic                         container
        ├── Story                    spawnable leaf
        ├── Task                     spawnable leaf
        ├── Bug                      spawnable leaf
        ├── Feature                  spawnable leaf
        ├── Refactor                 spawnable leaf
        └── Research                 spawnable leaf

TaskRabbit is a special one-shot leaf launched through the task workflow.
```

`Group` and `Epic` organize work; they never launch a provider subprocess.
Leaves do. The UI enforces legal parent/child combinations, so use a leaf for
agent work and a container only when you need structure.

The runtime statuses are:

| Status | Operator meaning |
|---|---|
| `Starting` | The daemon is preparing the provider process or API run. |
| `Running` | The provider is actively working or streaming output. |
| `WaitingApproval` | The session needs an operator answer or approval. |
| `Completed` | The session reached a normal terminal result. Review it; completion is not an automatic correctness certificate. |
| `Failed` | The provider or runtime could not finish normally. Preserve the evidence and inspect diagnostics before retrying. |
| `Interrupted` | Work was deliberately stopped or terminated. It can often be continued with a new instruction. |
| `Archived` | The session is kept as historical record and removed from active workflow. |
| `Deleted` | The session is logically deleted / in trash. Treat recovery as an explicit workflow, not a casual undo guarantee. |

There is intentionally no `Interrupting` status. The UI may show local
interrupt progress, but the durable session will settle into a real status.

### Codex diagnostics are not turn settlement

Codex CLI can emit item-level `error` diagnostics before a turn starts—for
ignored settings, unavailable model metadata, and similar warnings. The daemon
records these events as `process_error` with `terminal:false`; it does not fail
or close the turn (2026-09-23 #662). Terminal settlement remains `turn.failed`
or observed process exit. App-Server-compatible event mapping retains its
separate existing contract.

### Final metadata persistence

Final status persistence is not a license to report successful work whose final
metadata write failed. The finalizer persists status and final metadata, then
uses a scoped metadata persistence barrier. A failed write is classified as
`session_metadata_persistence_failed`, emits the daemon error, retains the
provider terminal state and recorded usage, and suppresses post-finalization
rotation and successful settlement actions that depend on ordered persistence
(2026-09-23 #640).

## Capability levels and source of truth

### Read capability labels literally

The harness has more surface area than a daily workflow needs. Use these levels
when deciding what to rely on:

| Level | What belongs here | How to use it |
|---|---|---|
| **Core** | Projects, ordinary sessions, provider/model selection, transcripts, lifecycle actions, session info, archive/trash, search, and keyboard navigation. | Use these as your default operating system. |
| **Structured** | Group/Epic hierarchy, topology attached to an Epic, project-scoped memory, context rotation, prompt creation. | Use when one session is not enough and prerequisites are clear. |
| **Advanced operator** | Schedules, issues, graph editor, cards, embedded terminal, settings categories, diagnostics, Git panel. | Use after you understand the related session/project state. |
| **Restricted agent control** | Agent spawning, progress/status inspection, halting, durable messages, wakes, and issue creation. | Agents use only the scoped controls the daemon authorizes; operators supervise the outcome. |
| **Internal, gated, or specialist** | Recursive DAG browsing, ProgramRun kernel, raw database inspection, raw generic daemon RPC, experiment-specific scripts. | Do not build an everyday workflow around these unless the feature's own guide and your installation say it is ready. |

This manual deliberately does not flatten those categories into “everything is
a button.” An implementation may exist without being configured, surfaced,
supported for your provider, or appropriate for normal work.

### When documentation and reality disagree

Use this order of evidence:

1. The TUI control that is actually available, its validation result, and the
   current session information (`F3`).
2. `?` for the compiled keybinding help and `:diag` / `:diagnostics` for live
   client health information.
3. The current agent catalog, when investigating provider-side control:
   `rsi-rpc agent` lists the agent-facing verbs available to that process.
4. Current source and focused tests, for a development checkout.
5. This manual and focused guides under `docs/`.
6. Architecture documents, old plans, and historical artifacts, which are
   valuable context but not a promise of present availability.

If a command is not offered, a form rejects it, or the daemon says a capability
is unavailable, treat that as authoritative for the current installation.
Capture the exact error, session/provider/model, and diagnostic context before
assuming the harness is broken.

## First-use procedure

This procedure gets you from a running harness to a reviewable result without
using any advanced orchestration.

### 1. Confirm the control plane is alive

Start the daemon before connecting the TUI. In a repository development setup,
the supported helpers are:

```bash
make release-install
./scripts/dev-daemon.sh
./scripts/dev-tui.sh
```

For an installed environment, use the daemon/service management method your
deployment provides. RSI normally communicates through a Unix socket at
`~/.rsi/daemon.sock` and stores durable state under `~/.rsi/`; do not edit the
database as routine administration.

If the TUI cannot connect, do not start multiple daemons blindly. First inspect
the existing daemon process, socket, and logs. RSI expects one active daemon
for its normal state store; a second process can correctly refuse its singleton
lock rather than repair the first one.

### 2. Select or create a project

Open the project picker with `<Space>p` or `:projects`. Create a project using
the picker, or in command mode:

```text
:project-new <name> [path]
```

Use one project per durable codebase or workstream. It gives sessions a coherent
home and enables project-scoped memory behavior. A project is not a substitute
for Git branches: it is an RSI organizational and context boundary.

### 3. Launch the smallest suitable session

Use a **Blank** session for general-purpose work or a **TaskRabbit** session
for a bounded one-shot task.

| Action | Shortcut | Command mode equivalent |
|---|---|---|
| Open general Blank session prompt | `<Space>m` | `:blank` |
| Launch Blank session with prompt | — | `:blank <objective>` |
| Open TaskRabbit one-shot prompt | `<Space>o` | `:task` |
| Launch TaskRabbit with prompt | — | `:task <objective>` |

Write the initial objective as a concrete deliverable. Good first prompts name
the target, constraints, expected evidence, and whether the agent may edit.
For example:

```text
Investigate why the integration test is flaky. Do not change code yet.
Return the likely cause, evidence, and the smallest safe fix.
```

Or:

```text
Implement the approved plan in thoughts/shared/plans/<file>.md.
Work only in the assigned sandbox, run the named focused tests, and report the
commit plus any verification gap.
```

### 4. Set execution context before launch

The creation form lets you choose the session's relevant execution settings.
Check these deliberately:

- **Project** — aligns organization and project-scoped context.
- **Working directory** — the codebase the agent is allowed to operate in.
- **Provider** — the integration class that will run the work.
- **Model** — the provider model requested for the session.
- **Effort** — use only when the selected provider/model offers it.
- **Sandbox** — choose isolation for code-changing work unless you explicitly
  need a shared working directory.

`Ctrl+M` opens the model chooser / changes the default model for new sessions. The
session creation form also supports a per-session model choice. `F3` is the
fastest way to audit what was actually recorded after launch.

### 5. Supervise, do not disappear

Open the session with `Enter`, `l`, or `L`. Read its first response and inspect
the settings in `F3`. Then choose one of four operator actions:

| Situation | Action |
|---|---|
| The agent needs clarification or approval | Open the session, answer directly in the input bar, or use `gq` for a waiting question. |
| The task is proceeding correctly | Let it continue; use transcript navigation and status rather than repeatedly sending “continue.” |
| The work is headed in the wrong direction | Send a concise redirect that names the new boundary and the evidence you expect. |
| The work must stop | Press `x` or use `:kill`; wait for the durable terminal status before reusing the session. |

Use `]a` / `[a` to move through sessions needing attention. Use `/` to filter
the session list or search a transcript. Press `p` to preview a selected
session's full initial prompt.

### 6. Close the loop

When the agent says it is done:

1. Read its final report and inspect tool/test evidence.
2. Review the actual diff in the relevant worktree or Git panel; a completed
   status means the agent finished its turn, not that the change is accepted.
3. Request a focused follow-up in the same session if context is valuable, or
   start a new bounded session if the task has changed materially.
4. Archive the session with `<Space>a` once the result is durable historical
   record. Pin useful active work with `P`.

Use `DD` only when you intend to move the session to trash. Archive is the
normal way to take completed work out of the active queue while preserving it.

## Everyday session operations

### Input and continuation

The session detail input bar is a modal text surface. Common actions are:

| Action | Control |
|---|---|
| Enter input insert mode | `i`, `a`, `o`, or `O` in the session detail view |
| Submit a prompt | `Ctrl+Enter` |
| Continue an idle session | `X`, `:continue`, or `:continue <instruction>` |
| Interrupt the focused session | `x` or `:kill` |
| Rotate the session's context | `R` or `:rotate` |
| Toggle automatic rotation for this session | `<Space>r` |
| Cancel a pending retry | `<Space>k` |

Use a follow-up prompt to correct the task, not to restate the entire original
prompt. State what changed, what must remain unchanged, and what evidence you
expect next. For example:

```text
Keep the investigation read-only. Narrow the cause to the retry scheduler and
show the exact source references; do not propose a database migration.
```

### Inspecting a session

`F3` opens the session information panel. It is the first place to verify:

- session ID and title;
- provider, model, and effort;
- working directory and project;
- hierarchy position, label, tags, rating, and context usage;
- whether the session is in the expected sandboxed setting.

Transcript navigation is optimized for review:

- `Shift+Up` / `Shift+Down` moves between events.
- `[u` / `]u` moves between your messages.
- `yy` copies the selected event content.
- `n` / `N` moves through transcript search matches after `/`.

Use `r` to refresh navigation data, `rc` to refresh the current view, and `rm`
to refresh project/label metadata if the UI is stale. Refresh is safer than
inventing state by restarting or editing persistence directly.

### Lifecycle choices

| Intent | Preferred action | Reason |
|---|---|---|
| Preserve a completed record but remove it from active work | Archive (`<Space>a` or `:archive`) | Normal historical lifecycle. |
| Recover a historical record | Navigate to archive/trash and use the corresponding recovery UI | Keeps the state transition explicit. |
| Stop active provider work | Interrupt (`x` / `:kill`) | Lets the daemon settle the provider process and status. |
| Resume with more instruction | Continue (`X` / `:continue`) | Preserves existing transcript/context when appropriate. |
| Start a materially different task | New session | Avoids hiding a new objective inside unrelated context. |
| Mark useful work for fast access | Pin (`P`) | Keeps it prominent without changing lifecycle. |

`Completed`, `Failed`, and `Interrupted` are different evidence states. Do not
collapse them into “done.” A failed session may contain the clue needed for the
next task; an interrupted one may have produced valid partial work.

## Projects, working directories, and sandboxes

### Project versus directory versus branch

These three things are intentionally separate:

| Item | Primary purpose | Scope |
|---|---|---|
| Project | Organizes sessions and project-scoped context. | RSI metadata. |
| Working directory | Tells a session which codebase/location it operates against. | Filesystem path. |
| Branch / worktree | Isolates or stages source changes. | Git repository state. |

A session must have a working directory. A project is strongly recommended for
any continuing work. A sandboxed session can have both a shared working
directory and a different sandbox root; the sandbox root is the agent's write
location when the daemon allocates a worktree.

### Sandboxed code work

For a code-changing session, inspect whether the session uses a Git worktree
sandbox before you ask it to edit. In an RSI-managed sandbox:

1. Treat the daemon-assigned sandbox root and branch as authoritative.
2. Tell the agent to work in its assigned sandbox, not the shared repository
   root shown as the broader working directory.
3. Do not direct an agent to `git checkout`, `git switch`, or create a branch
   inside an RSI-owned sandbox unless the harness explicitly allocated that
   branch for the work.
4. Review, commit, merge, and push through the normal repository workflow once
   the agent has finished.

This protects the operator's shared worktree and prevents two writers from
quietly editing the same branch. A sandbox is a custody boundary, not merely a
convenience path.

### Safe defaults

- Use an isolated sandbox for implementation, refactors, migrations, and
  anything that can change many files.
- Use a shared directory only for intentionally shared, low-risk work that you
  are actively supervising.
- Keep research sessions read-only unless a later implementation step is
  explicitly authorized.
- Before accepting a result, inspect the diff in the actual repository/worktree
  named by the session, not in a similarly named directory.

## Context, memory, and rotation

### Three different kinds of context

| Kind | What it does | Operator implication |
|---|---|---|
| Transcript context | The conversation carried within a session. | Continue a session only when prior context is still useful. |
| Context rotation | A controlled handoff when a long session needs a fresh context window. | Use `R` deliberately; enable automatic behavior only when you understand the task's continuity needs. |
| Project memory | Durable project-scoped material retrieved for relevant future work. | Assign a project before launch if you expect project memory to inform the agent. |

Memory is intentionally scoped. At launch, an assigned session receives
project-relevant memory; a session without a project does not receive that
project's memory. Interactive memory search (`<Space>M`) is a discovery tool and
may have a different scope than automatic agent injection. Do not assume a
manual search result was automatically sent to every agent.

Read [Memory Architecture](memory-architecture.md) before changing memory
configuration or treating it as a security boundary.

### Rotation discipline

Use rotation when the work has a clear handoff boundary: a plan is complete,
implementation is ready, evidence has been collected, or an agent is stuck in
large stale context. Preserve the current objective and result in the handoff
prompt. Avoid rotating merely because time has passed.

Before rotation, ask:

1. What durable artifact summarizes the work so far?
2. What exact next action should the successor take?
3. Which files, tests, or decisions must remain in scope?
4. Is a new session clearer than rotating this one?

If context rotation is disabled by daemon configuration or session setting, the
control may not behave as expected. Inspect settings and session information
instead of assuming a failed continuation created a separate agent.

## Structured work: hierarchy and topology

### When to add hierarchy

Use hierarchy to represent ownership and work decomposition, not to make a
single task look more sophisticated. A useful escalation path is:

```text
one bounded leaf session
    ↓ task has independently reviewable sub-work
Group → Epic → leaf sessions
    ↓ dependencies are fixed and meaningful
Epic with a topology
```

Create entities with these normal-mode chords:

| Chord | Creates |
|---|---|
| `gG` | Group |
| `gE` | Epic |
| `gS` | Story |
| `gT` | Task |
| `gB` | Bug |

They open the same kind-aware creation form. The form prevents illegal
parent/child combinations. Use its Kind selector when you need `Feature`,
`Refactor`, or `Research` rather than trying to reparent a leaf after it starts
work.

Navigation uses `Enter`, `L`, or `l` to enter/open an item and `-` or `H` to
ascend. `mp` opens a parent picker, and `mo` moves the focused session to root.

### Parentage is not continuation history

A child belongs to a Group or Epic. A rotated/continued session belongs to a
conversation lineage. These answer different questions:

- **Hierarchy:** Who owns this work in the project tree?
- **Continuation:** Which prior session supplied the handoff context?

Do not use a container merely to represent “the next attempt.” Use the session
continuation/rotation path for that.

### Epic topology

A topology is a named directed acyclic graph (DAG) of work nodes and
dependencies that an Epic owns. It is appropriate when you know the dependency
shape before execution, such as:

```text
research ─┬─ implementation ─┬─ verification
          └─ test design ────┘
```

Normal workflow:

1. Create or select a topology in the graph editor (`gv`).
2. Create/select an Epic and bind the topology in its creation form or graph
   workflow tooling.
3. Confirm the graph and prerequisites.
4. Focus the Epic and use `gR` to run its bound topology.
5. Enter the Epic to inspect spawned children and their status.

`gR` is meaningful only for an Epic with a bound topology. It is not a generic
“run everything” command. Read [Using the DAG Feature With an Epic](dag-on-epic-guide.md)
before making topology execution part of a production workflow.

### Lead-driven coordination

`gL` / `:lead` designates a direct leaf child as an Epic's lead. A lead can
coordinate work under server-side hierarchy and topology constraints. It does
not become a general administrator over the daemon or repository.

When a provider integration supports native agent-control tools, the agent uses
those scoped tools. Text-based spawn directives are compatibility plumbing for
some flows, not the preferred mental model for an operator designing everyday
work. The daemon validates child kind, parentage, dependencies, deduplication,
and authority before materializing work.

### Recursive DAG and ProgramRun boundaries

`:dag` opens a read-only recursive DAG browser. It is an advanced inspection
surface, not a replacement for an Epic topology or ordinary session creation.

The ProgramRun kernel is a daemon-owned, operator-only durable execution
system. Its current documentation intentionally describes a narrow kernel with
no normal work publisher. Treat it as internal/specialist infrastructure unless
a feature-specific operator guide says otherwise. Do not use raw ProgramRun RPC
or database mutations as routine project management.

## Agent control and scheduled continuity

### Operator authority versus agent authority

The operator has the broad product control plane through the TUI. A provider
agent has a deliberately narrow, server-authorized control plane. This keeps
an agent from treating the daemon as an unrestricted remote shell.

At the time of writing, `rsi-rpc agent` exposes these agent-facing verbs:

| Verb | Purpose |
|---|---|
| `AgentSpawnChild` | Request a permitted direct child session, optionally with a display-ready `agent_role`; the daemon returns its durable per-Epic ordinal. |
| `AgentGetProgress` | Read durable progress for the agent's child cohort. |
| `AgentGetStatus` | Read the caller/child status view. |
| `AgentHalt` | Halt an authorized child. |
| `AgentContinueChild` | Continue an authorized child with a new prompt, under a required staleness CAS. |
| `AgentScheduleWake` | Request a durable continuation, terminal watch, or program guard. |
| `AgentCreateIssue` | Create a scoped attributed issue follow-up. |
| `AgentSendMessage` | Persist a durable message to an authorized agent/session. |

The daemon resolves caller identity from the session credential in the process
environment. An agent must never put that credential in request parameters or
copy it into a prompt, transcript, log, or issue. Every other generic daemon
method is denied to an agent by default.

Repository cohort listing, audit, apply, and receipt lookup are part of that
default-denied set. Source worktree settlement is global destructive operator
authority and is not exposed through any `Agent*` verb, native provider tool,
or agent CLI command.

The practical authority rule is narrow:

- An Epic lead can act on the Epic's children.
- A leaf can act on itself and its direct children.
- An agent cannot promote itself into broader authority by naming a different
  session ID in a request.

If a child does not appear or a halt/spawn request is rejected, inspect the
actual parentage, lead designation, kind, and topology constraints before
trying again.

For visible pipeline identity, a lead may send an optional normalized
`agent_role` (for example `Researcher`) in `AgentSpawnChild`. The daemon reserves
the positive ordinal atomically with the durable request; committed gaps are
never reused, and exact replay returns the same role and ordinal. The current
lead is presented as `Demiurge`/`0` without rewriting its stored identity.
This display role is not a SessionKind, authority input, or replacement for the
SessionKind-derived `$CLAUDE_AGENT_ROLE`; caller, Epic, parent, and ordinal stay
server-bound. Raw `Session.title` remains separate and recoverable.

Example request:

```json
{"kind":"Task","agent_role":"Reviewer","query":"Review the identity slice.","idempotency_key":"identity-review-v1"}
```

### Reviewing a sandboxed child from its recorded base

An authorized `AgentGetProgress` row may include `base_commit`: the immutable
source commit persisted when that logical child received its sandbox custody
root. When it is present, compare the child against that recorded base, for
example `git diff <base_commit>..<child-ref>`, rather than against a newer
master tip. The progress cursor may follow a rotated tip while `base_commit`
continues to identify the original sandbox allocation. If the field is absent,
no durable sandbox base is available; it is not evidence that the current
master commit is the child's base.

### Scheduled jobs and wakes

Use the schedule browser with `gK` and the jobs zone with `gj` to inspect
operator-visible scheduled work. Scheduled continuation is durable state, so it
should have a clear purpose and outcome, not be a substitute for supervision.

Agent scheduling has explicit modes:

| Mode | Correct use |
|---|---|
| `resume` | Continue the same session later. This is the normal continuity mode. |
| `fresh` | Start a successor after a terminal condition. It is not a second writer for a still-running session. |
| `on_terminal` | Watch a child and react when it reaches a terminal state. |
| `program_guard` | Internal unattended-program sentinel, not normal manual scheduling. |

Do not ask an agent to schedule a `fresh` continuation for a live session. That
can create two writers in one worktree. A scheduled job should name the exact
condition it waits for and be cleared or settled once its work is complete.

### Codex capacity recovery for unattended programs

When an unattended program has a valid daemon program guard and Codex reports
its exact usage-limit condition, rsid pauses the same logical Session lineage
instead of creating a new writer. One outage epoch owns one stable one-shot
`resume` wake and, for a project-backed Session, one attributed issue. It never
converts that wake to `fresh` or `agent_fresh`.

Retries use a bounded, saturating schedule: 60, 120, 240, 480, 960, 1920, then
3600 seconds for every later capacity attempt in the same outage. These are
minimum delays. The scheduler's poll cadence or daemon downtime can make a
dispatch later, but cannot make it earlier. Replayed provider terminals and
replayed due slots do not add an issue, create another wake, advance the delay,
or start a second provider process.

A Session without a project still receives the same durable Resume-only
recovery and backoff, but no issue is filed. Assigning a project during that
open outage does not retroactively create one. A successful non-capacity turn
closes and disarms the outage; typed queue exhaustion or a typed human gate also
closes it and disables the program sentinel. A later independent capacity
outage starts a new epoch at 60 seconds and may file one new issue using the
then-current project assignment.

Capacity delivery records admission and provider-launch confirmation as two
separate durable phases. An admission-only replay reuses the same invocation;
the scheduler retires only the exact due slot after launch is confirmed. Before
that provider boundary, rsid performs checked orphan exclusion and refuses the
launch if it cannot prove that no older provider for the Session survives. If
launch confirmation fails after spawn, rsid kills and checks the unconfirmed
process while leaving the admitted receipt and due slot available for sequential
recovery. SQLite and an operating-system process cannot share one atomic commit,
so this is an explicit safety boundary: at most one launch is durably confirmed,
and replay is permitted only after checked exclusion prevents concurrent
providers.

The capacity incident owns its C5 failure marker in the same Store transaction.
A project-backed outage therefore produces exactly the single capacity issue,
not an additional generic pipeline-failure issue; a projectless outage produces
zero issues. Strict queue-exhausted or human-gate settlement closes the incident,
marks the terminal attempt, disables both the capacity wake and exact program
sentinel, and does not schedule a later launch.

If a recognized capacity wake is malformed, rsid disables and surfaces it
instead of falling through to a fresh launch. Inspect the Session, scheduled
job, attributed issue (when present), and daemon logs. Do not hand-edit the
private capacity tables or manufacture a Fresh successor.

### Issues, notifications, and emergency stop

- Open the local issue tracker with `<Space>i` when a finding needs durable
  follow-up rather than another vague reminder.
- Open notification history with `gn`; use `]a` / `[a` for the attention queue.
- Use `<Space>S` or `:stopall` only as a break-glass control. It prevents new
  paid work and cancels live invocations; investigate the resulting terminal
  state before restarting normal work.

## Settings, diagnostics, and recovery

### Settings

Open settings with `<Space>,` or `:settings`. The setting categories include
display, session defaults, API/models, daemon features, agent actors, hooks,
skills, TUI configuration, memory, message bridges, system prompt, stats, and
budgets. Availability varies with build and configuration.

Change settings through this operator surface. Do not treat hand-edited SQLite
values as a supported configuration API. The daemon owns validation, timestamps,
and cross-feature invariants that raw edits can bypass.

The **⚠ Source worktree settlement** row under **Daemon Features** opens an
operator-only destructive-maintenance overlay. It first performs a bounded,
zero-write audit and can remove a terminal source worktree only when the exact
source tip is a Git ancestor of the current daemon-derived local target. Apply
requires the operator to type the displayed repository-and-digest phrase from
an empty field; the daemon then re-audits before recording durable intent.
Dirty, active, shared, effectful, ambiguous, and non-ancestor roots remain
preserved. Read [Source worktree cohort settlement](cohort-settlement.md)
before using it; a partial or recovery-required receipt is a reason to preserve
the remaining Git state, not to finish cleanup manually.

### Diagnostics workflow

Use `:diag` or `:diagnostics` when a session, navigation view, or TUI feels
wrong. RSI profiling is controlled through `RSI_PROFILE`; set a nonempty,
nonzero value when you need to ensure profiling is active in an environment.

Diagnose in this order:

1. Identify the exact session and inspect `F3`.
2. Read its status, transcript, provider/model, working directory, and sandbox
   context.
3. Open `:diag` and note render, poll, cache, or connection evidence.
4. Test the smallest reproducible action: one new session, one provider, one
   known repository.
5. Preserve the error text and timestamps before restarting processes.

### Common recovery cases

| Symptom | First checks | Avoid |
|---|---|---|
| TUI cannot connect | Daemon process, socket, logs, singleton ownership. | Starting duplicate daemons or deleting the socket/database immediately. |
| Provider fails to launch | Session `F3`, provider/model availability, binary/auth/configuration, minimal fresh session. | Changing a working tree to compensate for an auth/config issue. |
| Session is waiting | Open it, read the question/approval request, answer or interrupt deliberately. | Blindly sending “continue.” |
| Session looks stale | `r`, `rc`, `rm`, then `:diag`. | Assuming persisted state is lost from a visual delay. |
| Agent changed the wrong files | Verify session working directory and sandbox root; interrupt if still active; inspect diff. | Asking it to switch arbitrary branches inside an RSI-owned sandbox. |
| Memory seems absent | Confirm project assignment at launch and memory configuration. | Assuming a global manual search automatically injects context. |
| A feature described in docs is missing | Check compiled help, settings, capability gate, provider, and exact version. | Enabling it with raw database writes. |

### Safe escalation packet

When filing an issue or asking for engineering help, include:

- exact command/key/action and the time it occurred;
- session ID, provider, model, effort, kind, status, project, and sandbox
  details from `F3`;
- visible error text and relevant `:diag` output;
- whether the issue reproduces in a new minimal session;
- whether the worktree contains uncommitted user changes.

Do not paste credentials, session authority tokens, private prompts, or secrets
into an issue.

## Performance and evaluation

### What “performance” means here

RSI performance has at least four separate dimensions:

| Dimension | Question | Useful evidence |
|---|---|---|
| TUI responsiveness | Does navigation/rendering remain fast? | `:diag`, `RSI_PROFILE`, targeted UI tests/benches. |
| Daemon throughput | Does session lifecycle/persistence behave efficiently? | Daemon profiling, focused tests, storage benches. |
| Provider latency | How long does the external agent/model take? | Provider timestamps, model limits, network/account telemetry. |
| Task outcome quality | Does the full system solve the intended work safely? | Frozen task corpus, review, tests, cost, and repeated runs. |

No one number captures all four. A fast UI cannot make a slow provider fast; a
high benchmark score cannot prove a sandbox policy is safe.

### Local microbenchmarks

For repository development, run the maintained microbenchmark suite:

```bash
cargo bench-all
```

It covers focused components such as RPC decoding, UI content/height work,
session restoration, and store-worker flushing. It measures implementation
hotspots, not end-to-end coding-agent quality.

### Harness regression evaluation

The repository also contains `rsi-eval`, an isolated corpus replay tool. It
replays the frozen ticket corpus, captures a baseline, and can gate a candidate
against a baseline threshold. Validate the corpus shape first:

```bash
./scripts/eval-corpus-validate.sh
```

Run evaluations only against an **isolated daemon socket**, never your active
daily daemon by accident. The evaluator intentionally refuses the default socket
unless explicitly overridden. A typical development invocation has the shape:

```bash
RSI_DAEMON_SOCKET_PATH=/path/to/isolated/daemon.sock \
  cargo run -p rsi-eval -- \
  --harness <candidate-label> \
  --capture-baseline /path/to/baseline.json
```

Add `--baseline <baseline.json>` to gate a later candidate, and use the
reproducibility helper when comparing repeated isolated runs:

```bash
RSI_DAEMON_SOCKET_PATH=/path/to/isolated/daemon.sock \
  ./scripts/eval-reproducibility-check.sh
```

Read [the eval corpus guide](../eval/README.md) before treating results as a
decision gate. The current evaluator runs its corpus serially in one working
directory. It is useful for **RSI regression tracking**, but it is not a fair
turnkey comparison against unrelated harnesses with different providers,
sandboxes, prompts, tools, budgets, or scoring rules.

### Fair cross-harness comparison

To compare RSI with another harness, keep these constant:

1. Same repository revision and task corpus.
2. Same model, model version, effort, tool permissions, and time/token budget
   where the providers permit it.
3. Equivalent sandbox and Git policy.
4. Same acceptance tests and human review rubric.
5. Multiple runs per task; report variance, cost, failures, and unsafe actions,
   not only pass rate.

Then separate the result into control-plane overhead, provider/model behavior,
and task success. That gives you a usable engineering comparison rather than a
benchmark-shaped anecdote.

## Command reference and glossary

### High-value controls

This is a working set, not the full binding catalog. Press `?` in the TUI or
read [Keybindings Reference](keybindings.md) for exhaustive, current mappings.

| Goal | Control |
|---|---|
| Launch general work | `<Space>m`, `:blank [objective]` |
| Launch one-shot work | `<Space>o`, `:task [objective]` |
| Choose project | `<Space>p`, `:projects`, `:project <name>` |
| Inspect session facts | `F3` |
| Open/enter selected session | `Enter`, `l`, or `L` |
| Send follow-up | input bar + `Ctrl+Enter`, or `:continue <instruction>` |
| Interrupt | `x`, `:kill` |
| Archive | `<Space>a`, `:archive` |
| Rotate context | `R`, `:rotate` |
| Search | `/`, then `n` / `N` |
| Go to attention | `]a` / `[a` |
| Open settings | `<Space>,`, `:settings` |
| Open diagnostics | `:diag`, `:diagnostics` |
| Open memory search | `<Space>M` |
| Open scheduled jobs | `gK` / `gj` |
| Create hierarchy | `gG`, `gE`, `gS`, `gT`, `gB` |
| Graph/topology editor | `gv` |
| Run an Epic topology | `gR` |
| Assign Epic lead | `gL`, `:lead` |
| Toggle terminal | `Ctrl+\\`, `:term` |
| Emergency global stop | `<Space>S`, `:stopall` |

### Glossary

**Agent:** A provider-run coding process. It is not the operator and has only
the daemon control verbs granted to its session.

**Agent control:** The restricted, server-authorized `Agent*` RPC surface used
by a managed agent for child coordination, status, scheduled continuation, and
issues.

**Archive:** A durable terminal state that removes a session from active work
while retaining its record.

**Context rotation:** A controlled continuation/handoff that refreshes the
context window while retaining durable session lineage.

**Daemon:** The long-lived service that owns lifecycle, persistence, and
provider subprocesses.

**Effort:** A model/provider capability that can alter reasoning allocation;
availability and semantics are model-specific.

**Epic:** A non-spawnable container that holds structured leaf work and can own
a topology.

**Harness:** The control-plane system around an AI agent: session management,
workspace isolation, state, tools, supervision, and evaluation.

**Leaf session:** A session kind that can actually launch a provider.

**Model:** The selected AI model within a provider integration.

**Project:** RSI's durable workstream and project-scoped memory boundary.

**Provider:** The way RSI launches or talks to an AI agent backend.

**Sandbox root:** The isolated filesystem location assigned to a session, often
a Git worktree. It can differ from the broader working directory.

**Session:** RSI's durable unit of work, including prompt, transcript, status,
execution configuration, and context.

**Topology:** A named DAG attached to an Epic that defines child work nodes and
dependencies.

**Working directory:** The required filesystem directory associated with a
session. It is not necessarily the location an isolated agent should write to.

## Related guides

- [Keybindings Reference](keybindings.md) — exhaustive keyboard and command
  behavior.
- [Using the DAG Feature With an Epic](dag-on-epic-guide.md) — detailed
  topology workflow and validation behavior.
- [Memory Architecture](memory-architecture.md) — memory indexing, scope, and
  configuration details.
- [ProgramRun kernel](program-run-kernel.md) — internal/operator-only durable
  execution kernel boundaries.
- [Evaluation corpus guide](../eval/README.md) — corpus, baseline, and
  reproducibility details.

## Operating principle

Use the smallest control plane that can safely express the work. One clear
session beats a premature hierarchy; a reviewed sandboxed change beats a fast
shared-tree edit; a measured regression result beats an impression; and a
visible capability boundary beats an undocumented assumption.
