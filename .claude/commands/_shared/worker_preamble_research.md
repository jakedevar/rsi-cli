---
version: 9
kind: research
inherits: worker_preamble.md
---

# Worker preamble — Research

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

## Shape of work

Research sessions produce findings, not code. Output is a written artifact
under `thoughts/shared/research/` that answers a specific question with
evidence drawn from the codebase, the literature, or external systems.

1. State the question precisely BEFORE investigating. Sharpen if vague;
   do not spelunk on a fuzzy premise.
2. Gather evidence with file:line citations. Every claim about codebase
   state must link to the source.
3. Conclude with a recommendation IF warranted. Some research questions
   have a survey answer, not a directive answer.

Use the base contract's exact `[observed]`, `[source]`, and `[inferred]`
vocabulary; do not redefine it for research. Machine-readable counts, field
lists, schemas, and DDL are `[observed]` only when mechanically extracted from
the primary artifact in this pass, or `[source]` when cited to an authoritative
artifact with a reproducible extraction. A tool or helper's exact shape is
`[inferred]` until the current surface is shown to express it. Preserve that
classification in the research artifact so planning cannot silently promote
an inference into an acceptance criterion.

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
OR a code-discovery budget (or both). Research evidence is inherently
discovered — declaring only a discovery budget is ACCEPTED, so do not pretend
to a fixed source list before the sweep.

### Inputs

- Static inputs: the precise research question plus any named prior artifact
  (e.g. an earlier doc under `thoughts/shared/research/`).
- Discovery budget: `rg`/glob across the codebase (and external references)
  to gather the evidence — sources are discovered during the sweep, not
  pre-enumerated.

### Process

- Sharpen the question, gather evidence with file:line citations, conclude
  with a recommendation only if warranted.

### Outputs

- A self-contained markdown artifact under `thoughts/shared/research/` with a
  question, an evidence section, and a conclusion, plus `VERIFICATION_ITEMS:`
  in the handoff body.

### Verify

- Every load-bearing claim carries a file:line citation or external
  reference; the artifact is actionable without re-running the research.

## Typical failure modes (research-specific)

- Premature implementation: writing code instead of findings. If research
  has graduated, hand off to a Feature or Refactor session.
- Citation-free claims: "the codebase does X" without a path:line. The
  master cannot verify these; they are worse than no claim.
- Scope drift: starting on question A and answering question B. Document
  surface-area discoveries as follow-ups, do not pivot mid-session.

## Success criteria

- A markdown artifact exists under `thoughts/shared/research/` with a
  question, evidence section, and conclusion section.
- Every load-bearing claim has a file:line citation or external reference.
- The artifact is self-contained — actionable without re-running research.
