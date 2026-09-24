# Gastown Capabilities Unique to Gastown (Candidates for Flywheel Implementation)

This document lists features that exist in Gastown but have no equivalent in Flywheel, along with proposed implementation approaches for the Flywheel context (single-user Rust/TUI tool).

---

## Multi-Agent Orchestration (Convoy + Sling)
**Gastown Implementation:** Convoys bundle related beads (issues) into trackable work units. `gt sling` dispatches beads to agents. Multiple polecats work in parallel, tracked by a single convoy. Auto-convoy creation on sling provides automatic work tracking. The convoy system enables 20-30 simultaneous AI agents with coherent oversight.
**Proposed Flywheel Implementation:** Flywheel could implement a lighter-weight "convoy" concept as a named group of sessions with a shared goal. The `LaunchSession` RPC already supports `project_id` and `continued_from` for chaining; a `convoy_id` field could group sibling sessions. The merge queue already tracks sessions ready to land. A "session group" overlay could show all sessions in a group with aggregate status. The `Space+X` (CommitAndPush) and docregblock execution already demonstrate the idea of launching related sessions from output.
**Priority:** Medium
**Rationale:** Flywheel's power user frequently runs multiple Claude sessions in parallel. A lightweight grouping mechanism (without Gastown's full multi-agent complexity) would improve oversight and reduce cognitive load when managing 5-15 simultaneous sessions.

---

## Witness — Automated Session Health Monitoring
**Gastown Implementation:** The Witness is a per-rig patrol agent that monitors polecats, detects stalled sessions (crashed mid-work), sends nudges to unresponsive sessions, and cleans up zombies. It does NOT force cycles but handles failures and edge cases. One Witness per rig runs continuously.
**Proposed Flywheel Implementation:** The flywheeld daemon could implement a lightweight session health monitor. After a configurable inactivity timeout (e.g., 30 minutes with `Running` status but no new events), the daemon could emit a `SessionStalled` bus event or change status to a new `Stalled` variant. The TUI could render stalled sessions distinctly (e.g., amber color in the session list) and expose a one-keystroke "nudge" action to send a "continue" message. The daemon already has the `monitor.rs` infrastructure and the event bus for notifications.
**Priority:** High
**Rationale:** Single-user Flywheel sessions frequently stall mid-task without the user noticing, especially when running many parallel sessions. Automated stall detection would significantly reduce the overhead of managing multiple sessions simultaneously.

---

## `gt seance` — Predecessor Session Querying
**Gastown Implementation:** `gt seance` spawns a Claude subprocess that resumes a predecessor session with full context via `claude --fork-session --resume <id>`. This allows asking questions of previous sessions: "Why did you make this decision?", "Where were you stuck?", "What did you try that didn't work?"
**Proposed Flywheel Implementation:** Flywheel already stores full conversation histories in SQLite. A "resume read-only" session type could be added: `LaunchSession` with a `resume_session_id` and a `read_only: true` flag would launch Claude in a read-only context-loading mode. The TUI could expose this via a `gS` keybinding ("seance" with selected session) that opens a new session seeded with the target session's transcript. Alternatively, a simpler approach: the `GetConversation` RPC could return the full history which the TUI could display in a special "historical view" pane.
**Priority:** Medium
**Rationale:** When continuing a previously interrupted session or debugging why a session made particular decisions, the ability to query the session's history is extremely valuable. Flywheel users currently must scroll through the detail view manually.

---

## Durable Hook System (Work Survives Session Restarts)
**Gastown Implementation:** Each agent has a "hook" — a pinned bead representing their current work. When a session crashes or is cycled (handoff), `gt prime` reads the hook at session start and restores context automatically. Work state in the hook survives any number of session restarts, compactions, or handoffs. The hook is the "durability primitive."
**Proposed Flywheel Implementation:** Flywheel's `Session.handoff_filepath` and `Session.continued_from` already partially implement this. A more complete implementation would add a `session.active_task` field (a short string describing current work) that persists in SQLite and is injected as a system prompt when continuing a session. The `RotateSession` workflow already does handoff document detection. Extending this to support user-authored "work context" strings would provide hook-like durability for single sessions.
**Priority:** High
**Rationale:** Flywheel users frequently lose context when sessions hit their context window limit and must be rotated. A lightweight "work context" that survives rotation would make the rotation experience seamless.

