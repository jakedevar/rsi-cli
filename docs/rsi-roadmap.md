# RSI Harness Roadmap

Maintained index of improvement candidates for the RSI meta-harness. One ticket per item in `thoughts/shared/tickets/rsi-harness/`. This document is the **reasoning archive** — it explains WHY each item is ranked where it is. Ticket files are the agent-consumable units; invoke `/master_implement thoughts/shared/tickets/rsi-harness/RSI-NNN_*.md` to pull one.

**Last updated**: 2026-04-26

**Session 2026-04-25/26 landed**: RSI-002, RSI-010 (held pending RSI-017 decision), RSI-012, RSI-013, RSI-014, RSI-020 (new — test baseline cleanup), RSI-021 (new — agent contract enforcement). Tier-0 measurement primitives (RSI-001/002) now complete on main; Tier 1 tickets that depend on them (RSI-003/004/005/006/008/016) are unblocked.

---

## Core thesis

The core loop of any recursively self-improving system is:

> **Act → Measure → Attribute → Mutate → Verify → Iterate**

RSI today is world-class at step 1 (structured agent pipeline, worker preamble v2, April 2026 context-bloat refactor) and **zero at steps 2–5**. The beautiful mutation target at `.claude/commands/_shared/worker_preamble.md:74` explicitly states telemetry correlates worker performance to preamble versions — but that telemetry does not exist in code.

The binding constraint on RSI's rate-of-self-improvement is not throughput or per-session accuracy. It is the feedback loop. Every item below is ranked by leverage over that loop.

---

## Ranked queue

Status: `ready` = all deps met, can be pulled · `in-progress` = active work · `blocked` = dependency unmet · `done` = merged · `superseded` = folded into a newer ticket · `deferred` = intentionally parked

### Priority 0 — Process infrastructure (parallel-execution unlock)

| # | ID | Title | Status | Complexity |
|---|---|---|---|---|
| 0 | [RSI-019](../thoughts/shared/tickets/rsi-harness/RSI-019_agent_sandbox_git_worktree.md) | Agent sandbox (git-worktree-per-session) | done | medium-high |
| 20 | [RSI-020](../thoughts/shared/tickets/rsi-harness/RSI-020_test_baseline_cleanup.md) | Workspace test baseline cleanup | done | low |
| 21 | [RSI-021](../thoughts/shared/tickets/rsi-harness/RSI-021_agent_contract_enforcement.md) | Agent contract enforcement (pre-commit hook + master regex + worker preamble v4) | done | medium |
| 22 | [Agent control via rpc-cli](agent-control.md) | Tokened `Agent*` verbs + native `rsi_control` tools — an agent drives its own daemon (spawn/status/halt/wake) behind a per-session attribution gate | done | high |

**Why these sit above Tier 0**: RSI-019 (sandbox) unblocks parallel agent execution. RSI-020 (test baseline) is the green-floor every other ticket builds on — without it, regressions hide in noise. RSI-021 (contract enforcement) is the durable fix for the four chronic agent failure modes (wrong-branch commits, malformed handoffs, stale-handoff drift, contract-violation no-ops) observed across the 2026-04 batch. All three are foundation work that compounds across every downstream ticket.

### Tier 0 — Foundational primitives (no measurement → no recursion)

| # | ID | Title | Status | Complexity |
|---|---|---|---|---|
| 1 | [RSI-001](../thoughts/shared/tickets/rsi-harness/RSI-001_session_rating_harness_hash.md) | Session rating + harness version hash | done | low |
| 2 | [RSI-002](../thoughts/shared/tickets/rsi-harness/RSI-002_outcome_proxies_autocapture.md) | Automated outcome proxies | done (Phase 2 server-side correctness shipped 2026-04-26) | low |

### Tier 1 — Attribution layer (learning from signal)

