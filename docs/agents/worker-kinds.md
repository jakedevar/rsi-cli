# Kind-specific worker guidance

## Bug

Start with a deterministic reproducer, find the smallest owning code region, and verify neighbors. Write a regression check for the desired end state when the behavior warrants it. Avoid masking the cause or expanding a fix into unrelated refactoring.

## Feature

Locate the existing seam first. Add the behavior and a user-reachable path using existing conventions, then a focused check and the smallest useful discovery documentation. Add a new abstraction only when the existing one cannot host the capability.

## Refactor

Name the cost being reduced, enumerate affected call sites, preserve observable behavior, and record a measurable improvement such as fewer lines, dependencies, allocations, or indirections. Treat any behavior change as a separate scope decision.

## Research

State the question before investigating. Produce a self-contained artifact under `thoughts/shared/research/` with cited primary evidence, stable finding identifiers when the research schema requires them, and a supported conclusion. Keep inference classified so later planning cannot silently promote it to acceptance evidence.

All kinds use the shared return and verification references in `docs/agents/worker-contract.md` and `docs/agents/verification.md`.
