# impl-001-trivial-bugfix

**Kind**: Bug
**Estimated effort**: ≤30 minutes

## Symptom

A small off-by-one fence-post bug in a corpus-frozen helper function:
the iterator slices the wrong end of an input slice, producing the
output rotated by one element.

## Acceptance criteria

- The fix is a single-line change inside the helper.
- `cargo test --workspace` reports no regressions.
- `cargo clippy --workspace -- -D warnings` is clean.

## Notes

This ticket is a corpus-frozen representative for the eval/replay harness.
Its purpose is to give the eval driver a deterministic "known-good"
replay target whose expected outcome (Completed + tests passing + clippy
passing) lets the regression gate detect harness mutations that break
the worker's ability to handle a trivial bug.

Do NOT modify this ticket between baseline runs. Mutations to corpus
content invalidate baseline comparisons.
