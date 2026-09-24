# RSI Operator Views Redesign

Status: design brief  
Date: 2026-08-22  
Scope: Session List, Issue Tracker, Scheduled Jobs, and Settings

This document separates explicit operator requirements from recommended additions:

- **Required** — directly requested or necessary to make the requested behavior coherent.
- **Recommended** — uses existing RSI data/capabilities to make the view operationally useful.
- **Later** — valuable, but safe to defer after the first complete release.

## Resolved Interpretation

1. An **Epic** remains the objective/container. Its designated lead leaf is the
   **Demiurge**; the Epic container itself keeps the objective name.
2. Every designated Epic lead displays the title `Demiurge` everywhere RSI
   presents an agent title. Every other direct Epic child displays its pipeline
   function, not an LLM-generated summary title.
3. The function title is deterministic spawn metadata, not prose inferred after
   the session starts. Add a required `agent_role`/`function_title` to the agent
   spawn contract; `topology_node`, then `SessionKind`, are compatibility fallbacks.
4. `yy` copies the canonical UUID of the focused object. For issues, `y#` is a
   recommended second binding for the short human-facing issue number.
5. “Delete” for issues means `Cancelled`, preserving history. Scheduled jobs are
   disabled/archived by default; permanent purge, if retained, requires explicit
   confirmation because run history has operational value.
6. In the session table, `∞` is the pin-column header and `◆` is the per-row
   pinned marker. Sandbox state is inspector metadata, not a `BOX` list column.

## Shared Interaction Contract

All four views should behave like first-class Vim-native panes:

| Interaction | Contract |
|---|---|
| `j` / `k` | Move selection; selection remains stable across refresh by UUID |
| `gg` / `G` | First / last row |
| `/` | Search the active dataset |
| `f` | Open structured filters |
| `s` | Open sort choices; show active sort in the header |
| `Enter` | Focus inspector or open the primary linked object |
| `yy` | Copy the focused object's full UUID via OSC52 and show a confirmation toast |
| `r` | Refresh without moving a still-present selection |
| `?` | Open contextual help filtered to commands available at the current pane, tab, focus target, mode, object state, and authority |
| `Esc` | Back one focus level; never discard an edited form without warning |

Every list must define loading, empty, stale, error, and permission-denied
states. Color may reinforce state, but a glyph or label must carry the meaning.

None of the views has a persistent bottom hint/command bar. Command discovery is
owned by the contextual `?` help overlay. The overlay and input dispatcher must
read from the same action registry so help cannot advertise stale bindings.
Unavailable actions are omitted rather than shown as generic global commands;
the list recomputes when focus, tab, selection state, edit mode, or authority
changes.

## Shared Theme Contract

Theme support is foundational work for these views, not a later cosmetic pass:

| ID | Requirement | Acceptance signal |
|---|---|---|
| TH-01 | Every new view consumes semantic theme roles rather than literal colors. | No view-local RGB/ANSI color constants are introduced. |
| TH-02 | Theme changes apply live to all open panes, overlays, inspectors, tables, toasts, and contextual help. | Switching or editing a theme repaints the next frame without restart. |
| TH-03 | Operators can select a theme and override individual semantic roles. | `Theme & Colors` provides live preview, reset-role, and reset-theme actions. |
| TH-04 | State never depends on color alone. | Status, pin, readiness, failures, and selection retain glyph/text meaning in monochrome. |
| TH-05 | Custom colors are validated for terminal capability and readable contrast. | Invalid values are rejected; low-contrast foreground/background pairs warn before save. |

Initial semantic roles should cover canvas, panel, elevated surface, border,
focused border, selected row, primary text, muted text, accent, pin, running,
waiting, success, warning, error, disabled, and toast. Existing theme functions
should be mapped into this registry before the four views land; temporary
per-view aliases are not a second source of truth.

Shared acceptance tests must verify that no view reserves a bottom hint-bar row,
that contextual help changes when focus or mode changes, and that every command
shown by `?` is dispatchable in that exact context. Theme tests must render each
view under the built-in themes plus a custom override set and assert semantic
role coverage without snapshotting literal palette values into view logic.