| # | ID | Title | Status | Complexity |
|---|---|---|---|---|
| 3 | [RSI-003](../thoughts/shared/tickets/rsi-harness/RSI-003_analytics_rpcs.md) | Cross-session analytics RPCs | ready (RSI-001/002 deps met 2026-04-26) | medium |
| 4 | [RSI-004](../thoughts/shared/tickets/rsi-harness/RSI-004_session_diff_confound_isolation.md) | Session diff / confound isolation | ready (RSI-001 dep met 2026-04-26) | medium |
| 5 | [RSI-005](../thoughts/shared/tickets/rsi-harness/RSI-005_failure_mode_taxonomy.md) | Failure-mode taxonomy | ready (RSI-001/002 deps met 2026-04-26) | low |
| 6 | [RSI-006](../thoughts/shared/tickets/rsi-harness/RSI-006_eval_replay_harness.md) | Eval / replay harness **(promoted from Tier 2)** | ready (RSI-001/002 deps met 2026-04-26) | high |

### Tier 2 — Closing the loop

| # | ID | Title | Status | Complexity |
|---|---|---|---|---|
| 7 | [RSI-007](../thoughts/shared/tickets/rsi-harness/RSI-007_automated_mutator_dreamer.md) | Automated mutator (Dreamer) | blocked (deps RSI-003/005/006) | high |
| 8 | [RSI-008](../thoughts/shared/tickets/rsi-harness/RSI-008_ab_harness_routing.md) | A/B harness routing at launch | ready (RSI-001 dep met 2026-04-26) | low |
| 17 | [RSI-017](../thoughts/shared/tickets/rsi-harness/RSI-017_custom_agent_harness.md) | Custom agent harness (API-based) | blocked | very-high |
| 18 | [RSI-018](../thoughts/shared/tickets/rsi-harness/RSI-018_managed_memory_system.md) | Managed memory system | blocked | very-high |

### Tier 3 — Iteration speed (mostly collapsed under RSI-017)

| # | ID | Title | Status | Complexity |
|---|---|---|---|---|
| 9 | [RSI-009](../thoughts/shared/tickets/rsi-harness/RSI-009_prompt_hot_reload.md) | Prompt hot-reload | superseded by 017 | medium |
| 10 | [RSI-010](../thoughts/shared/tickets/rsi-harness/RSI-010_model_routing_enforcement.md) | Model routing enforcement | **done** on `origin/rsi-010-model-routing` — **MERGE GATED pending RSI-017 decision** | low |
| 11 | [RSI-011](../thoughts/shared/tickets/rsi-harness/RSI-011_read_budget_instrumentation.md) | Read-budget instrumentation | superseded by 017 (RSI-002 dep met but value bridges only until 017) | low |

### Tier 4 — Accuracy refinements

| # | ID | Title | Status | Complexity |
|---|---|---|---|---|
| 12 | [RSI-012](../thoughts/shared/tickets/rsi-harness/RSI-012_per_kind_preamble_templates.md) | Per-session-kind preamble templates | done | medium |
| 13 | [RSI-013](../thoughts/shared/tickets/rsi-harness/RSI-013_handoff_schema_validation.md) | Handoff schema validation | done | low |
| 14 | [RSI-014](../thoughts/shared/tickets/rsi-harness/RSI-014_structured_research_outputs.md) | Structured research artifacts (JSON companion) | done | medium |

### Tier 5 — Human-in-loop quality of life

| # | ID | Title | Status | Complexity |
|---|---|---|---|---|
| 15 | [RSI-015](../thoughts/shared/tickets/rsi-harness/RSI-015_analytics_tui_overlay.md) | In-TUI analytics overlay | blocked (RSI-001 met; awaits RSI-003) | medium |
| 16 | [RSI-016](../thoughts/shared/tickets/rsi-harness/RSI-016_rating_on_session_cards.md) | Rating display on session cards | ready (RSI-001 dep met 2026-04-26) | low |

---

## Dependency graph

```
RSI-019 (sandbox) ──► unblocks parallel execution of everything below
                      (not a hard dep — current state works serially, just slowly)

RSI-001 ──┬──► RSI-003 ──┬──► RSI-007 (Dreamer)
          │              │
          ├──► RSI-004 ──┤
          ├──► RSI-005 ──┤
          ├──► RSI-006 ──┼──► RSI-017 ──► RSI-018
          │              │
          ├──► RSI-008   │
          ├──► RSI-015 ◄─┘ (also needs RSI-003)
          └──► RSI-016

RSI-002 ──┬──► RSI-003, RSI-005, RSI-006, RSI-007, RSI-008, RSI-015
          └──► RSI-011

Standalone (ready now): RSI-010, RSI-012, RSI-013, RSI-014
```

