---
description: Check an implementation against its plan - success criteria, evidence tiers, cross-stage coverage
---

# Validate Plan

Decide whether a plan was actually carried out: every phase landed, every
success criterion holds, and nothing the plan promised was dropped. Report
deviations as findings; do not fix them here.

## Setup

1. **Where are you?**
   - Same session that implemented the plan: start from what this session did.
   - Fresh session: reconstruct what happened from git and the code.
2. **Find the plan.** Use the path you were given. Otherwise look for a plan
   path in recent commit messages; if none turns up, ask for it.
3. **Collect the raw evidence:**
   ```bash
   git log --oneline -n 20
   git diff <first-implementation-commit>^..HEAD --stat
   cd "$(git rev-parse --show-toplevel)" && make check test
   ```

## Validation Process

### Step 1: Know What Should Exist

1. Read the plan in full.
2. Extract, per phase: the files it said would change, its automated and manual
   success criteria, and the behaviour it promised.
3. Verify in parallel, one sub-task per area (skip areas the plan does not touch):
   ```
   Sub-task A - Schema and persistence:
   Did the plan's migration land as an inline `if version < N` block in
   crates/rsid/src/store/mod.rs with the matching user_version bump?
   Compare the DDL with the plan's DDL.
   Return: planned vs actual, with file:line.

   Sub-task B - Code changes:
   List the files changed for [feature] and compare each against the plan's
   "Changes Required" entry.
   Return: per-file planned vs actual.

   Sub-task C - Tests:
   Were the planned tests added or updated? Run them.
   Return: pass/fail per test target and any planned test that is missing.
   ```

### Step 2: Phase by Phase

For each phase:

1. **Ticked versus real.** For every `- [x]` in the plan, confirm the code
   shows it; a tick without code is a deviation.
2. **Automated criteria.** Run each listed command and record pass or fail. On
   a failure, find out why before moving on.
3. **Manual criteria.** List what still needs a human, as concrete steps.
4. **Edge cases.** Failure paths handled? Inputs validated where the plan said
   they would be? Anything existing that this could break?

### Step 2.5: Evidence-Tier Audit (Blocking)

Audit the actual target plan, not this command template, using the shared
`[observed]`, `[source]`, and `[inferred]` vocabulary. For each load-bearing
claim, require its inline tag and primary artifact/result citation or
reproducible source extraction. A command/helper name alone is not evidence;
machine-readable counts, field sets, schemas, and DDL must be mechanically
extracted. An exact tool or helper shape is `[inferred]` until the current tool
surface is shown to express it.

`[inferred]` is forbidden in Acceptance Criteria and Success Criteria. A plan
with any load-bearing `[inferred]` claim must execute and reclassify the claim
or downgrade readiness; it cannot be labeled `decision-complete` or
`implementation-ready`, described as `ready for implementation`, or given any
equivalent readiness promise.

Both prohibited shapes are blocking validation failures, including under
automated/manual subsections and equivalently named acceptance checklists:

1. Report every `[inferred]` acceptance/success criterion with the exact plan
   location and require execution plus reclassification; it cannot be repaired
   by deleting the tag while leaving the unsupported claim.
2. If a readiness promise remains while any load-bearing `[inferred]` claim
   remains, report the exact location and require execution/reclassification or
   an explicit readiness downgrade.

Do not proceed to an implementation-complete verdict while either failure is
present. Record it under **Deviations from Plan** as a blocking `FAIL`.

### Step 3: Write the Validation Report

```markdown
## Validation: [plan name]

### Implementation Status
- Phase 1: [Name] - complete
- Phase 2: [Name] - complete
- Phase 3: [Name] - partial (see Deviations)

### Automated Checks
- PASS `make build`
- PASS `make test`
- FAIL `make lint` (3 warnings)

### Code Review Findings

#### Matches Plan:
- [planned change] - [file:line]

#### Deviations from Plan:
- [what differs] - [file:line] - [blocking FAIL | acceptable, and why]

#### Potential Issues:
- [risk the plan did not cover] - [file:line]

### Manual Testing Required:
1. [Area]:
   - [ ] [concrete step and expected result]

### Recommendations:
- [what to do before this merges]
```

## Cross-stage linkage gate (S6/D1 — machine-checkable)