---

## 1. Session List View

### Required behavior

| ID | Requirement | Acceptance signal |
|---|---|---|
| SL-01 | The designated lead of every Epic displays `Demiurge` as its effective title across the TUI. | List, detail, breadcrumbs, notifications, and selectors resolve the same title. |
| SL-02 | A non-lead direct child of an Epic displays the function it serves in the pipeline. | A spawned researcher appears as `Researcher`, not a generated prose title. |
| SL-03 | Normal sessions outside an Epic retain the existing title-generation/fallback behavior. | No global regression to ordinary session titles. |
| SL-04 | The title agent does not generate prose titles/descriptions for role-titled Epic children. | Epic-child creation does not enqueue the title model path. |
| SL-05 | The first list column is a faint, right-aligned per-Epic spawn ordinal. | Demiurge is `0`; subsequent spawned agents are `1…N`. |
| SL-06 | Spawn ordinals are durable, immutable, and never recomputed from current sort order or timestamps. | Refresh, restart, filtering, archival, and failure do not renumber rows. |
| SL-07 | Ordinals are scoped to an Epic. Non-Epic sessions show a blank in this column. | The number cannot be mistaken for a global list position. |
| SL-08 | Leadership changes are well-defined. | The current lead displays virtual `0`; stored child ordinals remain reserved and are restored if the agent is later demoted. |
| SL-09 | `yy` over a session copies its complete session UUID to the system clipboard. | Toast: `session <uuid> copied`; the selected row does not change. |
| SL-10 | No summary or description is rendered in the left session navigator. | Every list item remains one physical row at all expansion states. |
| SL-11 | Pin and lifecycle status each have a fixed symbol column. The pin-column header is `∞`; a pinned row contains `◆`. | Neither state is embedded into or shifts the title text. |
| SL-12 | Context fill percentage and turn count are visible list fields. | Unknown context renders a dim `◌` (operator-approved symbol pass, 2026-09-23; formerly `—`), never `0%`. |
| SL-13 | The session list has no `BOX`/sandbox column. | Sandbox details remain available in the selected-session inspector. |

### Data contract

Add two durable concepts rather than overloading title text:

```text
Session.agent_role: Option<String>       # Researcher, Planner, Reviewer…
Session.epic_spawn_ordinal: Option<u32>  # direct Epic child only; 1…N
Epic.lead_session_id                     # already exists; effective ordinal 0
```

The spawn request must carry `agent_role`. Reserve the ordinal atomically with
the durable spawn request so concurrent child spawns cannot receive the same
number. A failed or cancelled reservation is not reused; gaps preserve history.
Any schema change follows RSI's versioned inline migration and `user_version`
bump rules.

Use one canonical resolver everywhere:

```text
if parent is Epic and Epic.lead_session_id == session.id -> "Demiurge"
else if parent is Epic and agent_role is present          -> agent_role
else                                                       -> existing title/query fallback
```

Do not overwrite the raw/manual `Session.title`; derive `effective_display_title`
so historical data and operator edits remain recoverable.

### Recommended row layout

```text
 #  ∞  S  FUNCTION        MODEL/EFFORT       CTX   TURNS  AGE
 0  ◆  ●  Demiurge        gpt-5.6 max         42%     18t  now
 1     ●  Researcher      gpt-5.6 high        31%      9t   2m
 2     ◐  Planner         gpt-5.6 max         12%      2t  38s
 3     ?  Implementer     codex high          76%     27t  now
 4     ✓  Reviewer        gpt-5.6 high        54%     14t   7m
```

Suggested symbols (documented in contextual `?` help):

| Column | Glyphs |
|---|---|
| `∞` pin column | `◆` pinned, blank unpinned |
| Status | `◐` starting, `●` running, `?` waiting approval/input, `✓` completed, `×` failed, `■` interrupted, `·` archived |

### Recommended operational additions

- Keep model/provider and effort visible: they explain capability and spend at a glance.
- Show an attention glyph for waiting approval, waiting input, stalled, failed,
  unread output, and retrying; attention sorts above ordinary running rows.