---

## Ready to pull (status=ready AND deps met) — post 2026-04-26

**Tier 1 (attribution layer — newly unblocked):**
- **RSI-006** — Eval / replay harness — **highest leverage**; gates RSI-017 evaluation
- **RSI-005** — Failure-mode taxonomy — small, foundational signal-classification work
- **RSI-003** — Cross-session analytics RPCs — turns persisted ratings/proxies into queryable signal
- **RSI-004** — Session diff / confound isolation — separates real wins from noise

**Tier 2 (closing the loop — newly unblocked):**
- **RSI-008** — A/B harness routing at launch — small enabler for online experiments

**Tier 5 (QoL — newly unblocked):**
- **RSI-016** — Rating display on session cards — surface the rating signal in the TUI

**Tier 3 (bridge value, fates tied to RSI-017):**
- **RSI-011** — Read-budget instrumentation — only worth pulling if RSI-017 slips 2+ months

**Recommended sequence (post-batch)**:

1. **RSI-006** (eval harness) — single biggest leverage item available. Required before any responsible evaluation of RSI-017. Build the canned-ticket corpus + replay infrastructure.
2. **RSI-005** (failure-mode taxonomy) — small, parallelizable; lays the schema for what kinds of failures to enumerate.
3. **RSI-003** (analytics RPCs) — once schema is fixed, the queryable surface follows.
4. **Fork in the road** after RSI-006: either continue tuning Claude Code (RSI-004/007) or commit to the rewrite (RSI-017/018). Do not do both in parallel.
5. **Side track**: RSI-016 + RSI-008 are small and standalone — interleave with the above when context budget allows.

### Held items
- **RSI-010** — Model routing enforcement — **done on origin/rsi-010-model-routing but unmerged**. Hold per `merge_gate: HOLD_PENDING_RSI_017_DECISION`. Decide merge when RSI-017 commits/declines; if RSI-017 ships <1 month out, the validator becomes throwaway code.

---

## Reasoning archive

### Why watches are daemon-owned (A8, shipped 2026-07-04)

Seven incidents in one week proved every master-hosted watcher (pollers, hook
scripts, harness monitors) dies with the master while its children complete
unobserved. The fix inverts ownership: a watch is a persisted scheduled-jobs
row the **daemon** fires from database truth (the due-poll is the reconcile
tick; bus events only accelerate). See [docs/session-watches.md](session-watches.md)
and [docs/agent-control.md](agent-control.md).

### Why retry is kind-scoped (A9, shipped 2026-07-07)

Auto-retry is useful for headless worker sessions and dangerous for interactive
orchestrator sessions: replaying a `Story` master can duplicate the whole
program prompt. A9 keeps worker recovery but makes `Story` and `Standard`
default to no retry, makes retry cancellation durable across daemon restarts,
and adds a live `retry_enabled` kill-switch. See
[docs/symphony_capability_docs/retry_backoff.md](symphony_capability_docs/retry_backoff.md).

### Why measurement comes before everything

The April 2026 context-bloat refactor shipped `worker_preamble.md` v2 as an explicit mutation target. Line 74 of that file states: *"Downstream telemetry correlates worker performance to preamble versions."* **That telemetry does not exist in code.** Zero rating collection, zero harness versioning, zero analytics. The beautiful mutation target has no feedback signal. Until it does, every "improvement" is dead-reckoning and the system cannot recursively improve itself. Hence Tier 0.

### The Tier-0 primitives explained

**RSI-001 (rating)** and **RSI-002 (automated proxies)** are paired. Ratings are sparse — the user won't rate most sessions. Proxies (test-pass, clippy-pass, turn count, approval wait) are dense but noisier. Together they produce dense-and-honest signal. Either alone is too weak to drive recursion.

**The harness version hash** (part of RSI-001) is what makes ratings *attributable*. Without it, "session B scored higher than A" could be caused by anything — different preamble, different model, different time of day. With it, you can say "harness v4 outperformed v3 on Rust-heavy tickets by 15%."

