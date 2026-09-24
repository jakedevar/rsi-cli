# RSI Closure Review Evidence

Read this reference only when a slice carries Closure program/source identity.
It replaces ordinary review evidence custody; non-Closure review is unchanged.

1. Seal implementation commit as `sealed_source_sha`. Source ref and source
   worktree HEAD must both equal it.
2. Dispatch reviewer in separate evidence sandbox/branch allocated from exactly
   `sealed_source_sha`, never source custody. Pass `program_id`, `source_id`,
   `sealed_source_sha`, `reviewer_session_id`,
   `reviewer_model_invocation_id`, and persisted `review_policy_digest`.
3. Reviewer follows Closure exception in
   `.claude/commands/_shared/worker_preamble.md`: write strict review JSON and
   head-bound Manifest V2 at canonical paths, then run
   `scripts/seal-closure-review-evidence.sh`.
4. Validate final response with `rsi-contract-validate`, then run
   `rsi-closure-evidence-validate` with same IDs, sealed SHA, and policy digest.
   Evidence commit must have one parent equal to sealed SHA and exact two-path
   name-only diff.
5. Re-resolve source ref and worktree HEAD. Halt unless both remain exactly
   `sealed_source_sha` while evidence ref advances to reported commit.

Closure reviewer returns only:

```text
PIPELINE HANDOFF — REVIEW:
reviewer_session_id: <uuid>
reviewer_model_invocation_id: <uuid>
review_json_path: thoughts/shared/reviews/closure/<program_id>/<source_id>-review-v1.json
manifest_v2_path: thoughts/shared/verification/closure/<program_id>/<source_id>-manifest-v2.md
sealed_source_sha: <full SHA>
evidence_commit_sha: <full SHA>

## Stage contract
### Inputs
Static inputs: `<sealed_source_sha>`, `<program_id>`, `<source_id>`, `<review_policy_digest>`
### Process
Independent review in evidence custody.
### Outputs
Committed strict review JSON and Manifest V2.
### Verify
Same-parser validation and unchanged-source checks passed.
```