- Keep age/last-event time. It is more actionable than created time in the dense view.
- Make cost, accumulated work time, created time, project, rotation depth, and
  retry count optional columns with width-aware collapse.
- Move query, description, full UUID, cwd, branch, sandbox state, tags,
  hierarchy, token counts, lineage, cost detail, and last event into the inspector.
- Provide presets: `Dense`, `Operations`, and `Cost`. The required columns survive every preset.
- Maintain a single glyph legend and accessible text mode; never encode lifecycle solely by color.

### Session acceptance tests

1. Spawn an Epic lead and three children concurrently; titles resolve to
   `Demiurge`, function names, and ordinals `0,1,2,3` after daemon restart.
2. Filter, sort, pin, archive, unarchive, and rotate a child; its ordinal remains unchanged.
3. Transfer lead status; exactly one row shows `0` and `Demiurge`.
4. `yy` from list, detail, and split layouts copies the same full UUID.
5. A narrow terminal drops optional columns in a deterministic order without
   hiding ordinal, status, title, context, or attention.

---

## 2. Issue Tracker View

### Required behavior

| ID | Requirement | Acceptance signal |
|---|---|---|
| IT-01 | Issues are managed in a dedicated `Pane::Issues`, not only an overlay. | The view can occupy, split, and restore like other panes. |
| IT-02 | Display and navigate local RSI issues. | Local `issues` and derived dependency state populate the table. |
| IT-03 | `yy` copies the selected issue's canonical UUID. | Toast confirms the copied ID; `y#` copies `#<display_number>`. |
| IT-04 | Support complete operator management: create, inspect, edit, status, priority, labels, assignee, and dependencies. | Every mutable `IssueUpdate` field has a TUI path and operator RPC. |
| IT-05 | Preserve existing external tracker/dispatch visibility. | `Dispatched` and `Sync` tabs retain current poller status and associated sessions. |
| IT-06 | Issue removal is logical. | `dd` moves an issue to `Cancelled` after confirmation; no row is hard-deleted. |

The existing store has a general `IssueUpdate`, but the daemon exposes only
`UpdateIssueStatus`. Add a guarded operator `UpdateIssue` RPC and corresponding
TUI client call for title, body, priority, labels, and assignee.

### Recommended layout

Tabs: **Local · Dispatched · Sync**

```text
 #     P  STATE        READY  TITLE                         OWNER       AGE
 184   1  In progress    ●    Recurring jobs skip cadence  @jake        2h
 185   2  Open            ×    Add run-history persistence  unassigned  38m
 186   3  Closed          ✓    OSC52 issue UUID yank         agent:42     1d
```

The right inspector shows:

- full UUID and project;
- title/body, status, priority, labels, assignee;
- `blocked by` and `blocks` dependency lists;
- ready/blocked derivation;
- creator session, linked idea, source event, and source finding reference;
- created/updated/closed timestamps;
- dispatched/implementing session and its live status, when one exists.

### Recommended commands and capabilities

| Key | Action |
|---|---|
| `n` | New issue form |
| `e` | Edit all mutable fields |
| `S` | Change lifecycle status |
| `p` | Change priority |
| `a` | Assign/unassign |
| `l` | Edit labels |
| `b` | Add/remove dependencies with cycle validation feedback |
| `dd` | Cancel with confirmation |
| `Enter` | Inspect; open linked session when focus is on the link |
| `D` | Recommended later: dispatch ready issue to a new agent/session |

Filters should include project, status, priority, ready, blocked, assignee,
label, provenance, and text search. Provide saved views for `Ready`, `Blocked`,
`Mine`, and `Recently updated`.

### Issue acceptance tests

1. Create and edit every field, restart the daemon, and verify round-trip persistence.
2. Add a dependency, verify blocked/ready updates, reject a cycle, then remove it.
3. Cancel and reopen an issue without deleting provenance.
4. Refresh while editing and while selection is active; never jump to a different UUID.
5. `yy` copies UUID; `y#` copies the stable display number.