---

## Formula System — Reusable Workflow Templates
**Gastown Implementation:** Formulas are TOML/JSON files defining named workflows with steps, variables, and composition rules. They are "poured" to create molecules or "wisped" for ephemeral cycles. Formulas can be shared across teams, versioned in git, and instantiated with natural language arguments.
**Proposed Flywheel Implementation:** Flywheel's prompt overlay already has slash-command suggestions. Extending this with a "templates" system would allow users to define named prompt templates (stored in `~/.flywheel/templates/`) that appear in the slash-command list. A template could define a title, a body with `{{variable}}` placeholders, and defaults. The `Space+o` (TaskRabbit) system is already a specialized template. A general template system would generalize this. Templates could be stored as TOML files similar to Gastown's formulas.
**Priority:** Medium
**Rationale:** Power users repeatedly compose similar prompts (code review, bug investigation, feature implementation). Reusable templates with variable substitution would save significant time and improve prompt quality consistency.

---

## Molecule System — Durable Multi-Step Workflows
**Gastown Implementation:** Molecules are durable chained workflows where each step is a tracked bead. Steps survive session restarts. `gt mol step done` auto-advances. `gt mol squash` compresses history to a digest. The Witness monitors molecule progress to detect stalled workflows.
**Proposed Flywheel Implementation:** Flywheel's docregblock system already enables session chaining. A molecule-like feature could track a `pipeline` of sessions: `Session.pipeline_artifact` already detects artifact files. A `pipeline_id` field on sessions (grouping them as steps in a workflow) combined with a pipeline status overlay showing step completion would provide lightweight molecule tracking. The `Space+x` (ExecuteDocRegBlocks) action already launches child sessions from output — formalizing this as a named pipeline with progress tracking would complete the feature.
**Priority:** Low
**Rationale:** Flywheel's primary use case is single interactive sessions, not long multi-step pipelines. However, for research/planning workflows (the `thoughts/shared/` artifact system), lightweight pipeline tracking would be useful for power users.

---

## Cost Tracking and Analytics
**Gastown Implementation:** `gt costs [--today|--week|--by-role|--by-rig]` aggregates costs from Claude transcript files, applying model-specific pricing. Daily digests are created as permanent bead records. Cost breakdown by role and rig enables team-level cost management.
**Proposed Flywheel Implementation:** Flywheel already tracks `cost_usd`, `input_tokens`, `output_tokens`, `total_cache_creation_tokens`, and `total_cache_read_tokens` per session. The `StatusBarSegment::Cost` shows a live total. Missing: historical trends (costs by day/week), per-model cost breakdown, and export. A `GetCostSummary` RPC could aggregate session costs by time period. A `:costs [--today|--week]` command mode command could display a formatted cost breakdown in a new pane or overlay. The data is already there; the aggregation and presentation layer is missing.
**Priority:** Medium
**Rationale:** LLM costs can grow quickly when running many parallel sessions. Historical cost tracking helps users identify expensive workflows and optimize prompt length and session count.

---

## Wasteland Federation — Distributed Work Sharing
**Gastown Implementation:** The Wasteland is a DoltHub-backed federation enabling multiple Gas Town installations to share work via a commons database. Rigs can claim work from the commons, post completions, and earn reputation. Enables community-level task distribution.
**Proposed Flywheel Implementation:** Flywheel's scope is single-user, making full Wasteland federation out of scope. However, a simpler "session export/import" feature could enable sharing interesting sessions or prompts between users. Sessions could be exported as JSON (with conversation events stripped to just assistant messages) and imported to seed new sessions. The existing `Session` struct with its `query` and `handoff_filepath` fields provides a natural export format.
**Priority:** Low
**Rationale:** Flywheel's single-user design philosophy means full multi-user federation is out of scope. A lightweight export/import for sharing session prompts and outcomes could be useful for team members using separate Flywheel instances.