This gate makes research→plan→impl drift a **hard failure**, not a silent gap.
It is a schema assertion, not a style preference: a plan item left uncovered by
the implementation's verification manifest MUST fail validation.

### Plan-item schema requirement (assert per item)

Every plan item that traces to research MUST declare, in machine-parseable
form, the research `Finding.id`(s) it `satisfies`:

```yaml
# per plan item (frontmatter or a `satisfies:` line under the item)
satisfies: [F-001, F-014]   # research Finding.id(s) — RESEARCH_SCHEMA_VERSION >= 2
```

- `satisfies:` is a list of `Finding.id` strings drawn from the research
  sidecar (`<doc>.json`, `findings[].id`). An item that closes a planned
  finding but declares no `satisfies:` is an **uncovered plan item** → FAIL.
- A plan item that is pure scaffolding (no research antecedent) MAY omit
  `satisfies:`; note it explicitly so the omission is intentional, not drift.

### Manifest coverage requirement (assert per finding)

The verification manifest (`thoughts/shared/verification/<ticket>-<date>.md`) is
the source of truth for what the implementation actually covered. Every
`Finding.id` a plan item declares under `satisfies:` MUST appear under some
verification item's `satisfies:`/`covers:` line in the manifest.

### The gate (fails the validation)

For each declared linkage key `K` across all plan items:

1. `K` MUST be covered by ≥1 manifest item's `satisfies`/`covers`.
2. If any `K` is uncovered → the cross-stage VERIFY pass returns
   `ContractError::UncoveredLinkage` and this validation FAILS.

Machine check (the VERIFY handoff carries the declared keys on a `satisfies:`
line; the manifest is parsed for coverage). This is the **real, armed**
invocation — the same binary the master pipes the VERIFY reply through at
`master_implement.md` Step 3.5, now running `cross_stage_verify_coverage`:

```bash
# reply.txt = the VERIFY-stage `PIPELINE HANDOFF — VERIFY:` block, which
# carries the declared linkage keys on its `satisfies:`/`covers:` line.
# The manifest path is auto-resolved from the handoff's `Manifest path:` field,
# or forced with `--manifest`. Exit 2 == an uncovered plan/research Finding.id.
cargo run -q -p rsi-common --bin rsi-contract-validate -- \
  <TICKET> --manifest thoughts/shared/verification/<ticket>-<date>.md < reply.txt
# exit 0 → every declared Finding.id is covered by a manifest satisfies/covers
# exit 2 → ContractError::UncoveredLinkage{keys:[...]} on stdout — DRIFT, FAIL
```

The gate is dormant (exit unchanged) for non-VERIFY handoffs, VERIFY handoffs
that declare no linkage, or when no manifest resolves — it bites ONLY when a
VERIFY handoff declares linkage AND a manifest is resolvable.

Report an uncovered plan item under **Deviations from Plan** as a blocking
`FAIL`, never a soft note — a dropped planned finding is the exact drift this
gate exists to catch.

## Validating Your Own Work

If this session did the implementation, use the todo list and the transcript
as a map, not as proof: re-run the checks and re-read the code. Call out any
shortcut or unfinished item yourself before the report does.

## Rules

1. Run every automated check the plan lists; a skipped check is reported as NOT RUN, never as a pass.
2. Evidence over impressions: every finding carries a `file:line` or a command result.
3. Report successes and failures alike; a clean section still says so.
4. Ask whether the change solves the stated problem, not only whether it matches the plan's letter.
5. Flag anything that will be hard to maintain.

## Validation Checklist

- [ ] Every phase ticked complete is actually present in the code
- [ ] Automated tests pass
- [ ] New code follows the surrounding patterns
- [ ] No regressions in behaviour the plan did not mean to change
- [ ] Failure paths are handled
- [ ] Docs updated where behaviour changed (e.g. `docs/keybindings.md`)
- [ ] Manual test steps are concrete
- [ ] No test asserts that user-visible information is ABSENT
      (`!contains(...)`, `assert_not_visible`, a snapshot with a name removed).
      Such a test pins data loss as a requirement; require the positive
      assertion instead.
- [ ] Implementation honors every constraint the plan declared, not just its
      design decisions

## Where This Fits

1. `/implement` lands the phases and commits each one.
2. `/validate_plan` checks the result against the plan.

Run it after the implementation commits exist: git history is its main
evidence of what was done.