---

## 3. Scheduled / Cron Jobs View

### Required behavior

| ID | Requirement | Acceptance signal |
|---|---|---|
| JB-01 | `gj` opens a dedicated `Pane::Jobs`. | It no longer only switches the session-list zone. |
| JB-02 | The view supports create, read, update, enable/disable, archive/delete, and manual trigger for one-time and recurring jobs. | All existing schedule RPCs are reachable from a full-page manager. |
| JB-03 | Recurring operator jobs continue firing on their schedule. | A 60-second test job records at least three distinct due slots without manual intervention. |
| JB-04 | The editor exposes an actual time trigger, not only raw recurrence fields. | Once, Interval, and Cron forms preview their next three fire times. |
| JB-05 | Run history makes every firing observable. | Each due attempt records due time, start/end, outcome/error, and spawned session UUID. |
| JB-06 | Existing scheduled-job sessions remain reachable. | They move into the `Runs` tab instead of disappearing with the old zone binding. |

Keep `gK` as a compatibility alias to the same view for one release, then remove
it from documentation if desired.

### Recommended layout

Tabs: **Jobs · Runs · System**

- **Jobs** — operator-created definitions.
- **Runs** — attempts and spawned sessions, including manual triggers.
- **System** — daemon terminal watches, continuation wakes, and program guards;
  hidden by default so 60-second internal watches do not pollute operator jobs.

```text
 E  STATE    NAME                  SCHEDULE             NEXT       LAST / RESULT
 ●  healthy  Nightly research     02:00 daily · local  in 3h      21h · success
 ●  running  Inbox triage         every 6h             now        6h  · running
 ○  paused   Weekly synthesis     Mon 09:00 · local    paused      6d  · success
 !  failing  Repo health check    every 2h             in 43m      1h · failed
```

The inspector includes UUID, prompt, project/cwd, provider/model/effort, wake
mode, timezone, anchor, misfire and overlap policies, retries/backoff, next three
times, last success/failure, linked session, and recent run history.

### Schedule semantics

Support three operator-facing trigger types:

1. **Once** — local date/time plus IANA timezone.
2. **Interval** — every N minutes/hours/days/weeks, anchored to a visible due slot.
3. **Cron** — five-field expression plus IANA timezone and human-readable summary.

Define these policies explicitly:

| Policy | Recommended default | Other choices |
|---|---|---|
| Misfire after downtime | Fire once | Skip; bounded catch-up |
| Overlap while prior run active | Skip and record | Queue one; allow with concurrency cap |
| Failure | Exponential backoff, bounded attempts | No retry; fixed delay |
| Manual trigger | Do not shift recurring cadence | Optional “run and reset anchor” explicit action |
| DST ambiguity | First valid local instant, visibly recorded | Skip duplicate; run both only if explicit |

For LLM-spawning jobs, default to one hour rather than seconds. Intervals below
15 minutes should show estimated runs/day and require explicit confirmation;
do not impose that warning on internal terminal watches.

### Timer correctness work

Current code can demonstrably re-fire recurring system watches, but the live
database did not contain an ordinary recurring provider job to reproduce the
reported once-only behavior. One cadence-sensitive path needs direct correction
and coverage: after an attempt, the scheduler calls
`next_fire_time(&job.schedule, now, now)`. Advancement should be anchored to the
persisted due slot (`job.next_fire_at`), with `now` used only for catch-up policy.

Required verification:

1. deterministic interval test at recurrence equal to scheduler resolution;
2. three consecutive ordinary `fresh` provider-job firings;
3. daemon restart before and after a due slot;
4. long-running overlap behavior;
5. failure and retry behavior without shifting the base cadence;
6. concurrent scheduler ticks claim a due slot exactly once;
7. manual trigger does not mutate the scheduled next due time;
8. run-history row exists for success, launch failure, policy denial, and skip.

Prefer a wake keyed to the earliest enabled `next_fire_at`, with a bounded
fallback scan and CRUD nudge, rather than asking the operator to tune a daemon
poll interval for accuracy.