---

## Patrol Cycle and Digest Aggregation
**Gastown Implementation:** Patrol cycles (Deacon, Witness, Refinery) create per-cycle digest beads. `gt patrol digest` aggregates daily summaries into permanent bead records, creating an audit trail of system health history. The six-stage data lifecycle automates archival and cleanup.
**Proposed Flywheel Implementation:** Flywheel's daemon already generates bus events for session status changes. A lightweight daily activity digest (created automatically by the daemon at midnight or via a `:digest` command) could write a `~/.flywheel/digests/YYYY-MM-DD.json` file summarizing: sessions launched, completed, costs, token counts, and notable events. This would provide the "what did I accomplish today" view that patrol digests provide in Gastown, without requiring a full Dolt database.
**Priority:** Low
**Rationale:** Flywheel users lack historical productivity metrics. A daily digest file would enable retrospective analysis without complex database infrastructure.

---

## Role-Based Context Injection (Prime System)
**Gastown Implementation:** `gt prime --hook` injects full role-appropriate context at every session start. For polecats: hook contents, molecule progress, pending mail, ready beads. For the Mayor: cross-rig status, active convoys. For the Refinery: queue status. This is a Claude Code `SessionStart` hook.
**Proposed Flywheel Implementation:** Flywheel already supports `system_prompt` in `LaunchSessionParams`. The memory subsystem indexes session transcripts and can inject relevant context. Missing: a structured context injection system that automatically includes project-specific context (e.g., project README, recent git log, open issues from a beads-compatible store). A `project_context` field in project config could specify files to inject as system prompt context. The prompt compiler framework in `docs/prompt-compiler.md` already describes this as a planned integration point.
**Priority:** High
**Rationale:** Currently Flywheel sessions start with minimal context. Automatic injection of project-relevant context (README, recent changes, relevant memory files) would significantly improve first-turn quality for new sessions.

---

## Checkpoint — Session Crash Recovery
**Gastown Implementation:** `gt checkpoint write|read|clear` stores atomic session snapshots in `.polecat-checkpoint.json`. Checkpoints capture git state, molecule progress, and hooked work. On crash, the next session reads the checkpoint via `gt prime` and resumes from the exact state.
**Proposed Flywheel Implementation:** Flywheel's `DevState` already captures scroll positions and input drafts for hot-reload. Extending this to include a per-session "work checkpoint" (a short plaintext description of current goals, key files modified, and next step) would provide lightweight crash recovery. The checkpoint could be written automatically when a session enters `WaitingApproval` or every N turns. On session `ContinueSession`, the checkpoint content could be prepended to the follow-up query as context. This would be far simpler than Gastown's full checkpoint system but address the same core problem.
**Priority:** Medium
**Rationale:** When a Flywheel session crashes or the user needs to restart, manually reconstructing what the session was doing is friction-heavy. Even a simple "what were you doing?" note would significantly improve recovery.

---

## DND (Do Not Disturb) Flag
**Gastown Implementation:** `gt dnd on|off|status` allows agents to block nudge interruptions during focus periods. The DND flag is checked by the nudge delivery system before sending. This enables sessions to protect critical operations from interruption.
**Proposed Flywheel Implementation:** Flywheel could add a per-session `do_not_interrupt: bool` flag to `Session`. When set, the `InterruptSession` RPC would require confirmation (a second `x` press) before sending SIGINT. The approval flow (`a`/`d` keybindings) already shows how context-sensitive actions work. An `X` long-press or `gx` keybinding could set the DND flag, with the session list rendering a distinct icon (e.g., `⊘`) for DND sessions. This would prevent accidental interruption of long-running sessions.
**Priority:** Low
**Rationale:** Flywheel users sometimes accidentally press `x` and interrupt important long-running sessions. A DND mechanism would prevent this without adding confirmation dialogs (which violate the no-confirmation-dialogs design philosophy).

