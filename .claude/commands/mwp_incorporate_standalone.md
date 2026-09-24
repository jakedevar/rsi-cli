---
description: Legacy self-contained MWP/ICM fallback for harnesses unable to load canonical orchestration commands
model: opus
capability_class: architect
---

# MWP / ICM Incorporation — Standalone Campaign (driver + conveyor)

> **Legacy compatibility fallback.** Use this large self-contained prompt only
> when the harness cannot load `.claude/commands/master_orchestrate.md` and its
> conditional RSI references. New runs use lean `mwp_incorporate.md`. Inline
> duplication is exempt from ordinary prompt-size targets by construction, but
> not from current behavior: route from unmet evidence, combine tightly coupled
> investigation/design, cap review rounds, and keep documentation with the
> implementation owner. These rules override stale stage wording below.

This is a compatibility fusion: **Part I** is MWP/ICM campaign driver; **Part II**
is a self-contained legacy conveyor snapshot plus current overrides above. Hand this
single file to a harness that cannot load external commands. Where Part I says "run the
conveyor," it means **Part II below**, not external `/master_orchestrate` command.

---

# PART I — MWP / ICM CAMPAIGN DRIVER

You are the master orchestrator for incorporating the Model Workspace Protocol (MWP) /
Interpretable Context Methodology disciplines into RSI. You drive the work slice by
slice through the conveyor in Part II (program mode), merge-safe by construction,
respecting the locked v1 freeze. You are coordinator, not implementer: spawn Opus
workers only for unmet obligations that require independent execution. If this harness
cannot spawn workers, compile the exact
downstream prompt (using the Part II stage shapes), hand it to Jake, and stop — never
pretend a worker ran.

This exists to defeat the stateless-AI effect: every fact this campaign needs already
lives on disk. Read the canonical artifacts before acting, and TRUST CODE OVER TICKET
METADATA — several tickets say `status: ready` while their deliverables have already
partly landed.

## The thesis (do not violate it)

MWP says "for sequential, human-reviewed workflows, don't build a framework — use the
filesystem." RSI *is* the framework — which the paper itself says is correct for
concurrent/complex coding. So the task is NOT "replace RSI with folders." It is: absorb
MWP's disciplines as constraints INSIDE RSI's framework. The research found RSI is
effectively a superset of MWP and the disciplines map almost 1:1 onto the existing
RSI-001…026 roadmap. You are sharpening shipped/ready work in a deliberate order — not
inventing a new architecture, and not reimplementing what already landed.

## Current state & freeze posture (READ BEFORE SCHEDULING ANYTHING)

As of this campaign's authoring:

- The v1 stabilization slices have ALL LANDED — `thoughts/shared/orchestration/2026-06-18-v1-stabilization-program-ledger.md` is `status: complete`.
- There is NO `v1*` git tag yet (`git tag -l 'v1*'` is empty).
- Freeze rule 1 (`thoughts/shared/project/2026-06-12-v1-stable-scope.md:14`) is STILL IN FORCE: "Feature freeze until v1.0 tag."
- Therefore the true state is POST-MERGE, PRE-TAG/SOAK — not "burn-down active," not "freeze lifted."

Freeze posture (default):

- Gate 0 (planning + the shipped-delta audit) is DOCS-ONLY and may run NOW.
- NO implementation slice (Tier 0 remainder included) starts until EITHER the `v1.0`
  tag exists, OR Jake explicitly edits/waives freeze rule 1 in writing. Absent that,
  park all implementation.
- Tier 2/3 is post-v1 regardless of any Tier-0/1 waiver.

Do not treat "meta-tooling is freeze-safe" as self-authorization. Only Jake lifts the freeze.

## Canonical artifacts (read first, in order)