### Job commands

| Key | Action |
|---|---|
| `n` | New job |
| `e` | Edit selected definition |
| `Space` | Enable / pause |
| `!` | Trigger now without shifting cadence |
| `dd` | Archive after confirmation |
| `yy` | Copy job UUID |
| `Enter` | Inspect job; from Runs, open spawned session |
| `R` | Retry selected failed run as a new attempt |

---

## 4. Settings Page Cleanup

### Required behavior

| ID | Requirement | Acceptance signal |
|---|---|---|
| ST-01 | Every row names what it changes and who/what it affects. | Inspector shows purpose, human impact, agent/harness impact, scope, owner, persistence, apply timing, and restart requirement. |
| ST-02 | Dead settings are removed through state, UI, input handling, tests, and compatibility code. | No runtime or serialized field remains for audio waveform controls. |
| ST-03 | Categories and setting labels use unfamiliar-user language. | Prompt processing is not called “TUI Configuration”; operational dashboards are not settings. |
| ST-04 | Duplicate controls have one authoritative location. | Memory, dream, system prompt, classifier model, and control mode do not appear twice. |
| ST-05 | Real but non-configurable telemetry/actions move out of Settings. | Stats, active invocations, emergency stop, and cache actions have appropriate operational homes. |
| ST-06 | Every displayed mutable value is editable, or clearly marked read-only with a reason. | `Max retries` is editable rather than a dead Enter action. |
| ST-07 | Theme selection and semantic color overrides are first-class settings. | Changes preview and apply live, persist across restart, and can reset by role or whole theme. |

### Proposed information architecture

| New category/view | Contents |
|---|---|
| Interface | Submit behavior, question panel, text surfaces, animation |
| Theme & Colors | Theme selection and semantic color-role overrides with live preview |
| Session View | Event visibility defaults and session-list columns/presets |
| Models & Providers | Default model, custom providers, role models |
| Prompt Processing | Processor enablement, auto-compile, temperature |
| Automation & Recovery | Retry, reconciliation, stall detection/classifier, rotation, queue, orchestration cap |
| Memory & Reasoning | Memory, dream, dialectic, thresholds, cooldown |
| Sandboxing & Storage | Codex sandbox policy and cache retention thresholds |
| Integrations | Signal, iMessage, Claude hooks, Claude skills |
| Agent Behavior | System-prompt preset |
| Safety & Spend | Model-control mode and budget policies |
| Observability (separate view) | Usage stats, circuits, invocations, denials, alerts, emergency stop |

### Complete current-setting audit

#### Display

| Current item | What it actually does | Disposition |
|---|---|---|
| Audio waveform | Only changes `UserSettings`; no renderer consumes it. | **Delete by the roots.** |
| Audio visualization | Only cycles a settings enum; no audio renderer consumes it. | **Delete by the roots.** |
| Text area background | Enables filled backgrounds on messages, input, bottom strip, terminal overlay, and list surfaces. Visual only; no agent/harness effect. | **Keep**, rename `Opaque text surfaces`. |
| Submit on Enter | Plain Enter submits multiline input; Shift+Enter inserts a newline. Human input only. | **Keep** under Interface. |
| Auto-open question panel | Opens pending agent questions when no other overlay is active. Affects human attention flow, not agent behavior. | **Keep** under Interface. |
| Background color | Overrides the text-surface fill color; theme value is used when empty. Visual only. | **Keep conditionally**, shown only when opaque surfaces are enabled; rename `Text surface color`. |
| Formulation animation | Controls the grow/reveal duration for messages being formulated. Visual only. | **Keep** under Interface; allow `Off`. |

Removing audio controls means deleting `show_audio_waveform`,
`AudioVizModeSetting`/`audio_viz_mode`, their rows, key handlers, defaults, and
tests. Old state files should deserialize while ignoring the retired keys.

#### Session Defaults

