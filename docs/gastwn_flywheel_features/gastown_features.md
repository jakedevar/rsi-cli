# Gastown Features

## Multi-Agent Workspace Management (Town/Rig/Crew)
**Category:** Workspace
**Description:** Gastown implements a hierarchical workspace model with three levels. The Town (`~/gt/`) is the top-level headquarters directory containing all projects, agents, and configuration. A Rig is a project-specific Git repository container under Gas Town management, holding its own polecats, refinery, witness, and crew members. Crew members are named, persistent human workspaces within a rig with their own git clone. The `gt install [path] --git` command bootstraps the full structure, creating `CLAUDE.md`, `mayor/`, `.beads/`, and configuration files. Multiple rigs are registered in `mayor/rigs.json`.

## Mayor — Global AI Coordinator
**Category:** Agent Management
**Description:** The Mayor is the singleton global coordinator agent running from the town root directory. Implemented as a Claude Code session in a tmux window, the Mayor has full visibility across all rigs and serves as the primary interface between the human user and the automated agent system. The Mayor creates convoys, distributes work via `gt sling`, receives escalations from Witnesses and the Deacon, and notifies the user of important events. Commands: `gt mayor start|stop|attach|restart|status`. The `--agent` flag overrides the default Claude runtime. The Mayor is not a project worker — it is an orchestrator.

## Deacon — Background Supervisor Daemon
**Category:** Agent Management
**Description:** The Deacon is a persistent background supervisor daemon that forms the top of the watchdog chain. It runs continuous Patrol cycles, monitoring all Witnesses and ensuring worker activity across all rigs. The Deacon manages "Dogs" — infrastructure helper agents for cross-rig tasks. It receives escalations from Witnesses, triggers recovery for failed agents, and enforces system-level health policies. The Deacon is NOT a scheduler — all intelligence lives in agents; the Deacon is a "dumb" heartbeat/lifecycle manager. The Boot Dog checks the Deacon every 5 minutes to ensure the watchdog itself is alive.

## Witness — Per-Rig Polecat Health Monitor
**Category:** Agent Management
**Description:** Each rig has one Witness agent (`gt witness start <rig>`) that monitors its polecats with a patrol loop. The Witness detects stalled polecats (crashed mid-work), nudges unresponsive sessions back to life, cleans up zombie polecats (finished but failed to exit cleanly), and nukes sandboxes after polecats complete work via `gt done`. The Witness does NOT force session cycles or interrupt working polecats — it handles failures and edge cases only. One Witness per rig. The Deacon monitors all Witnesses. Role shortcuts ("witness") in mail/nudge addresses resolve to the rig's Witness.

## Polecats — Persistent-Identity Ephemeral-Session Workers
**Category:** Agent Management
**Description:** Polecats are the primary project worker agents in Gastown. Each polecat has a permanent agent bead (identity) and CV chain accumulating work history across assignments, but sessions and git worktree sandboxes are ephemeral — spawned for specific tasks, cleaned up on completion. Polecat states: working (active), stalled (crashed mid-work), zombie (finished but cleanup failed), nuked (session ended, identity preserved ready for next assignment). The self-cleaning model: polecat runs `gt done` on completion, which pushes the branch and submits to the merge queue. Witness then nukes the sandbox. Up to 20-30 polecats can work in parallel.

## Refinery — Per-Rig Merge Queue Processor
**Category:** Agent Management
**Description:** The Refinery is the per-rig merge queue processor, serializing all merges to main. It receives MRs submitted by polecats via `gt done`, rebases work branches onto latest main, runs validation (tests, builds, checks), and merges to main when clear. On conflict, the Refinery spawns a fresh polecat to re-implement the work from scratch (the conflicting polecat is already nuked by that point). The Refinery implements a Bors-style batch-then-bisect strategy: it batches multiple MRs, runs tests on the tip commit, and binary-bisects to isolate failures. One Refinery per rig. `gt refinery start|stop|attach|status`.

## Dogs — Cross-Rig Infrastructure Workers
**Category:** Agent Management
**Description:** Dogs are persistent-identity reusable workers managed by the Deacon for infrastructure and cleanup tasks that span rigs or require system-level access. Unlike polecats (which build features in one rig), dogs clean up messes across rigs. Dogs handle infrastructure tasks (rebuilding, syncing, migrations), cleanup operations (orphan branches, stale files), and cross-rig work spanning multiple projects. Multiple named dog types exist: Compactor Dog (daily Dolt commit flattening), Doctor Dog (automated health monitoring), JSONL Dog (spike detection and pollution firewall), Wisp Reaper Dog (automated wisp garbage collection). Dogs are reusable — they cycle through multiple tasks before being recycled.

