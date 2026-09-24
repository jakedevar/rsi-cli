---
version: 8
kind: feature
inherits: worker_preamble.md
---

# Worker preamble — Feature

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

Feature sessions add a new capability to the system. Output is new code
plus tests plus the smallest documentation update needed for the feature
to be discoverable.

1. Identify the existing seam where the feature plugs in. Do NOT carve a
   new abstraction unless the existing one provably cannot host the change.
2. Add tests FIRST when the feature has a stable observable contract.
   Tests last when the contract is genuinely exploratory.
3. Surface the new capability in the smallest user-visible way that
   matches existing conventions (keybinding, RPC method, config key).

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
OR a code-discovery budget (or both). A feature's integration seam is usually
discovered — declaring only a discovery budget is ACCEPTED, so do not
over-constrain to a fixed file list you have not yet found.

### Inputs

- Static inputs: the plan/spec for the capability plus any named seam file the
  plan cites (e.g. `crates/rsi/src/action_handler/mod.rs`).
- Discovery budget: `rg`/glob to LOCATE the existing seam the feature plugs
  into — the seam is found, not assumed, before any new abstraction is carved.

### Process

- Find the seam, add the capability at the smallest matching convention
  (keybinding / RPC method / config key), add a test asserting the new
  behavior.

### Outputs

- New code plus tests plus the smallest doc update needed for discoverability,
  plus `VERIFICATION_ITEMS:` in the handoff body.

### Verify

- The new capability is reachable from at least one user-facing path and a
  test asserts it would fail on the pre-feature code; no existing test
  regresses.

## Typical failure modes (feature-specific)

- Over-abstraction: introducing a trait or generic when one concrete
  implementation exists. Concrete first; generalize only on the second
  caller.
- Hidden capability: feature lands but no user-reachable path exposes it.
  If the user cannot invoke it via existing paths, the feature is not done.
- Test gap: the feature ships without an assertion that the new capability
  actually works end-to-end.

## Success criteria

- New capability is reachable from at least one user-facing path.
- A test asserts the new behavior and would fail on the pre-feature code.
- No existing tests regress.