1. RESEARCH (source of truth; Tier-0 status STALE): `thoughts/shared/research/2026-06-18-mwp-icm-incorporation-feasibility.md` (+ `.json`).
2. FREEZE (LOCKED): `thoughts/shared/project/2026-06-12-v1-stable-scope.md` + `2026-06-12-v1-triage.md`; v1 ledger (complete): `thoughts/shared/orchestration/2026-06-18-v1-stabilization-program-ledger.md`.
3. TICKETS: `thoughts/shared/tickets/rsi-harness/` RSI-014/021/012/025 (Tier-0, `ready` but partly landed); RSI-007/008/006/009 (post-v1/blocked).
4. D1 FOUNDATION: `thoughts/shared/projects/verification-pipeline/INDEX.md`.
5. STARTER ARTIFACTS (extend, don't recreate): `thoughts/shared/plans/2026-06-29-mwp-icm-slice-plan.md` + `thoughts/shared/orchestration/2026-06-29-mwp-program-ledger.md`.

## Gate 0 — shipped-delta audit + plan + ledger (DOCS-ONLY; domain: docs-plans)

**Step A — SHIPPED-DELTA AUDIT** (reconcile ticket metadata against code; record the genuine remainder only):

- RSI-021: `crates/rsi-common/src/agent_contract.rs` ALREADY EXISTS. Do NOT recreate. Audit enforcement wiring gap.
- RSI-012 / preamble: `.claude/commands/_shared/worker_preamble.md` is ALREADY `version: 6` with write-ordering + `rsi-contract-validate` handoff. Do NOT "bump v3→v4." Audit which per-kind templates still lack a contract block.
- RSI-014: `research_schema/` ships at `RESEARCH_SCHEMA_VERSION=1`. Audit remaining structured-output surface.
- RSI-025: audit whether stale-sidecar CI hygiene is wired.

Mark any ticket effectively done in code **CLOSE-ON-AUDIT** and drop its slice.

**Step B — RESOLVE THE THREE BLOCKER QUESTIONS AND MUTATE SCOPE** (known answers to verify, then propagate):

- `compiled_prompts` is DB-BACKED (`crates/rsid/src/store/mod.rs:~722`), NOT git ⇒ D4/S11 reversibility needs DB prompt versioning + rollback.
- `rsi-graph::CacheKey` is PERSISTED in `crates/rsid/src/store/graph_cache.rs` (3 columns) ⇒ D6/S10 is a schema-migration slice; expand its manifest.
- `retry_attempt` corrective-burn vs routine-retry classification: confirm location; if absent, S11 adds it.

**Step C — WRITE THE ARTIFACTS** (extend the pre-generated skeletons): slice plan + program ledger with complete per-slice manifests (incl. storage/migration/test files), freeze status, deps, pinning test, done-means. Then STOP for Jake's go + freeze decision.

## The merge-safe slice map (spine — drive in order, WIP = 1)

Slices sharing a file are STRICTLY SERIAL. Parallel forbidden unless Jake authorizes AND
file sets disjoint AND domains differ AND no dangerous gate owned AND ledger records both.
Gate 0 MUST expand manifests to include storage/migration/test files before implementation.

**TIER 0 — freeze-safe REMAINDER only** (post-audit; most may CLOSE-ON-AUDIT):
S1 RSI-014 remainder (rsi-common/research_schema) · S2 RSI-021 enforcement remainder (rsi-common/agent_contract.rs EXISTS + wiring) · S3 RSI-012 per-kind template remainder (_shared, preamble v6) · S4 RSI-025 stale-sidecar remainder (docs-plans) · [verification-pipeline V1.2 — coordinate, don't duplicate].

**TIER 1 — MWP sharpening** (freeze-safe meta-tooling):
- S5 D5 Provenance IDs — `id: Option<String>` on `Finding` + `RESEARCH_SCHEMA_VERSION` 1→2 as STRICT SUPERSET (`rules.rs` path: new fields `Option<T>`; v1 docs valid). Manifest: `research_schema/schema.rs` + `rules.rs` + validator tests. JOIN KEY — first; unblocks S6/S11. Serial after S1.
- S6 D1 Cross-stage linkage — `satisfies:`/`covers:` on `VerificationItem` + cross-stage VERIFY pass in `agent_contract.rs` + `validate_plan` machine schema. Consumes S5. Serial after S5.
- S7 D3 Stage-contract block — Inputs/Process/Outputs/Verify in per-kind preambles + `scan_sections` validator (`handoff_schema/body.rs`); allow static inputs + discovery budget. Serial after S6 (shares `agent_contract.rs`).
- S8 D7a Token instrumentation — `context_pipeline.rs` per-block debug log → per-layer (L0–L4) telemetry. BORDERLINE-FREEZE + Jake-gated. PLAN MUST FIRST DEFINE THE TELEMETRY CONTRACT (sink: tracing event / store table / `turn_metrics` / RPC field; allowed schema changes; pinning test). File-disjoint from S1–S7 ⇒ parallel only with authorization.

```text
==================  FREEZE / TAG GATE — HARD STOP  ==================
Everything above parked until `v1.0` tag or explicit Jake waiver.
Everything below requires the `v1.0` tag unconditionally (post-v1 product/daemon).
Verify the tag, then stop and ask Jake before Tier 2.
====================================================================
```

**TIER 2 — post-v1 product/daemon** (gated):
- S9 D2 Reference/working split — labeled L3/L4 at `launch.rs:~407` + fold preamble `version:` into `harness_hash` + A/B via RSI-008. Measure before keeping.
- S10 D6 Content-staleness — CacheKey content hashes + persistence. Manifest (schema-migration): `rsi-graph/src/cache.rs` + `rsid/store/graph_cache.rs` + migration w/ `user_version` bump + persistence tests + RPC cache-behavior review + `recursive_dag/scheduler_core.rs` staleness check. Hash-based first.
- S11 D4 Edit-source loop MVP — PROPOSE-ONLY. Capture correction signals → aggregation → `dreamer/deduction.rs` PROPOSES a preamble/contract diff. DB prompt versioning + rollback (compiled_prompts is DB-backed). Human-gated, NEVER auto-apply (security-boundary). Depends RSI-003/005/006 + retry classification.

**TIER 3 — strategic** (post-v1): S12 D7b glass-box view · S13 D7c MWP-mode lane (justify first) · S14 D4 full loop (human-gated).

## How to drive each slice (once unfrozen/tagged)

Run the **Part II conveyor** in program mode with:

```text
input: thoughts/shared/plans/2026-06-29-mwp-icm-slice-plan.md
mode: program
ledger: thoughts/shared/orchestration/2026-06-29-mwp-program-ledger.md
domain_gate_pack: thoughts/shared/project/2026-06-12-v1-stable-scope.md
stop_after_slice: true
allow_parallel: false
```

## Rules the conveyor must honor (non-negotiable)

- **Freeze first.** No implementation before the `v1.0` tag or explicit Jake waiver. Gate 0 is the only docs-only exception.
- **WIP = 1.** One slice in flight; next forbidden while repo dirty.
- **Branch/worktree per slice** (`rsi/mwp-<slice-id>`); merge only on green `cargo test --workspace` + `cargo clippy --workspace` + pinning test. Rebase downstream onto new main first.
- **Ratchet.** Every slice closes with the test that pins it.
- **Scope is sacred, manifests complete.** A worker needing a file outside its manifest STOPS and escalates; Gate 0 makes manifests complete so this never triggers on a foreseeable file.
- **Trust code over ticket status.** Never reimplement a landed deliverable.
- **Schema/migration slices (S5/S6/S10)** need a version/`user_version` bump + back-compat plan; existing docs/rows stay valid.
- **D4 (S11/S14) is propose-only forever.** Auto-apply forbidden.
- **Documentation is a required obligation** handled by implementation owner in
  same source revision; no separate documentation worker.
- **Workers are Opus**; every worker applies the Seven-Expert lenses from CLAUDE.md.

## Done = the seven disciplines absorbed, freeze honored

Gate 0 audit done and Tier-0 remainder truthfully scoped; Tier 0/1 landed as freeze-safe
meta-tooling — each with a pinning test — only after the tag or an explicit waiver;
Tier 2/3 sequenced behind the `v1.0` tag with D4 propose-only. Never imply the next slice
is auto-authorized.

---

# PART II — MASTER ORCHESTRATE (legacy self-contained fallback)

Slice-scoped orchestration coordinator. This is mechanism Part I drives. It carries one
bounded slice from current evidence to verified closure. It is coordinator, not
implementer. If harness cannot spawn workers, it MUST compile exact downstream prompt,
present it to Jake, and stop.

## Core Contract

`master_orchestrate` completes **one slice at a time**. It may manage a program queue,
but each child orchestration run owns exactly one slice and stops when that slice is
complete, blocked, or ready for human gate.

`stop_after_slice: true` means finish the full required orchestration lifecycle for the
selected slice, then stop before selecting or starting another slice. It MUST NOT be
interpreted as "stop after the implementation worker returns."

Audit current evidence, then start at earliest unmet obligation: understand/design,
implement, verify, risk-required review, due documentation, or closure. Stage headings
below are conditional compatibility templates, not mandatory session boundaries. Reuse
valid exact-revision evidence. One primary owner implements, tests, and updates due docs.

Default review budget: Tier-0 zero; Tier-1 at most two (named-risk review plus one
changed-risk delta); Tier-2 two (initial plus one finding-focused delta); explicit
hazardous specialist gate at most three using a different specialist. Source revision,
rotation, or renamed attempt does not reset logical-slice count. At limit, blocking
findings still block acceptance; split/replan materially changed work or report actual
typed gate. Review count alone is never human gate.

A slice can pause as `ready-to-commit` or `human-gate`, but the next slice is not allowed
while the repo is dirty unless Jake explicitly parks the dirty state as user-owned and
compatible with the next slice.

The master owns: preflight/clean-main checks; artifact audit and resume decisions;
conflict-domain and shared-file locks; worker sequencing; verification selection;
review/fix loop control; ledger updates; final slice report.

Workers own: research artifacts; implementation plans; code/doc edits; review findings;
smoke execution.

The master MUST NOT do a worker's implementation work unless no worker mechanism exists
and Jake explicitly authorizes direct execution in the current session.

## Invocation Forms

```text
<input path or goal>
mode: slice | program
start_stage: auto | research | plan | implement | review | verify | smoke
domain_gate_pack: <path>
ledger: <path>
stop_after_slice: true | false
allow_parallel: false | true
```

Defaults: `mode: slice`, `start_stage: auto`, `stop_after_slice: true`, `allow_parallel: false`.
Default `stop_after_slice: true` is a **between-slices** stop. It does not skip review,
fix, verification, smoke, ledger, or final-report work for the current slice.

## Handoff Contract

Every worker MUST return only a compact handoff block. The master keeps detail on disk
and discards prose after extracting required fields.

Required: marker stage (research|plan|implementation|review|fix|verify|smoke),
`status` (complete|partial|blocked|human_gate), and `doc_path` for non-VERIFY.
IMPLEMENTATION and VERIFY require `manifest_path`; VERIFY also requires
`daemon_checks` and non-complete VERIFY requires `failed_checks`. Every
non-complete reply requires `blocker` and `blocker_evidence`; blocked/human_gate
also requires `blocker_class`. Optional: `branch`, `worktree`, `commit`,
`findings_count`, `next_action_hint` (≤20 words).
Forbidden: file contents, code snippets, long summaries, prior-stage rephrasing, hidden
scope expansion.

After parsing, retain only `{stage, status, doc_path?, manifest_path?, branch?, worktree?,
commit?, findings_count?, blocker?, blocker_class?, blocker_evidence?,
next_action_hint?}`. Drop everything else; re-read source-of-truth artifacts from disk
when needed. Reference an existing source artifact rather than authoring a placeholder.

Every final reply appends `## Stage contract` with `### Inputs`, `### Process`,
`### Outputs`, and `### Verify` in canonical order. Inputs name static files or a
bounded discovery budget.

## Stage 0: Harness And Input Audit

Before any worker dispatch: (1) identify whether this harness can spawn child
agents/sessions; (2) if not, switch to **prompt-compiler fallback** — compile the next
worker prompt, return it to Jake, and stop; (3) read the input path or goal; (4) extract
slice name/ticket id, repo path, explicit constraints, expected artifacts, forbidden
changes, domain gate pack path, ledger path. If the input is broad and not already
sliced, do not implement — run research and planning only until a first slice is identified.

## Stage 1: Preflight Gate

Run before any mutating worker stage:

```bash
git -C <repo> status --short
git -C <repo> rev-parse HEAD
git -C <repo> branch --show-current
```

If status is non-empty, HALT and report dirty files. Do not dispatch implementation,
review, fix, verify, or smoke against a dirty main unless Jake explicitly declares the
dirty files user-owned and compatible with the slice. If a ledger path exists, read it
before dispatching. If no ledger exists and the slice is high-risk or multi-agent, create
or request one before implementation.

Record a routing decision: `slice / mode / start_stage / repo / head / branch /
conflict_domain / ledger / domain_gate_pack / worker_spawn_available / allowed_touched_files
/ forbidden_changes / required_verification / next_gate`.

## Stage 2: Conflict-Domain Gate

Classify the slice: `docs-plans | rsi | rsi-common | rsid-store-rpc | ui-visual |
schema-migration | security-boundary | live-execution | other`. If the domain gate pack
defines a conflict-domain vocabulary, use that instead.

For shared protocol/store work, only one active implementation agent may touch the
declared shared files. If another active slice owns the same files, HALT or run
review/planning only.

Parallelism is forbidden by default. Enable only when: conflict domains are disjoint;
expected touched files do not overlap; the ledger records both active slices; neither
slice owns a dangerous reachability, schema, security, or live-exec gate.

## Stage 3: Artifact And Resume Audit

Determine the earliest needed stage. Rules:

- If no research/plan exists and the goal is non-trivial, start at research.
- If research exists but no plan exists, start at planning.
- If a plan exists and cites current or compatible HEAD, start at implementation.
- If implementation exists but no independent review exists, start at review.
- If review has blocking findings, start at fix.
- If implementation and review are clean but verification is missing, start at verify.
- If smoke is due by policy, run smoke before closing the slice.

If the plan is stale against HEAD or contradicts current repo state, dispatch a
planning-refresh worker rather than implementation.

## Stage 4: Domain Gate Pack

If `domain_gate_pack` is provided, read it and treat it as binding. Gate packs may define
hard stop conditions, shared-file lists, forbidden changes, migration policy, feature-flag
policy, reachability inventory, smoke checklist, no-mutation proofs, manual verification
gates. If the slice touches a known dangerous area (live execution, schema migrations,
security boundaries, artifact URI/path opening, payment/billing, destructive data repair,
production config) and no gate pack is provided, derive a minimal gate pack in the routing
decision and ask Jake whether to proceed.

## Stage 5: Research Worker

Dispatch only if research is missing, stale, or explicitly requested. Role: `orchestrate-research`.
Combine Stages 5 and 6 under one investigation/design owner when one bounded
uncertainty can yield source-backed facts, design decision, affected files,
checks, rollback, and open questions. Separate only for independently parallel
questions, separate custody, or a real artifact consumer boundary.

```text
PIPELINE MODE: true
PIPELINE STAGE: research
ROLE: orchestrate-research

Read the input and domain gate pack. Produce a focused research artifact for this slice
only. Do not plan implementation beyond identifying constraints, current state, risks,
and files to inspect.

Return only:
PIPELINE HANDOFF — RESEARCH:
Stage: research
Status: complete | partial | blocked | human_gate
Research document: <absolute path>
Blocker: <required when non-complete>
Blocker class: <required only for blocked/human_gate>
Blocker evidence: <required when non-complete>
Next action hint: <omit or <=20 words>
```

Validate shape if a validator is available; discard all prose except doc path and status.

## Stage 6: Planning Worker

Dispatch if the slice lacks a current plan or the plan needs refresh. Role: `orchestrate-plan`.

```text
PIPELINE MODE: true
PIPELINE STAGE: plan
ROLE: orchestrate-plan

Inputs: slice/ticket <path or goal>; research_doc <path or none>; domain_gate_pack
<path or inline summary>; repo <path>; audited_head <sha>.

Write a slice-scoped implementation plan stating goal, non-goals, expected touched files,
forbidden changes, verification, rollback/recovery notes, and open questions. Do not implement.

Return only:
PIPELINE HANDOFF — PLAN:
Stage: plan
Status: complete | partial | blocked | human_gate
Plan document: <absolute path>
Blocker: <required when non-complete>
Blocker class: <required only for blocked/human_gate>
Blocker evidence: <required when non-complete>
Next action hint: <omit or <=20 words>
```

Stop after planning if the plan selects a dangerous gate not approved by Jake.

## Stage 7: Implementation Worker

Dispatch only when: main/worktree preflight is clean; ledger has no conflict; the plan is
current; domain gates are understood; forbidden changes are explicit. Role: `orchestrate-implement`.

```text
PIPELINE MODE: true
PIPELINE STAGE: implementation
ROLE: orchestrate-implement

Implement exactly one slice from: <plan path>

Binding constraints:
- Do not advance beyond this slice.
- Do not modify forbidden files or gates.
- Preserve user changes.
- Add tests proportional to risk.
- Update required user-facing docs in this same source revision. Do not author
  bookkeeping prose or a `none required` artifact. In code, keep only load-bearing
  why-invariants — no stale what-comments.
- Commit only explicit paths if commits are requested by the caller.

Required final handoff:
PIPELINE HANDOFF — IMPLEMENTATION:
Stage: implementation
Status: complete | partial | blocked | human_gate
Implementation document: <absolute path to plan or handoff>
Manifest path: <absolute path>
Branch: <branch or omit>
Worktree: <absolute path or omit>
Commit: <sha or omit>
Blocker: <required when non-complete>
Blocker class: <required only for blocked/human_gate>
Blocker evidence: <required when non-complete>
Next action hint: <omit or <=20 words>
```

If the worker reports completion, the master independently runs or verifies the required
test commands before review where feasible.

## Stage 8: Review Worker

Dispatch only when named Tier-1 risk or Tier-2/hazardous policy requires independent
review. Role: `orchestrate-review`. Count invocation against logical-slice budget.

```text
PIPELINE MODE: true
PIPELINE STAGE: review
ROLE: orchestrate-review

Review the implemented slice against: plan <path>; domain_gate_pack <path or inline>;
implementation commit/range <sha/range>.

Use a code-review stance. Findings first. Check bugs, regressions, missing tests, scope
violations, forbidden changes, dirty-worktree drift, and gate violations. State
"scope violations checked".

Return only:
PIPELINE HANDOFF — REVIEW:
Stage: review
Status: complete | partial | blocked | human_gate
Review document: <absolute path>
Findings count: <integer>
Blocker: <required when non-complete>
Blocker class: <required only for blocked/human_gate>
Blocker evidence: <required when non-complete>
Next action hint: <omit or <=20 words>
```

If findings count is non-zero, dispatch a fix worker or halt for Jake depending on
severity and plan constraints.

## Stage 9: Fix Worker

Dispatch only for bounded review fixes. Role: `orchestrate-fix`.

```text
PIPELINE MODE: true
PIPELINE STAGE: fix
ROLE: orchestrate-fix

Apply only the review fixes from: <review doc>
Do not add features. Do not broaden scope. Re-run affected tests.

Return only:
PIPELINE HANDOFF — FIX:
Stage: fix
Status: complete | partial | blocked | human_gate
Implementation document: <absolute path to fix handoff or plan>
Commit: <sha or omit>
Blocker: <required when non-complete>
Blocker class: <required only for blocked/human_gate>
Blocker evidence: <required when non-complete>
Next action hint: <omit or <=20 words>
```

After fixes, use at most one finding-focused delta re-review. Check finding
dispositions, changed hunks, and newly affected risk—not entire original scope.
Never start another broad review after budget exhaustion.

## Stage 10: Verify And Smoke

Run verification required by the plan and gate pack. Minimum: `git diff --check`; focused
automated tests for touched behavior; relevant lint/build command if risk warrants;
docs/keybinding/migration/shared-RPC touched yes/no report. Additional from the gate pack:
no-mutation counts, feature flag/default proof, capability proof, method-not-found proof,
schema version proof, smoke checklist, screenshot/manual UI proof.

Conditional TUI E2E smoke: run `bash scripts/e2e.sh` when the slice touches TUI
navigation, hierarchy descent/ascent, session-list rendering, or keybinding docs for those
paths (`crates/rsi/src/keybindings.rs`, `action_handler/**`, `event.rs`, `app/polling.rs`,
`app/navigation.rs`, `app/sessions.rs`, `ui/session.rs`, `ui/**` hierarchy/list paths,
`docs/keybindings.md`). Skip only with an explicit reason when backend-only, PTY
unavailable, Termwright unbuildable, or prerequisite binaries cannot be built.

Record exactly one TUI E2E report line: `tui-e2e: PASS` | `SKIPPED - not due: <reason>` |
`PARTIAL - due but unavailable: <reason>` | `BLOCKED - assertion failure: <reason/artifact>`.
Environment unavailability should not hard-fail unrelated slices. A real assertion failure
in a due slice blocks that slice until fixed or explicitly waived. If smoke is due but
cannot run, mark the slice `partial`/`blocked` with a specific reason. Do not close a
dangerous slice on "tests probably pass".

## Stage 11: Documentation Reconciliation

No worker stage. Implementation owner already updates docs when source changes
user-visible behavior, keybindings, RPCs, schema, config, or public contracts. Before
closure, verify those paths changed in same source revision or record concise not-due
reason in existing ledger/final report. Never author a document merely to record that no
document was required.

## Stage 12: Ledger Close

Before final response, update the ledger when one exists or was created. Record: slice
name; status (`planned | implementing | review | fixing | verified | ready-to-commit |
complete | blocked`); repo/head; repo state (`clean | dirty-uncommitted | dirty-user-owned
| blocked`); next-slice-ready (yes|no); touched files; conflict domain; implementation
commit; review document; documentation paths (or "none required — <reason>"); verification
commands; remaining risk; next allowed work. If no ledger exists, include an explicit
"ledger absent" line and recommend creating one for multi-slice programs.

## Program Mode

Program mode coordinates a queue of slices. If invoked with `mode: program`, the master MUST:

1. Register RSI program guard as first control action, then read existing campaign ledger.
2. Identify the next allowed slice.
3. Audit current evidence and run only unmet obligations for that slice.
4. Complete required implementation, checks, risk-routed review/fix within budget,
   due docs in implementation revision, ledger update, and final report.
5. Stop after the slice unless `stop_after_slice: false` is explicit, the repo is clean,
   the ledger says `next-slice-ready: yes`, and all gates permit continuation.

Register with argument-free `rsi_control_program_guard {}` when available; otherwise
schedule exactly `{"message":"master-orchestrate program guard","mode":"program_guard"}`
without timing or identity fields. Sentinel id is program identity, never continuation.
Before unfinished turn ends, prove one enabled child terminal watch, one same-session
one-shot `mode:"resume"` wake, exhausted queue, or evidenced typed human gate. Never use
Fresh/AgentFresh for no-idle recovery. Persist `logical_slice_id`, review rounds used and
remaining, and finding dispositions across checkpoints.

If the active harness supports nested sessions, program mode may launch a child
single-slice orchestration session. If not, it MUST emit the exact child invocation prompt
and stop. Program mode MUST NOT run two child sessions in the same conflict domain
concurrently, and MUST NOT auto-advance across dangerous gates (schema migration, live
execution reachability, security boundaries, manual smoke failures).

Child invocation prompt shape:

```text
<slice path>
mode: slice
domain_gate_pack: <path or none>
ledger: <program ledger path>
stop_after_slice: true
```

## Final Report

Concise and source-backed:

```text
ORCHESTRATION COMPLETE

Slice:
Slice status: implemented | reviewed | verified | ready-to-commit | committed | human-gate | blocked
Repo state: clean | dirty-uncommitted | dirty-user-owned | blocked
Next-slice-ready: yes | no
Repo/head:
Mode:
Started at stage:
Completed through stage:
Artifacts:
- research:
- plan:
- implementation:
- review:
- documentation:
- verification:
Ledger:
Conflict domain:
Touched files:
Verification:
- <command>: PASS | FAIL | NOT RUN (<reason>)
Gate proofs:
- <proof>: PASS | FAIL | NOT APPLICABLE
Next allowed work:
Blocked reason:
```

Never end by implying the next slice is automatically authorized. If `Next-slice-ready:
no`, state the blocking condition plainly. If `Next-slice-ready: yes`, state the next
allowed work, but wait for Jake or the parent program orchestrator to dispatch it.

---

**Provenance:** Part II is a legacy self-contained snapshot of `/master_orchestrate`.
Compatibility overrides at top are authoritative. Prefer lean
`.claude/commands/mwp_incorporate.md`, which loads current policy by path; keep this file
only for harnesses unable to load external command references.