## Beads — Git-Backed Issue Tracking Integration
**Category:** Work Tracking
**Description:** Gastown deeply integrates with the Beads system (`bd` CLI), a git-backed issue tracking database. Beads serve as the fundamental work unit, stored in Dolt databases at `.beads/`. Bead IDs use a prefix+5-char alphanumeric format (e.g., `gt-abc12`, `hq-x7k2m`) where the prefix identifies the rig of origin. Gastown's `gt sling`, `gt convoy`, `gt hook`, and all work-dispatch commands accept bead IDs as first-class arguments. The `bv` (beads-view) CLI provides graph analysis: `--robot-triage`, `--robot-next`, `--robot-plan`, `--robot-alerts`, `--robot-insights`. All work state is persisted in git-backed Dolt commits.

## Convoy System — Work Tracking Units
**Category:** Work Tracking
**Description:** Convoys are the primary work-tracking units in Gastown, bundling related beads (issues) into a single trackable unit. `gt convoy create "Title" <bead-ids> [--notify agent] [--human]` creates a convoy. `gt convoy list` shows all active convoys with status. `gt convoy status <convoy-id>` shows detailed progress. When all beads in a convoy close, the convoy automatically lands. Convoys support cross-rig tracking (a convoy in the hq-* namespace can reference issues from any rig). Auto-convoy creation on sling: when slinging a single issue, Gastown creates a "Work: <title>" convoy automatically unless `--no-convoy` is specified. Convoy IDs are 3-character base36 strings.

## Sling — Unified Work Dispatch
**Category:** Work Dispatch
**Description:** `gt sling` is the primary work dispatch command for assigning beads to agents. It handles: existing agents (mayor, crew, witness, refinery), auto-spawning polecats when targeting a rig, dispatching to dogs via deacon/dogs, formula instantiation and wisp creation, and auto-convoy creation. Target resolution supports: self (no target), crew worker (`crew`), rig-level polecat auto-spawn (`rigname`), specific polecat (`rig/polecat`), mayor, and specific dog (`deacon/dogs/name`). Merge strategy (`--merge=direct|mr|local`) controls how completed work lands. Natural language args (`--args "focus on security"`) and stdin mode for complex messages are supported.

## Hooks — Git-Backed Persistent Work Storage
**Category:** Work Storage
**Description:** Hooks are the "durability primitive" in Gastown — pinned beads serving as each agent's primary work queue. Work placed on a hook survives session restarts, context compaction, and handoffs. When an agent restarts (via `gt handoff`), its SessionStart hook runs `gt prime`, which finds the attached work and continues from where it left off. `gt hook <bead-id>` attaches work to your hook; `gt hook status` shows current hook content. `gt unsling` removes work from the hook. Hooks are stored in Dolt as pinned beads, providing the git-backed durability guarantee.

## Prime — Session Context Restoration
**Category:** Session Management
**Description:** `gt prime [--hook]` detects the agent role from the current directory and outputs full session context. Role detection covers: town root (neutral), `mayor/` (Mayor), `<rig>/witness/rig/` (Witness), `<rig>/refinery/rig/` (Refinery), `<rig>/polecats/<name>/` (Polecat), and crew directories. In hook mode (`--hook`), it reads session metadata from stdin (Claude Code sends `{"session_id": "uuid", "source": "startup|resume|compact"}`) and persists it. Prime reads the hook, checks for handoff markers, loads in-progress molecules, and outputs role-appropriate context. It is configured as a Claude Code `SessionStart` hook to run at every session start.

## Handoff — Session Refresh and Context Transfer
**Category:** Session Management
**Description:** `gt handoff [bead-or-role]` is the canonical session-ending command for all roles. For Mayor, Crew, Witness, Refinery, Deacon: respawns with a fresh Claude instance in a new tmux session. For Polecats: calls `gt done --status DEFERRED` (Witness handles lifecycle). When given a bead ID, hooks that work first then restarts. The `--collect (-c)` flag gathers current state (hooked work, inbox, ready beads) and includes it in the handoff mail. The `--cycle` flag triggers automatic session cycling, used by PreCompact hooks for all roles. Any molecule on the hook is auto-continued by the new session via `gt prime`.

