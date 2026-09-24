# Closure Kernel K1 contracts

K1 provides identity capture, structured terminal outcomes, independent-review
evidence admission, and the durable receipt/journal nucleus. It does not
integrate a source into a destination, run final gates, clean up sandboxes or
refs, or add a TUI surface. Those effects remain reserved for K2 and K3.

## Operator RPC surface

The daemon routes these unattributed operator methods:

| Method | Contract |
|---|---|
| `CreateClosureProgram` | `CreateClosureProgramRequestV1 -> CreateClosureProgramResultV1` |
| `UpdateClosureProgram` | `UpdateClosureProgramRequestV1 -> ClosureProgramMutationResultV1` |
| `LaunchClosureSource` | `LaunchClosureSourceRequestV1 -> LaunchClosureSourceResultV1` |
| `ListClosurePrograms` | `ListClosureProgramsRequestV1 -> ClosureProgramListV1` |
| `GetClosureProgram` | `GetClosureProgramRequestV1 -> ClosureProgramDetailV1` |
| `RecordClosureEvidence` | `RecordClosureEvidenceRequestV1 -> ClosureEvidenceResultV1` |

All six methods are absent from the agent-attributed write and read allowlists.
K2 finalization/promotion/gate/discard methods and K3 cleanup methods are not
routed, so operator or agent callers receive method-not-found for them.

Every successful operator write uses a UUID idempotency key plus a canonical
request fingerprint. An exact retry returns the stored typed result. Reusing
the same method/key with different request bytes returns
`idempotency_mismatch` and creates no new effect.

## Shared artifacts

`rsi_common::closure_kernel` owns validated full Git SHAs, fully qualified refs,
repository/source/evidence identities, policies, lifecycle states, request and
result types, refusal codes, receipts, and journals.

A Closure source must finish with a provider-authored response whose first
nonblank line is `PIPELINE HANDOFF — CLOSURE:`. It contains exactly one
single-line `closure_outcome_v1` strict JSON field and a canonical `## Stage
contract` with `Inputs`, `Process`, `Outputs`, and `Verify` subsections. The
envelope correlates the program, source, custody generation, root and tip
sessions, rotation depth, model invocation, and source base SHA. Its declared
outcome is `committed`, `no_change`, or `blocker`; terminal session status alone
never supplies an outcome.

Required review evidence consists of strict `ClosureReviewArtifactV1` JSON and
a verification Manifest V2. V2 adds `schema_version: 2` and a full
`source_head`; Closure admission requires that head to equal the sealed source
SHA. Generic manifest parsing remains compatible with legacy V1. The
`rsi-closure-evidence-validate` CLI calls the same shared parsers used before
daemon import. Its policy-digest argument is the durable `Sha256Digest` form,
exactly `sha256:<64 lowercase hex>`; raw hexadecimal is intentionally rejected.

The independent reviewer commits exactly these two regular files in distinct
sandbox custody, in a commit whose sole parent is the sealed source SHA:

- `thoughts/shared/reviews/closure/<program>/<source>-review-v1.json`
- `thoughts/shared/verification/closure/<program>/<source>-manifest-v2.md`

The reviewer uses `scripts/seal-closure-review-evidence.sh` for that boundary.
It binds the supplied IDs to `RSI_SESSION_ID` and
`RSI_MODEL_INVOCATION_ID`, invokes the actual same-parser validator, permits
only those two paths, advances only the evidence ref, and rechecks that the
source ref/worktree remained at the sealed SHA.

Admission checks the completed reviewer session and model invocation, provider
provenance, custody, clean evidence worktree/ref, exact two-path diff, committed
Git object bytes, embedded heads, policy digests, and the source ref/worktree
head before and after review. Successful K1 admission publishes only an
internal `eligible_k2` integration-queue row.

## V84 persistence

Schema V84 is additive to V83. It adds:

- `conversation_event_provenance`, written atomically with each typed event;
- program, source, source-session, output-validation, evidence, and internal
  integration-queue tables;
- destination leases, attempts, receipts, consumptions, final-gate and
  quarantine/settlement journals predeclared for K2;
- cleanup proof/action catalogs predeclared for K3;
- immutable operator replay and append-only Closure event journals.

The active destination claim is unique by repository identity and destination
ref only while held. Terminal ingestion has the non-null unique key
`(source_id, tip_session_id, model_invocation_id)`. Live completion and bounded
startup recovery call the same transaction; the loser of a race returns the
stored validation and cannot duplicate a state transition, event, evidence, or
queue row.

Provider assistant output and daemon/provider diagnostics have distinct closed
producer kinds. Closure selects only a non-empty provider-assistant event for
the exact persisted model invocation; it never guesses authorship from text.

## Availability boundary

K1 can capture and inspect identity/outcome/evidence, create the source and
initial staging identities, and make a source eligible for a future K2 worker.
It cannot advance staging for integration, mutate the destination ref, consume
a receipt, execute or recheck a final gate, quarantine a failed head, delete a
sandbox/ref, or navigate Closure in the TUI. No keybindings are added by K1.