### Why RSI-006 was promoted to Tier 1

Originally ranked Tier 2. Promoted because **RSI-017 (custom agent harness) cannot be validated without it.** You cannot know whether a 2–3 month rewrite improved outcomes without a canned-ticket corpus to replay both harnesses against. Attempting RSI-017 without RSI-006 is flying blind on the biggest architectural change in the project. The eval harness is a prerequisite, not a nice-to-have.

### The custom harness decision (RSI-017 + RSI-018)

Claude Code headless CLI is the **hard ceiling** on everything downstream:

- Context rotation is a blunt instrument
- Handoffs are a workaround for session-persistence gaps
- Prompt caching is opaque
- The harness itself isn't mutable (it's a CLI binary, not a library)

For a recursively self-improving system, owning the loop is the single biggest architectural unlock available. RSI-017 ships an in-tree API-based harness with orchestrator-worker split. RSI-018 is the substrate: a 3-tier memory system (working set + recency cache + archive) with embedding-based retrieval, cache-prefix discipline, and canonical-reference preservation.

**Why diffusion workers are deferred** (despite being fast and cheap): Diffusion LLMs (Mercury, Inception, LLaDA) are currently weaker at tool use and structured output than Haiku 4.5. For an RSI system, **worker quality is a capacity gate, not a cost knob** — subtly-wrong tool calls force the orchestrator to re-run and erode the speed advantage. Start with Haiku 4.5 workers behind a universal worker interface; diffusion models plug in later via that interface without rewrite.

**Why managed memory is architecturally correct** (not just an optimization): Raw long-context models degrade past ~100–200k tokens even when windows are bigger (attention gets noisy, retrieval errors compound). Managed memory lets the agent control what it attends to, makes retrieval first-class, bounds cost per turn, and preserves prompt-cache hit rates. Every serious production agent (Devin, Cursor agent mode) is built this way.

### Tier 3 tickets that get superseded

**RSI-009** (hot-reload), **RSI-010** (model routing), **RSI-011** (read-budget instrumentation), **RSI-013** (handoff schema validation) all become trivial or obsolete under RSI-017's architecture. They remain in the roadmap for *bridge-period value*: do them if RSI-017 is delayed more than 2 months; skip them if you commit to RSI-017 sooner. Each ticket's frontmatter carries a `superseded_by: RSI-017` flag so an orchestrator can deprioritize them once RSI-017 is in progress.

### Stop-doing list (anti-recommendations)

- **Don't build RSI-007 (Dreamer) first.** Most exciting item, hardest to validate. Useless without {signal, attribution, eval}. It will propose changes you cannot evaluate.
- **Don't expand the RPI command suite.** The April refactor was right. Every new command is new mutation surface to test. Consolidate before expanding.
- **Don't loosen worker read/return budgets.** Load-bearing wall against context regression. Adjust via measurement (RSI-011), not taste.
- **Don't couple worker-model choice to memory architecture.** Build the worker abstraction first, then experiment with models behind it.
- **Don't commit to RSI-017 before RSI-001/002/006 land.** You need baseline measurements to prove the rewrite improved anything.

---

## Process notes

- **Ticket files are self-contained** for agent consumption. `/master_implement path/to/ticket.md` works end-to-end.
- **YAML frontmatter is machine-readable**: `id`, `status`, `depends_on`, `blocks`, `complexity`, `estimated_phases`. Future Dreamer (RSI-007) consumes these fields directly.
- **Status transitions**: `ready → in-progress → done` (or `blocked` if deps unmet; `superseded` if replaced). Update frontmatter when state changes.
- **When a ticket lands**, set its frontmatter `status: done` and regenerate the "Ready to pull" section of this roadmap (manual for now; scriptable post-RSI-003).
- **New candidates**: add a ticket file with the next free ID (`RSI-019_*.md`), update this index, wire dependencies. Do not edit existing IDs.
# Local issue tracker capability

The local V72 tracker has operator CRUD/dependency verbs plus the create-only
`AgentCreateIssue`/`rsi_control_create_issue` follow-up surface; C4/TUI remains
deferred. See [local-issue-tracker.md](local-issue-tracker.md).
