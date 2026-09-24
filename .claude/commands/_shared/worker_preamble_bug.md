---
version: 8
kind: bug
inherits: worker_preamble.md
---

# Worker preamble — Bug

## Durable issue follow-ups

Manual issues are only durable follow-ups outside this deliverable. Prefer `rsi_control_create_issue`, then `AgentCreateIssue`, with a stable key and no identity fields. Never file this process failure manually; report write errors in the handoff without retry spam or malformed output.

Ordinary workers use create-only follow-ups. The guarded project Issue controls
execute only for the current owning-Epic lead or the `IssueCoordinate` manager;
a registered native tool does not itself grant authority. Payload/result/CAS/replay/error shapes are in the
base preamble.

This file COMPOSES ON TOP of the base contract in `worker_preamble.md`.
The base rules (read budget, return budget, forbidden content) all apply.
This file overrides ONLY the kind-specific guidance below.

Cross-stage linkage (from the base contract) applies: a VERIFY-stage handoff
carries a `satisfies:`/`covers:` line naming the research `Finding.id`(s) it
proves, and every declared key must be covered by a manifest item or the
cross-stage VERIFY pass (`cross_stage_verify_coverage`) fails the handoff.

## Investigation depth

Bug sessions begin with a reproducer, not a redesign. Before proposing any
change, you must:

1. Reproduce the failure deterministically. State the exact reproducer.
2. Identify the smallest code region that, if changed, would close the gap.
3. Confirm the fix does not regress neighboring behavior.

## Verification item categorization

Every verification item must be assigned to exactly one bucket using this
decision tree:

| Question | Yes -> bucket |
|---|---|
| Can this be expressed as a `#[test]` or `#[tokio::test]` exercising the changed code? | **AUTOMATED** - write the test, do not emit a manual item |
| Does verification require seeing TUI rendering, color, focus, or keyboard input outcome? | **TUI MANUAL** - emit under TUI manual |
| Is the observable behavior a daemon-side effect (DB row, log line, RPC response payload, sandbox state, socket existence)? | **DAEMON AUTONOMOUS** - emit under Daemon-level with a concrete check command |

Hard rules: automated is mandatory if possible; TUI manual is the fallback
only; every daemon-level item MUST include `check:` and `expected:` lines.
You may NOT write to the manifest file directly. Emit verification items in
your handoff body under `VERIFICATION_ITEMS:` with bucket subsections; the
orchestrator writes them to the manifest atomically at phase seal. Workers may
not append mid-phase or per-sub-task entries.

```markdown
EXAMPLE A - automated bucket
Change: added Session.workflow_id_override field
Verification: round-trip serde JSON with the field set and unset
Bucket: AUTOMATED - write a #[test] in crates/rsi-common/src/types.rs::tests
Manual item emitted: NONE

EXAMPLE B - daemon bucket
Change: spawn coordinator now writes workflow_id_override on every child row
Verification: spawned children have the column populated correctly
Bucket: DAEMON AUTONOMOUS
Manual item emitted under ### Daemon-level:
  - [PENDING] Spawned child rows carry workflow_id_override matching the LaunchConfig override field
    check: cargo run -q -p rsi-common --bin rsi-rpc -- LaunchSession --params '{"working_dir":"/tmp/x","workflow_id_override":"00000000-0000-0000-0000-000000000001"}' && sqlite3 $RSI_DB "SELECT workflow_id_override FROM sessions ORDER BY created_at DESC LIMIT 1"
    expected: 00000000-0000-0000-0000-000000000001

EXAMPLE C - TUI bucket
Change: gv overlay now resolves workflow draft via effective_topology walk
Verification: leaf under Epic with workflow_id=X opens overlay with the right draft
Bucket: TUI MANUAL (cannot be automated - no terminal renderer mock today)
Manual item emitted under ### TUI manual:
  - [ ] gv overlay on a leaf-under-Epic resolves the Epic's workflow draft (not a stale or empty draft)
  - [ ] Note: blocked from automation - see thoughts/shared/research/2026-05-01-agent-driven-tui-testing-feasibility.md
```

## Stage contract

Declare this stage's I/O as an `Inputs / Process / Outputs / Verify` block.
The four `###` sub-headings are REQUIRED; self-check them against
`scan_contract_block` (`crates/rsi-common/src/handoff_schema/body.rs`), which
`rsi-contract-validate` now runs on your reply (exit 2 if the block is present
but malformed): all
four must be present, and `### Inputs` MUST declare EITHER named static inputs
OR a code-discovery budget (or both). A bug's inputs are usually discovered —
declaring only a discovery budget is ACCEPTED, so do not invent a fixed file
list you do not have.

### Inputs

- Static inputs: the bug report / failing assertion, the reproducer, and any
  named file the report points at (e.g. `crates/rsid/src/session.rs`).
- Discovery budget: `rg`/glob across the workspace to localize the smallest
  code region that owns the defect — the offending region is FOUND, not
  pre-listed.

### Process

- Reproduce deterministically, localize to the minimal region, apply the
  smallest fix, add a regression test that fails pre-fix.

### Outputs

- A minimal diff touching only the files the fix requires, plus a regression
  test, plus `VERIFICATION_ITEMS:` in the handoff body.

### Verify

- The regression test fails on the pre-fix code and passes on the post-fix
  code; no neighboring test regresses.

## Typical failure modes (bug-specific)

- Fix the symptom, not the cause: a one-line patch that masks the underlying
  invariant violation. If the invariant is unclear, surface it.
- Test added that asserts the buggy behavior: write the test against the
  desired post-fix behavior.
- Scope creep into refactor: a bug session output is a minimal targeted fix.
  Refactors belong in a separate session of `kind: refactor`.

## Success criteria

- A regression test exists that fails on the pre-fix code and passes on the
  post-fix code.
- The diff is minimal — touches only files necessary for the fix.
- The repro steps documented in the original report still produce the failure
  on the pre-fix code (i.e., the bug was real, not a misread).
