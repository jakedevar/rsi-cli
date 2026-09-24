---
description: Prime and drive the RSI v1 stabilization burn-down via master_orchestrate program mode
model: opus
capability_class: architect
---

# v1 Burn-down — campaign driver

Single entry point for the RSI v1 "Stable" campaign. Invoke this to load the full
campaign context and carry it to a v1.0 tag, slice by slice, through
`/master_orchestrate`.

This command exists to defeat the stateless-AI effect: every fact the campaign
needs already lives on disk. This tells you which facts, in what order, under what
rules — so any session or any model can pick the work up cold and continue without
re-deriving the plan. Read the four canonical artifacts before acting.

## Lineage (how this started)

Born in session `277f2e4e` (Claude Fable 5) as a v1-stabilization diagnosis: the
bottleneck moved from writing code (cheap now) to **decision quality** (upstream)
and **verified integration** (downstream). The cure is a **ratchet** — every fix
pinned by a test so it can never silently regress — applied to a **frozen, finite
scope**. Phase 0 (freeze + scope + triage) is done and signed. This command drives
Phases 1–4.

## The four canonical artifacts (read first, in order)

1. **SCOPE** (the contract — LOCKED): `thoughts/shared/project/2026-06-12-v1-stable-scope.md`
   — 6 freeze rules, 10 v1 surfaces each with a testable "works means", the Definition of Done.
2. **TRIAGE** (the bucketing): `thoughts/shared/project/2026-06-12-v1-triage.md`
   — 57 items → 27 active → the Phase-3 slice map.
3. **SLICE PLAN** (the queue): `thoughts/shared/plans/2026-06-13-v1-burndown-slice-plan.md`
   — Gate A + 10 slices in order, conflict domains, gates, done-means, dependency graph.
4. **PROGRAM LEDGER** (the spine): `thoughts/shared/orchestration/2026-06-13-v1-program-ledger.md`
   — per-slice status; `master_orchestrate` reads and updates this every slice.

INBOX (not a queue): `thoughts/ideas-intake.md` — new ideas land here, one line, never inline in a slice.

## The non-negotiable shape: gate + 10, not 8

The triage's 8 slices (S-INPUT → S-THEME) are **Phase 3 only**. The locked
Definition of Done forces two slices plus one read-only gate *in front of* them:

- Freeze rule 3: "every fix lands with the test that pins it."
- The vim_textarea / viewer / chassis / theme bugs are render/input bugs. The only
  way to pin them is the **Tier-2 harness** (`step_once` + `TestBackend` + insta
  buffer snapshots) — which does not exist yet. That is **F2 / S-HARNESS**.
- A new harness can't be trusted on a red baseline. Close RSI-020 (the l-test
  baseline) first — that is **F1 / S-RATCHET**, the green floor.
- **Gate A** (read-only inventory) resolves the quarantine/naming decisions before
  slices 3/5/6 harden.

Order: **Gate A → F1 S-RATCHET → F2 S-HARNESS → S-INPUT → S-STATUS → S-HIER →
S-CHASSIS → S-VIEWER → S-KEYS → S-FRESH → S-THEME** (last; it snapshot-pins every
surface above it). Rip the 8 first and you rebuild the treadmill the campaign exists
to kill.

## How to drive it

For each slice, in ledger order, run the slice conveyor:

    /master_orchestrate thoughts/shared/plans/2026-06-13-v1-burndown-slice-plan.md
    mode: program
    ledger: thoughts/shared/orchestration/2026-06-13-v1-program-ledger.md
    domain_gate_pack: thoughts/shared/project/2026-06-12-v1-stable-scope.md
    stop_after_slice: true
    allow_parallel: false

Rules the conveyor must honor:

- **WIP = 1.** One slice in flight. The next slice is forbidden while the repo is dirty.
- **Ratchet.** Every slice closes with the test that pins it. No pinning test = not done.
- **Three gates.** After F1 (prove `cargo test --workspace` green), after F2 (prove
  the harness pins a real surface), at S-FRESH (live-execution — needs its own plan
  doc). In an **interactive** run, stop for Jake at each. In an **autonomous / arena**
  run, SELF-VERIFY the gate (prove green / prove the harness works) before advancing;
  only truly stop if something is physically unavailable (e.g. a live daemon a frozen
  binary cannot reflect), then skip to the next independent slice and note it.
- **Documentation is a required obligation** (`master_orchestrate`
  §Implementation, Verification, And Documentation): implementation owner updates
  docs for any keybinding / RPC / schema / user-facing change before ledger close.
- **Cut beats fix.** Quarantine or delete marginal surfaces — every removed surface
  is stability you never have to earn.

## Open vetoes (resolved 2026-06-13)

- **#15 Gable AI** → deferred post-v1 (in intake). **#24 "the bar"** → dropped (no defect).
- **#10 merge_queue**, **#33 cockpit** → quarantine (Gate A confirms with usage evidence).
- **#38 stall detector** → defer post-v1 unless Gate A sizes the wiring small (then S-FRESH).

## Done = v1.0 tag

Every DoD box in the scope contract true: green workspace + CI (RSI-020 closed),
Tier-2 harness live with a snapshot per v1 surface, every v1-bug pinned, keybindings
single-source and CI-checked, theme snapshot-pinned, coverage ratchet up-only, clippy
clean, and a 7-day soak with zero new v1-bug filings. Then the gate opens:
meta-harness, memory integration, idea-tracker-as-groups — all waiting in intake, none lost.

## Reusable method (for the next campaign, after v1)

To turn any locked scope + triage into a `master_orchestrate`-rippable queue:

1. **Surfaces → slices.** One widget/subsystem per slice; cluster bugs by the thing they live in.
2. **Floor first.** A green baseline plus the test harness that can *pin this domain*, before any burndown.
3. **Order pain-then-risk.** Highest daily-pain density first; biggest/riskiest late, on a mature harness.
4. **Snapshot-pin last.** The visual/theme sweep runs after behavior settles, so it pins final state.
5. **Spine.** One plan doc per slice, a program ledger as the spine, WIP = 1, gate the dangerous domains.