---

## Agent Reputation and CV Tracking
**Gastown Implementation:** Each polecat accumulates a CV (curriculum vitae) chain tracking work history across assignments. The CV records: issues worked, completion rate, merge success rate, and escalation frequency. This provides a reputation system for assessing agent quality and routing work to reliable agents.
**Proposed Flywheel Implementation:** Flywheel already tracks `num_turns`, `cost_usd`, `duration_ms`, `stop_reason`, and `rotation_depth` per session. A `session_analytics` view (accessible via a new `:stats` command or `ga` → analytics browser) could aggregate these across sessions to show: average session cost, typical session duration, most common stop reasons, context usage patterns, and model performance comparisons. The `turn_metrics` table provides per-turn granularity for detailed analysis.
**Priority:** Low
**Rationale:** Flywheel's current analytics are limited to per-session cost and token display. Aggregate analytics across sessions would help users optimize their workflow (e.g., discovering that shorter prompts produce better results, or that certain models are more efficient for specific task types).

---

## Upgrade / Migration System
**Gastown Implementation:** `gt upgrade` is the post-binary migration orchestrator that propagates configuration changes, applies schema migrations, seeds new daemon.json entries, and verifies installation consistency. It is safe to run repeatedly (idempotent) and handles all version transitions automatically.
**Proposed Flywheel Implementation:** Flywheel's daemon already has versioned SQLite migrations (`PRAGMA user_version`). The missing piece is a TUI-side migration mechanism. When the daemon upgrades and adds new capabilities (new `DaemonCapabilities` flags), the TUI could display a "upgrade available" notification in the status bar and offer a `:upgrade` command that applies any pending client-side migrations (e.g., migrating old `PersistedState` format, re-indexing memory files, resetting capability caches). The hot-reload workflow already handles dev-time migrations via DevState.
**Priority:** Low
**Rationale:** As Flywheel gains new features, managing state format changes between versions will become increasingly important. A formal upgrade command would prevent silent failures when users update the binary.

---

## Capacity-Controlled Dispatch Scheduler
**Gastown Implementation:** `gt config set scheduler.max_polecats N` enables deferred dispatch, preventing more than N simultaneous polecats. When the cap is hit, slings queue and dispatch as slots open. Full scheduler management via `gt scheduler status|list|run|pause|resume|clear`.
**Proposed Flywheel Implementation:** Flywheel could add a `max_concurrent_sessions` setting in `UserSettings`. When launching a new session via `LaunchSession`, the daemon would check the count of `Running` sessions. If at the limit, the session would enter a `Queued` status (a new `SessionStatus` variant) and automatically launch when another session completes. The TUI could render queued sessions distinctly in the session list. The `:task` command already provides a TaskRabbit shortcut; a `:q-task` variant could enqueue rather than immediately launch.
**Priority:** Low
**Rationale:** Power users running many parallel sessions sometimes overwhelm available context windows or token rate limits. A soft cap on simultaneous sessions would prevent accidental overcommit without requiring manual tracking.

---

## Structured Session Signaling
**Gastown Implementation:** The signal system (`gt signal stop`, `gt mol await-signal`, `gt mol emit-event`) provides typed structured messages between agents. Signals use structured data (JSON) enabling deterministic routing. Sessions can await specific signals from other sessions before proceeding to the next step.
**Proposed Flywheel Implementation:** Flywheel sessions currently communicate only through the user typing follow-up queries. A lightweight inter-session signal system could be built on the existing event bus. Session A could emit a named signal (a special `ConversationEvent` with `event_type = "Signal"`) that Session B is waiting for (blocking at a step boundary). The daemon's `EventBus` already broadcasts `ConversationEvent` updates — sessions could subscribe to specific session events. This would enable simple coordination patterns: "run session B only after session A produces a specific artifact."
**Priority:** Low
**Rationale:** Flywheel's docregblock system already implicitly coordinates sessions (parent launches children). Explicit signal-based coordination would enable more sophisticated pipeline patterns where sessions can wait on each other.