## Seance — Predecessor Session Communication
**Category:** Session Management
**Description:** `gt seance` enables agents to literally talk to predecessor sessions by spawning a Claude subprocess that resumes a predecessor session with full context. The command discovers recent sessions from event logs, filterable by role (`--role crew`) or rig (`--rig gastown`). `--talk <session-id>` spawns an interactive conversation; `--talk <id> -p "question"` sends a one-shot query. Internally, it executes `claude --fork-session --resume <id>`, loading the predecessor's full context without modifying their session. This enables agents to query previous decisions, discover work in progress, and understand context from prior sessions.

## Nudge — Real-Time Agent Messaging
**Category:** Communication
**Description:** `gt nudge <target> [message]` sends synchronous messages to any Gas Town worker. Three delivery modes: `immediate` (sends directly via tmux send-keys, interrupts in-flight work), `queue` (writes to a file queue picked up at next turn boundary, zero interruption), `wait-idle` (waits for agent to become idle then delivers, falls back to queue on timeout). Supports `--force` to override DND (Do Not Disturb), `--if-fresh` to suppress compaction nudges, and `--stdin` for multi-line messages. Nudge is used for protocol messages, work completion notifications, and escalations. Auto-nudge on mail delivery wakes idle agents.

## Mail System — Agent Messaging
**Category:** Communication
**Description:** The mail system provides persistent asynchronous messaging between agents stored as Dolt bead issues of type=message. Messages route through a hierarchical directory structure: mayor inbox at `mayor/`, rig-level inboxes at `gastown/<role>`. Commands: `gt mail send <recipient> [message]`, `gt mail inbox`, `gt mail check`, `gt mail thread`, `gt mail search`, `gt mail archive`, `gt mail drain`. Messages support: priority (urgent flag), wisp/permanent classification, CC recipients, reply-to threading, and auto-nudge notification to idle recipients. A mail drain command empties the inbox. The system distinguishes nudges (ephemeral) from mail (persistent bead records).

## Formula System — Reusable Workflow Templates
**Category:** Work Dispatch
**Description:** Formulas are TOML/JSON workflow source templates defining reusable patterns for operations like patrol cycles, code review, or deployment. They define steps, variables, and composition rules, and can be "poured" to create Molecules or "wisped" for ephemeral patrol cycles. `gt formula list` shows all formulas from three search paths (project `.beads/formulas/`, user `~/.beads/formulas/`, orchestrator `$GT_ROOT/.beads/formulas/`). `gt formula run <name>` pours and dispatches a formula. `gt formula create` generates a new template. Formulas can reference natural language arguments that agents interpret as instructions.

## Molecule System — Durable Chained Workflows
**Category:** Work Dispatch
**Description:** Molecules are durable chained bead workflows representing multi-step processes where each step is tracked as a bead. They survive agent restarts, ensuring complex workflows complete even through session cycling. Commands under `gt mol` (alias `gt molecule`): `attach`, `detach`, `burn`, `squash`, `step done`, `progress`, `current`, `await-event`, `await-signal`, `emit-event`, `status`. A Molecule has a root bead (the molecule ID) with child beads for each step. Step completion auto-advances the molecule. `gt mol squash` compresses a completed molecule to a permanent digest bead. The Witness monitors molecule progress to detect stalled workflows.

## Wisp System — Ephemeral Work Items
**Category:** Work Tracking
**Description:** Wisps are ephemeral beads created for transient operations that don't need permanent tracking. They are automatically destroyed after use, unlike permanent beads that persist in git. Wisps are used for: patrol cycle digests (individual cycle summaries before daily aggregation), ephemeral nudge-type protocol messages, and formula steps that don't need permanent records. The Root-only wisp model (as of 0.9.0) creates single root wisps per formula rather than materializing individual step wisps, reducing Dolt commit volume by ~6,000 rows/day. The Wisp Reaper Dog auto-deletes closed wisps older than 7 days.

