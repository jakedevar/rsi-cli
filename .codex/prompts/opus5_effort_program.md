---
description: Prime and drive the Opus 5 + per-model effort ladder campaign via master_orchestrate program mode
---

# Opus 5 + effort ladders — campaign driver

Single entry point for bringing RSI's model and effort handling up to current Claude
semantics. Invoke this to load the full campaign context and carry it to done, slice
by slice, through `/master_orchestrate`.

This command exists to defeat the stateless-AI effect: every fact the campaign needs
already lives on disk. This tells you which facts, in what order, under what rules —
so any session or any model can pick the work up cold without re-deriving the plan.
Read the canonical artifacts before acting.

## Lineage (how this started)

Born in a `/drive_plan` session (2026-07-24) from a request to "replace any other
versions of Opus with Opus 5" and fix effort levels — Sonnet was capped at 3 levels.
Research found the request understated the problem three ways:

1. **`claude-opus-5` does not parse at all.** `model_utils.rs:77-91` admits a missing
   minor version only for Fable and Sonnet; Opus hits `return None`. Opus 5 therefore
   gets zero effort levels, a 128K context window, and renders as a raw ID string.
   Someone did this exact work for Sonnet 5 and left Opus out.
2. **The effort ceiling is wrong for every current Claude model**, not just Sonnet.
   No Claude model in RSI is ever offered `xhigh`, and Sonnet 4.6 is denied `max`.
3. **The direct-API harness emits a request shape that now returns HTTP 400** on
   Opus 5 / 4.8 / 4.7 / Sonnet 5 / Fable 5 — `thinking.budget_tokens` and
   `temperature` are both removed on those models. A live defect, independent of the
   Opus 5 work, that Opus 5 adoption walks straight into.

The originating session had **no shell** (`Bash` returned exit 1 on `true`), so nothing
was implemented and nothing was verified. That constraint shapes GATE-0 below.

## The canonical artifacts (read first, in order)

1. **PLAN** (the contract): `thoughts/shared/plans/2026-07-24-opus-5-and-per-model-effort-ladders.md`
   — ground-truth effort table, all file:line refs, D1–D6 design decisions, R1–R5 risks.
2. **PROGRAM LEDGER** (the spine): `thoughts/shared/orchestration/2026-07-24-opus5-effort-program-ledger.md`
   — slice queue, conflict domains, gates, done-means. `master_orchestrate` reads and
   updates this every slice.
3. **HANDOFF** (the entry point): `thoughts/shared/handoffs/general/` — the most recent
   `*opus5-effort*` handoff carries session state and the immediate next action.

Ground truth for effort levels is the bundled `claude-api` skill (Thinking & Effort
quick reference + `shared/model-migration.md`), transcribed into the plan's Context
Brief. **Do not re-derive it from model memory** — the levels changed recently and a
training prior will be wrong.

## The non-negotiable shape: gate first, then parser, then everything

Order: **S-HARNESS-400 → S-PARSE → S-LADDER → S-CONSUMERS → S-DOCS**

- **S-HARNESS-400 is first and doubles as GATE-0.** It is a live production defect,
  independently shippable, touches one function in two files, and its worker's
  `cargo test -p rsid` is the cheapest possible proof that spawned workers have a
  working shell. If that worker cannot compile, **halt the program** — do not spend
  four more slices writing Rust nobody can verify.
- **S-PARSE gates S-LADDER and S-CONSUMERS.** Renaming model strings before the
  parser accepts `claude-opus-5` produces a model that parses to nothing. Every
  downstream "Opus 5" symptom disappears once the parser is right.
- **S-PARSE and S-LADDER share a file** (`model_utils.rs`) — serial by conflict
  domain, not merely by WIP policy.
- **S-DOCS last.** It transcribes the shipped ladders; running it early pins a table
  that S-LADDER then changes.

## How to drive it

For each slice, in ledger order, run the slice conveyor:

    /master_orchestrate thoughts/shared/plans/2026-07-24-opus-5-and-per-model-effort-ladders.md
    mode: program
    ledger: thoughts/shared/orchestration/2026-07-24-opus5-effort-program-ledger.md
    stop_after_slice: true
    allow_parallel: false

Rules the conveyor must honor:

- **WIP = 1.** One slice in flight. The next slice is forbidden while the repo is dirty.
- **Ratchet.** Every slice closes with the test that pins it. No pinning test = not done.
- **Three gates.** GATE-0 (worker shell proof, at S-HARNESS-400), GATE-1 (full
  `model_utils` suite green after S-PARSE — the R1 blast-radius gate), GATE-2 (every
  changed assertion justified against the CLODCO table after S-LADDER). In an
  **interactive** run, stop for Jake at each. In an **autonomous** run, SELF-VERIFY by
  running the command and pasting the result; only stop if something is physically
  unavailable.
- **Assertion churn is a decision, not a chore.** S-LADDER breaks assertions that
  currently pass. Each edit cites the CLODCO table or it does not land.
- **Documentation is a required obligation** (`master_orchestrate`
  §Implementation, Verification, And Documentation); implementation owner updates
  due docs in same source revision.
- **No unscoped `cargo fmt`.** Format only touched files; `git status` after.
- **No schema migration.** `sessions.effort` is already `TEXT`.

## Open decisions (resolved 2026-07-24, reopen only with cause)

- **D3 default effort** → Opus 5 / 4.8 / 4.7 / Sonnet 5 default to `xhigh`, not `max`.
  CLODCO calls `xhigh` best for coding/agentic and it is Claude Code's own default;
  `max` is "prone to overthinking". Opus 4.6 keeps `max`, Sonnet 4.6 keeps `high`.
- **D6 router guidance** → `architect` tier moves from `effort=max` to `effort=xhigh`,
  `max` remaining an explicit opt-in. **This changes routing defaults for every
  spawned worker** — product-visible, land it in S-DOCS with a note in the ledger.
- **D2 abstraction** → `effort_level_count -> u8` is replaced by
  `effort_ladder -> &'static [&'static str]`. The count is lossy: `[low,medium,high,max]`
  and `[low,medium,high,xhigh]` are both "4", disambiguated today by branching on
  provider in three places. A five-level Claude ladder cannot be expressed as a count.

## Done = the ledger's Done-means, all true

`claude-opus-5` parses and renders as `Opus 5 (1M)` with a 1M window; `e`/`E` reach
`xhigh` and `max` on Opus 5 and stop wrapping at `high` on Sonnet; the direct-API
harness no longer 400s; `effort_rank` orders `xhigh` correctly; workspace tests and
clippy green; `docs/keybindings.md` matches the shipped ladders.

## Reusable method (for the next model-bump campaign)

1. **Parser first.** Model-ID parsing is the chokepoint — every label, context window,
   effort ladder, and capability route hangs off it. Fix it before renaming anything.
2. **Transcribe vendor tables, never recall them.** Model semantics change between
   releases; a training prior is a silent wrong answer. Copy the table into the plan.
3. **Separate the protocol bug from the feature bump.** The 400 was pre-existing and
   independently shippable — finding it inside a rename request is normal, coupling
   the fixes is not.
4. **Prove the shell before fanning out.** One cheap slice whose test run doubles as
   an environment gate is worth more than four slices of unverified output.