| Current item | What it actually does | Disposition |
|---|---|---|
| Show system events | Default visibility for system events in newly initialized session views. Human display only. | **Keep**, rename `Show system events by default`. |
| Show thinking events | Default visibility for reasoning/thinking events. Human display only; may expose verbose model output. | **Keep**, rename `Show thinking by default`. |
| Hide tool results | Sets the global default and immediately updates existing session views. Human display only. | **Keep**, rename `Show tool results` with non-inverted semantics. |

#### API Models

| Current item | What it actually does | Disposition |
|---|---|---|
| One dynamic row per custom provider | Stores an OpenAI-compatible name, base URL, model list, and API key for model discovery/launch. | **Keep** under Models & Providers; label category `Custom Providers`. Mask secrets and show connection-test status. |

#### List Fields

| Current item | What it actually does | Disposition |
|---|---|---|
| Context bar | Shows context-window fill. | **Keep required**, rename `Context usage`; show percent plus compact bar. |
| Cost | Shows session cost. | **Keep optional**. |
| Turn count | Shows number of turns. | **Keep required**. |
| Retry info | Shows current/max retry state. | **Keep optional**, elevate automatically when retrying. |
| Pin indicator | Shows the pin marker. | **Keep required** as a fixed symbol column. |
| Rotation depth | Shows continuation/context-rotation depth. | **Keep optional** in inspector/dense preset. |
| Heat color | Colors title by recency. | **Keep optional**, but pair with explicit age and accessible non-color state. |
| Description | Shows description on expanded list cards. | **Remove from the left list**; keep in inspector. |
| Kind pill (TR/BUG) | Shows TaskRabbit/bug kind on expanded cards. | **Move to inspector** or a dedicated compact type column when relevant. |
| Docregblock pill | Shows document-registration command metadata. | **Move to inspector**. |
| Accumulated work time | Shows measured session work duration. | **Keep optional**. |
| Created date | Shows absolute creation time. | **Keep optional**; default list should prefer last-event age. |

Add fixed controls for the `∞` pin/`◆` marker and status glyph. Sandbox state
stays in the inspector. Replace twelve unrelated booleans with column presets
plus an advanced per-column editor and deterministic narrow-width collapse order.

#### Daemon Features

| Current item | What it actually does | Disposition |
|---|---|---|
| Model control mode | Changes admission globally: normal, pause background, deny paid, local only, or stop all. | **Move** to Safety & Spend; explain blast radius. |
| Emergency stop | Stops active model invocations. It is an action, not a setting. | **Move** to Observability/Safety with confirmation. |
| Retry on failure | Enables default session retry; disabling also drives max retries to zero. | **Keep** under Automation & Recovery. |
| Max retries | Daemon retry ceiling/default consumed by retry policy; currently display-only in the TUI. | **Keep and make editable** beside Retry on failure. |
| Retry on stall | Allows stall detection to trigger retry policy. | **Keep** under Automation & Recovery; dependent on stall detection. |
| Reconciliation loop | Enables daemon reconciliation of session/process state. | **Keep** under Automation & Recovery. |
| Stall detection | Enables rule-based detection of non-progressing sessions. | **Keep** under Automation & Recovery. |
| Context rotation | Enables automatic continuation when context rotation criteria are met. | **Keep** under Automation & Recovery. |
| Memory system | Enables daemon memory observation/search paths. | **Consolidate** into Memory & Reasoning. |
| Background queue | Enables queued background work. | **Keep** under Automation & Recovery. |
| Dream consolidation | Enables memory consolidation/deduction work. | **Consolidate** into Memory & Reasoning. |
| Dialectic engine | Enables dialectic reasoning subsystem. | **Keep** under Memory & Reasoning with an effect description. |
| Codex sandbox | Selects read-only, workspace-write, or danger-full-access for newly spawned Codex processes. | **Keep** under Sandboxing & Storage; state that existing sessions do not change. |
| System prompt preset | Selects default/concise/code-only/caveman daemon prompt behavior. | **Consolidate** into Agent Behavior. |
| Orchestration max child effort | Caps child-agent effort admitted by orchestration. | **Keep** under Automation & Recovery; explain inheritance and `unset`. |
| Sandbox cache reclaim | Enables automatic deletion of authenticated regenerable build caches. | **Keep** under Sandboxing & Storage. |
| Cache reclaim TTL | Minimum terminal-sandbox cache age before eligibility. | **Keep**; show human duration. |
| Cache reclaim interval | Frequency of reclaim scans. | **Keep**; show human duration. |
| Cache pressure high | Disk-use threshold that starts pressure reclaim. | **Keep**. |
| Cache pressure low | Target threshold at which pressure reclaim stops. | **Keep** and validate it is below high. |
| Cache reclaim pass limit | Maximum candidate caches processed per scan. | **Keep**. |
| Sandbox storage | Read-only storage report. | **Move** to a Storage status panel. |
| Preview cache reclaim | Calculates eligible cache reclamation without deleting. It is an action. | **Move** to Storage status/actions. |
| Reclaim sandbox caches now | Deletes authenticated regenerable target caches within configured bounds. It is an action. | **Move** to Storage status/actions with confirmation. |
| Stall classifier | Enables model-assisted stall classification. | **Keep** under Automation & Recovery. |
| Classifier model | Model used to classify stalls; duplicated in Agent Actors and read-only here. | **Consolidate** into Models & Providers, link from Automation. |
| Classifier idle threshold (Claude) | Idle time before a Claude session becomes classifier-eligible. | **Keep** under Automation & Recovery; show duration. |
| Classifier idle threshold (Codex) | Idle time before a Codex session becomes classifier-eligible. | **Keep** under Automation & Recovery; show duration. |
| Classifier cooldown | Minimum time between classifications for a session. | **Keep** under Automation & Recovery. |
| Classifier max per session | Caps model-assisted classifications per session. | **Keep** under Automation & Recovery. |
| Classifier confidence floor | Minimum confidence required to apply a classifier decision. | **Keep** under Automation & Recovery. |