## Merge Queue (MQ) — Branch Integration
**Category:** Merge Management
**Description:** The merge queue manages polecat work branches awaiting integration. `gt mq submit` submits the current branch with epic/issue association. `gt mq list` shows queued entries with status (ready, waiting, blocked). `gt mq status` shows queue health. The Bors-style batch-then-bisect algorithm: batch MRs together, run tests on the tip commit, and on failure binary-bisect to isolate the failing MR. `GatesParallel` runs test + lint concurrently per MR. The Refinery processes the MQ for its rig. `gt mq post-merge` handles branch cleanup. `gt mq integration` manages cross-rig integration branches.

## Scheduler — Capacity-Controlled Dispatch
**Category:** Work Dispatch
**Description:** The dispatch scheduler provides capacity-controlled polecat spawning. `gt config set scheduler.max_polecats N` enables deferred dispatch, preventing more than N simultaneous polecats per rig. When the cap is hit, new slings are queued in the scheduler and dispatched as slots open. `gt scheduler status` shows current capacity and pending queue. `gt scheduler list` shows all scheduled beads. `gt scheduler run` manually triggers a dispatch cycle. `gt scheduler pause|resume` halts/resumes dispatch. `gt scheduler clear <bead>` removes specific beads from the queue. With `max_polecats=-1` (default), dispatch is immediate.

## Dolt Server — Multi-Client Beads Database
**Category:** Infrastructure
**Description:** Gastown uses Dolt (a MySQL-compatible version-controlled SQL database) as the storage backend for beads. The `gt dolt start|stop|status|logs` commands manage a background Dolt SQL server on port 3307. Each rig has its own database in `.dolt-data/`. Multi-client mode avoids the single-writer limitation of embedded Dolt. `gt dolt init` scans and repairs Dolt workspace configuration. `gt maintain` (one command) runs flatten + gc. The Compactor Dog performs daily commit history flattening via `DOLT_RESET --soft` on the live server. Maximum connections default to 1,000. Log rotation with gzip compression is automatic.

## Doctor — Workspace Health Checks
**Category:** Operations
**Description:** `gt doctor [--fix] [--rig <name>]` runs comprehensive diagnostic checks across the workspace with optional auto-remediation. Checks cover: workspace config validity, rig registry, mayor structure, town git/branch protection, pre-checkout hook, binary staleness, beads binary version, daemon health, boot watchdog, orphan sessions/processes, session name format, wisp GC, misclassified wisps, JSONL bloat, stale beads redirects, clone divergence, default branch existence, worktree validity, crew state files, crew worktrees, sparse checkout migration, and per-rig checks (git repo validity, bare repo, witness/refinery structure, polecat clones, beads config). Most checks are fixable with `--fix`.

## Daemon — Background Heartbeat/Lifecycle Manager
**Category:** Infrastructure
**Description:** The Gas Town daemon is a lightweight background process providing agent heartbeats and lifecycle management. It pokes agents periodically, processes lifecycle requests (cycle, restart, shutdown), and restarts sessions when agents request cycling. Unlike a scheduler, the daemon is explicitly "dumb" — all intelligence lives in agents. `gt daemon start|stop|status|logs|reload|clear-backoff` manage the daemon. `gt daemon clear-backoff` resets exponential backoff on restart loops. Log rotation with configurable size thresholds is built in. The daemon's PID is tracked via nonce-based PID files (replacing fragile `ps` string matching).

## Checkpoint — Session Crash Recovery
**Category:** Session Management
**Description:** `gt checkpoint write|read|clear` manages session checkpoints for polecat crash recovery. A checkpoint captures: current molecule and step, hooked bead, modified files list, git branch and last commit, and timestamp. Checkpoints are stored in `.polecat-checkpoint.json` in the polecat directory. When a session crashes, the next session can read the checkpoint via `gt prime` and resume from the exact state before the crash. Checkpoints are written after closing molecule steps, periodically during long sessions, and before handoff to another session. `gt checkpoint clear` removes the file after successful work completion.