#### Agent Actors

| Current item | What it actually does | Disposition |
|---|---|---|
| Title model | Generates ordinary session titles/descriptions. Epic role-titled children will bypass it. | **Keep** under Models & Providers, label `Ordinary-session title model`. |
| Processor model | Compiles/refines prompts before submission. | **Keep** under Models & Providers; link from Prompt Processing. |
| Memory fallback model | Fallback for observation extraction and summaries. | **Keep** under Models & Providers. |
| Default Model | Provider/model selected for newly launched sessions. | **Keep**, rename `New session default`. |
| Dream model | Runs memory consolidation and deduction. | **Keep** under Models & Providers. |
| Classifier model | Classifies stalled sessions and selects nudge actions. | **Keep one authoritative row** under Models & Providers. |

#### Hooks (Claude)

| Current item | What it actually does | Disposition |
|---|---|---|
| One dynamic row per hook | Reads/edits Claude Code hook event, matcher, and shell command in `~/.claude/settings.json`. | **Keep** under Integrations → Claude; describe external command/security impact. |

#### Skills (Claude)

| Current item | What it actually does | Disposition |
|---|---|---|
| One dynamic row per skill | Previews, enables/disables, or deletes a Claude user skill under `~/.claude/skills`. | **Keep** under Integrations → Claude; it is a resource manager, not a simple preference. |

#### TUI Configuration

| Current item | What it actually does | Disposition |
|---|---|---|
| Prompt processor | Enables prompt compilation/correction. Changes text submitted to the target agent. | **Keep**, move to `Prompt Processing`. |
| Auto-compile on submit | Runs the processor automatically before sending. Affects latency, spend, and submitted prompt content. | **Keep** with those impacts stated. |
| Compile temperature | Sampling temperature for the processor model. Changes determinism of rewritten prompts. | **Keep**, relabel `Rewrite temperature`. |

#### Memory Configuration

| Current item | What it actually does | Disposition |
|---|---|---|
| Memory enabled | Enables observation extraction and memory search; duplicated with daemon features. | **Keep once** under Memory & Reasoning. |
| Dream enabled | Enables periodic consolidation; duplicated with daemon features. | **Keep once** under Memory & Reasoning. |
| Observation threshold | Number of accumulated observations that triggers consolidation. | **Keep**, rename `Consolidate after observations`. |
| Dream cooldown | Minimum time between consolidation cycles. | **Keep**, show duration and interaction with threshold. |

#### Message Bridges

| Current item | What it actually does | Disposition |
|---|---|---|
| Signal | Configures bridge enablement, account/allowlist, and message transport policy. | **Keep** under Integrations; add connection health/test. |
| iMessage | Configures bridge enablement, account/allowlist, and message transport policy. | **Keep** under Integrations; add connection health/test. |

#### System Prompt

| Current item | What it actually does | Disposition |
|---|---|---|
| Preset | Changes the daemon-owned preset applied to session prompts. | **Keep once** under Agent Behavior; show exact affected providers/session timing. |

#### Stats

| Current item | What it actually does | Disposition |
|---|---|---|
| Chats, Spend, Input tokens, Output tokens, Cache create, Cache read, Work time, Per-model | Lifetime usage aggregates. | **Move** to Observability. |
| Control mode, Circuit, Policies, Active, Recent, Denials, Alerts | Model-control status/telemetry. | **Move** to Observability/Safety. |
| Stop now | Emergency action affecting active invocations. | **Move** to Observability/Safety with confirmation. |
| Dynamic Circuit/Active/Recent/Denied/Alert rows | Per-scope circuit, invocation, denial, and budget-alert detail; some invocation rows can cancel work. | **Move** to Observability with dedicated filtering and inspector. |

#### Budgets

| Current item | What it actually does | Disposition |
|---|---|---|
| One dynamic row per model budget policy | Creates/edits/deletes daemon model-control limits by scope; hardcoded defaults remain when no explicit policy exists. | **Move** to Safety & Spend; keep full CRUD and expose effective inherited policy. |

### Settings inspector contract

For every remaining row, render these fields from structured metadata rather
than ad-hoc descriptions:

```text
What it does
Human impact
Agent / harness impact
Applies to: current sessions | new sessions | daemon globally | provider-specific
Owner: UserSettings | rsid | Claude | integration
Persisted in: state.json | SQLite | external file
Applied: immediately | next launch | next cycle | restart required
Dependencies / conflicts
Default and reset action
```

Hide inapplicable rows. For example, hide text-surface color when opaque surfaces
are off, classifier thresholds when the classifier is off, and reclaim thresholds
when automatic reclaim is off.

---

## Delivery Slices

1. **Theme and action-discovery foundation** — semantic theme registry, live
   theme updates, contextual action registry, and filtered `?` help. Remove the
   persistent hint bar from all four views.
2. **Identity foundation** — durable `agent_role` and spawn ordinal, canonical
   effective-title resolver, migration, RPC/serde updates, and concurrency tests.
3. **Session navigator** — one-line rows, `∞`/`◆` pin semantics, fixed status,
   context, turns, `yy`, inspector relocation, and width presets; no `BOX` column.
4. **Issue workspace** — local issue client/RPC update support, dedicated pane,
   dependencies, clipboard, then external dispatch tabs.
5. **Scheduler correctness** — due-slot advancement/claim tests and run-history
   persistence before presenting a richer manager.
6. **Jobs workspace** — `gj`, Jobs/Runs/System tabs, schedule builder, policies,
   run inspector, compatibility alias.
7. **Settings cleanup** — delete dead audio state, fix max-retry editing,
   consolidate duplicates, reorganize categories, then move observability/actions.

Each slice should preserve existing behavior outside its named surface, update
`docs/keybindings.md` for bindings, use versioned migrations for schema changes,
and avoid hard deletion of operator history.

## Mockups

- `session-list.png` — one-line Epic agent navigator with `∞`/`◆` pin semantics,
  no `BOX` column, and no persistent hint bar.
- `issue-tracker.png` — local/dispatched issue workspace without a persistent hint bar.
- `scheduled-jobs.png` — job definitions, schedules, and run health without a persistent hint bar.
- `settings.png` — reorganized settings with `Theme & Colors`, impact inspector,
  and no persistent hint bar.