## Costs — LLM Usage Analytics
**Category:** Analytics
**Description:** `gt costs` calculates and displays costs for Claude Code sessions by parsing transcript files at `~/.claude/projects/`, summing token usage from assistant messages, and applying model-specific pricing. Flags: `--today` (current day from log file), `--week` (this week from digest beads + today's log), `--by-role` (breakdown by role: polecat, witness, etc.), `--by-rig` (breakdown by rig). The `gt costs record` command logs cost data for a session/work-item. `gt costs digest` aggregates daily cost data into permanent bead records. This provides multi-day cost tracking without requiring external LLM API dashboards.

## Signal System — Agent Communication Protocol
**Category:** Communication
**Description:** The signal system (`gt signal stop`) provides structured inter-agent signals distinct from nudges and mail. `gt signal stop` sends a stop signal to a polecat or agent process. Signals use structured data rather than plain text, enabling deterministic routing and processing by agent hook handlers. `gt mol await-signal` suspends molecule execution until a specific signal is received. `gt mol emit-event` sends events to waiting steps. The signal system supports the Stalled Polecat Detection mechanism — replacing screen-scraping with structured heartbeat-based liveness checks (as of 0.9.0).

## Tap — Claude Code Hook Handlers
**Category:** Integration
**Description:** `gt tap guard|audit|inject|check` implements Claude Code PreToolUse/PostToolUse hook handlers that intercept the tool execution flow. `tap guard` blocks forbidden operations (exit code 2 to cancel). `tap guard dangerous` blocks interactive commands (`cp -i`, `mv -i`, `rm -i`) that would cause agent sessions to hang waiting for user input. Hook configuration in `.claude/settings.json` uses matchers like `Bash(gh pr create*)`. The tap system enables policies, auditing, and input transformation without modifying agent code. This integrates Gas Town's governance layer with Claude Code's native hook system.

## Prime Context System — Role-Aware Session Bootstrap
**Category:** Session Management
**Description:** The prime system detects agent roles from directory structure and outputs rich, role-appropriate context at session start. Role detection logic: town root → neutral, `mayor/` → Mayor role, `<rig>/witness/` → Witness role, `<rig>/refinery/` → Refinery role, `<rig>/polecats/<name>/` → Polecat role, `<rig>/crew/<name>/` → Crew role, `deacon/dogs/<name>/` → Dog role. For each role, prime outputs the current hook contents, molecule progress, pending mail summary, ready beads count, recent commit history, and role-specific instructions. Prime also handles handoff markers left by previous sessions for context continuity through compaction.

## Install — Workspace Bootstrapping
**Category:** Setup
**Description:** `gt install [path] [--git]` creates a complete Gas Town HQ directory structure. With `--git`, initializes git in the workspace root. Creates: `CLAUDE.md` (Mayor role context), `mayor/` directory with `town.json` config and rig registry, `.beads/` town-level bead database. Options: `--name` (workspace name), `--owner` (owner name), `--public` (GitHub Pages exposure), `--shell` (install shell helpers like `gt` alias). `gt rig add <name> <git-url>` adds a project by cloning a repository and setting up the full rig structure including refinery clone, mayor clone, witness directory, polecats directory, `.beads/`, and patrol molecule seeds.

## Rig Management
**Category:** Workspace
**Description:** Rigs are managed via `gt rig add|list|dock|park|settings|detect` commands. `gt rig add` clones a repository and creates the full rig structure. `gt rig dock` marks a rig as active for dispatch. `gt rig park` marks a rig as parked (inactive, excluded from auto-dispatch). `gt rig settings` manages per-rig configuration (default model, worker agents, capacity limits). `gt rig detect` determines the current rig from the working directory. Rigs are registered in `mayor/rigs.json`. The `--adopt` flag adopts an existing directory as a rig without cloning. Rig prefixes (e.g., `gt`, `gp`) are used for bead ID namespacing.

## Crew Management
**Category:** Agent Management
**Description:** `gt crew add <name> [--rig <rig>]` creates a named persistent crew workspace within a rig. Unlike ephemeral polecats, crew members have persistent clone state and are intended for long-running collaborative work. `gt crew list` shows all crew members. `gt crew at <name>` navigates to a crew directory. `gt crew cycle` performs a session cycle for a crew member (equivalent to handoff + restart). `gt crew maintenance` performs maintenance tasks (cleanup stale worktrees, update state files). Crew members can have per-worker `worker_agents` configuration overriding the default agent command.

## Wasteland — DoltHub Federation
**Category:** Federation
**Description:** The Wasteland is a federation of Gas Towns via DoltHub, enabling distributed work sharing. Each rig has a sovereign fork of a shared commons database containing a wanted board (open work), rig registry, and validated completions. `gt wl join <upstream>` joins a wasteland by forking the upstream commons database to your DoltHub org, cloning it locally, registering your rig, and saving configuration. `gt wl browse` displays available work items. `gt wl claim <bead>` claims work from the commons. `gt wl post <bead>` posts completed work back to the commons. `gt wl done <bead>` marks work done and updates reputation tracking.

## Config — Agent and Workspace Configuration
**Category:** Configuration
**Description:** `gt config agent list|get|set|remove` manages agent aliases and configuration. Built-in agents include claude, gemini, and codex. Custom agents can be configured with arbitrary command lines. `gt config default-agent [name]` sets the default agent for new sessions. Config is stored in `town.json`. `gt config set scheduler.max_polecats N` and other dot-path keys manage operational parameters. The OperationalConfig system (as of 0.9.0) moves hardcoded thresholds (hung sessions, stale claims, max retries, crash-loop backoff) into `operational.json` so agents can tune them without code changes.

## Telemetry — Command Usage Logging
**Category:** Observability
**Description:** Gastown records command usage to `~/.gt/cmd-usage.jsonl` (JSONL format) for every command invocation except excluded ones (`tap`, `signal` which fire per-tool-use). Each record contains: `ts` (RFC 3339 timestamp), `cmd` (full command path), `actor` (from `GT_ROLE` env var), `argc` (argument count). OpenTelemetry integration via `go.opentelemetry.io/otel` supports OTLP export of metrics and logs to external collectors. The telemetry system is fire-and-forget — errors are silently ignored to prevent command failure due to observability issues.

## Agent State Management
**Category:** Agent Management
**Description:** `gt agent-state` (internal) manages the JSON state files for each agent's runtime configuration. State files track: current molecule attachment, session ID, handoff marker status, role, working directory, and last heartbeat time. The `gt agents` command shows all active agents across all rigs with their states. Agent state is used by prime, handoff, and the Witness to determine agent liveness and current work. As of 0.9.0, PID detection uses nonce-based PID files rather than process name matching for reliable identification.

## Activity Feed
**Category:** Observability
**Description:** `gt feed` shows a real-time activity feed of agent actions across all rigs. The feed aggregates events from the Dolt event log, showing session starts/completions, work assignments, convoy updates, and system messages. `gt feed --rig <name>` filters to a specific rig. The feed provides the "what's happening right now" view for a human overseer monitoring multiple simultaneous agents without reading individual tmux sessions. Related: `gt activity` shows a historical activity view.

## Patrol Digest Aggregation
**Category:** Operations
**Description:** `gt patrol digest [--yesterday|--date YYYY-MM-DD]` aggregates ephemeral per-cycle patrol digests (created by Deacon, Witness, Refinery patrol loops) into a single permanent daily summary bead. This daily aggregation compresses the high-frequency ephemeral patrol data into a permanent audit trail. The resulting digest bead is synced via git. Patrol digests serve as the operational history for the system. `gt patrol new` triggers a new manual patrol cycle. The daily aggregation is typically run by the Deacon patrol agent.

## Plugin System
**Category:** Extensibility
**Description:** Gastown supports a plugin system for extending functionality with custom formulas and commands. Plugin search paths: project-level `.beads/formulas/` (in rig), user-level `~/.beads/formulas/`, orchestrator-level `$GT_ROOT/.beads/formulas/`. Plugins are Go packages in `plugins/` at the repository root: `compactor-dog`, `dolt-archive`, `github-sheriff`, `rebuild-gt`, `session-hygiene`. The `gt plugin` command manages plugin installation. The plugin system enables teams to add custom patrol behaviors, formula types, and dog implementations without modifying the core gastown binary.

## DND (Do Not Disturb)
**Category:** Communication
**Description:** `gt dnd on|off|status` manages the Do Not Disturb flag for an agent. When DND is enabled, nudges are suppressed unless sent with `--force`. This allows agents to block interruptions during focus periods or when executing critical operations that should not be interrupted mid-stream. The DND state is stored per-agent and checked by the nudge delivery system before sending. Auto-nudges on mail delivery respect DND settings. The `--if-fresh` flag on nudge sends only to sessions less than 60 seconds old, suppressing compaction-triggered nudge storms.

## Health Monitoring (vitals)
**Category:** Operations
**Description:** `gt vitals` provides a unified health dashboard summarizing the entire Gas Town workspace state: daemon status, Dolt server status, active polecats per rig, Witness and Refinery status, recent error counts, and active convoy summary. `gt health` performs targeted health checks for specific subsystems. The Doctor Dog performs automated health monitoring patrols, detecting zombies, orphan databases, and stale locks. Health data is returned as structured JSON for agent decision-making (ZFC-compliant — no hardcoded thresholds). The health system integrates with the Deacon's Operational Config for configurable alert thresholds.

## Six-Stage Data Lifecycle
**Category:** Data Management
**Description:** Gastown implements a six-stage data lifecycle for bead data management: CREATE (bead is created, active work), LIVE (actively being worked), CLOSE (work completed), DECAY (cooling period before archival), COMPACT (data compacted, history flattened), FLATTEN (Dolt commit history flattened via DOLT_RESET --soft). Each stage is automated via Dogs. `EnsureLifecycleDefaults()` auto-populates `daemon.json` with patrol entries on startup. The Wisp Reaper handles DELETE (closed wisps > 7 days), Auto-close (stale issues > 30 days), and Mail purge (> 7 days). This prevents unbounded growth of ephemeral data.

## Compact — Session History Flattening
**Category:** Operations
**Description:** `gt compact` flattens per-polecat session history to prevent unbounded growth in long-running agents. Compact operations include: squashing completed molecule history into digest beads, pruning old ephemeral wisps, and triggering the Compactor Dog for Dolt-level commit history flattening. The Compactor Dog uses `DOLT_RESET --soft` to flatten commit history on the live server without downtime, configurable threshold (default 10,000 commits). Surgical interactive Dolt rebase is available for advanced cases. `gt compact report` shows what would be compacted.

## Escalate — Blocked Work Recovery
**Category:** Work Dispatch
**Description:** `gt escalate <bead-id>` handles blocked work items by escalating them to a higher-level agent. When a polecat encounters an unresolvable situation (emits `[TASKRABBIT_ESCALATE]` or similar signals), the escalation system routes the bead to the Witness, which may escalate to the Mayor. `gt escalate impl` provides the implementation details for escalation routing. The escalation chain: Polecat → Witness → Mayor → Human. Each escalation level has more authority to unblock the work but also more cost in terms of human attention. Escalation history is tracked in the bead's audit trail.

## Signal Stop — Graceful Agent Termination
**Category:** Communication
**Description:** `gt signal stop <target>` sends a graceful stop signal to a specific agent's tmux session. Unlike kill, signal stop gives the agent time to complete its current turn, write a handoff, and exit cleanly. The signal is delivered via a structured message rather than a Unix signal, allowing the agent to respond intelligently (e.g., finish the current step before stopping). Signal stop is used by the Witness for clean polecat termination and by the Deacon for ordered shutdown sequences. The `--no-wait` flag sends without waiting for acknowledgment.

## Remember/Forget — Agent Key-Value Memory
**Category:** Memory
**Description:** `gt remember <key> <value>` and `gt forget <key>` manage a key-value store within the beads system, namespaced under `memory.*` keys. `gt memories [search-term]` lists or searches all stored memories. This enables agents to persist small pieces of information across sessions without requiring full memory file creation. Memories are stored as bead key-value pairs, inheriting beads' git-backed durability. The memory search uses case-insensitive substring matching on both keys and values. This is complementary to MEMORY.md files — memories store structured key/value data while MEMORY.md stores freeform markdown notes.

## Orphan Detection and Cleanup
**Category:** Operations
**Description:** `gt orphans [--fix]` detects and optionally cleans up orphaned resources across the workspace. Orphan types: sessions (tmux sessions without matching agent state), processes (Claude processes not tracked by any session), beads (bead records referencing non-existent polecats), and worktrees (git worktrees pointing to deleted branches). Doctor's `orphan-sessions` and `orphan-processes` checks detect session and process orphans. The Wisp Reaper Dog handles wisp orphans. Orphan cleanup is also triggered by the Doctor with `--fix`. The Witness handles orphaned polecat sandboxes.

## Upgrade — Post-Install Migration
**Category:** Setup
**Description:** `gt upgrade` is the post-binary migration orchestrator run after updating the Gas Town binary. It propagates configuration changes from new defaults to existing installations, applies schema migrations, seeds new required patrol entries in daemon.json, and verifies the installation is consistent with the new version's requirements. `gt version --verbose` shows full build information including commit hash and build date. `gt version --short` shows just the version number. The upgrade command is safe to run multiple times (idempotent) and the doctor checks for stale binary detection.
