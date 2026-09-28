# RSI codegraph design set

Status: proposed; research and design only

Date: 2026-07-30

Implementation authorization: not granted

This directory defines an RSI-native structural code knowledge graph inspired by
Graphify. It refines the existing D5 “Code-graph (Gable-like)” idea rather than
creating a competing initiative. Nothing in this design set changes production
code, schemas, dependencies, configuration, or runtime behavior.

The documents deliberately keep three domains separate:

1. **rsi-graph** is RSI's workflow and multi-agent orchestration DAG system.
2. **RSI memory** is episodic knowledge: transcripts, chunks, observations,
   Dreamer derivations, entity cards, embeddings, FTS, and dialectic retrieval.
3. **codegraph** is deterministic structural knowledge about one source
   workspace at one observed revision.

The federation connects these domains without merging them. Structural
AST/manifest facts stay deterministic. Links to observations, decisions,
research, plans, sessions, summaries, or memory chunks live in a separately
owned, evidence-labelled semantic/episodic layer.

## Documents

- [Comparative research](2026-07-30-comparative-research.md) and its
  [schema-v2 sidecar](2026-07-30-comparative-research.json)
- [North-star project specification](north-star-spec.md)
- [Typed schema proposal](schema-proposal.md)
- [RPC, event, native-tool, and TUI contracts](interfaces.md)
- [Storage and Rust-library decision records](decision-records.md)
- [Benchmark and golden-query specification](verification-benchmarks.md)
- [Risk register](risk-register.md)
- [Dependency-ordered slice map](slice-map.md)
- [Human decisions required before implementation](human-decisions.md)

The JSON sidecar is governed by the comparative research document's stage
contract. The repository's strict research schema rejects unknown top-level
fields, so duplicating Markdown stage-contract headings inside that machine
companion would make it invalid.

## Approval boundary

The recommended first implementation slice is documented but has not begun.
Implementation must wait for the decisions in
[human-decisions.md](human-decisions.md) and explicit human approval.

## Stage contract

### Inputs

- The repository at HEAD `33bb72a2a6c82d38fb0227d15f4a948cc256b315`.
- Existing D5 and ideas-intake artifacts under `thoughts/`.
- The implemented RSI memory architecture and current daemon/TUI code.
- Graphify commit `2fa6cd3d5548577f8c5f591b713f0bf80c1af183`.
- The named Rust and SQLite primary references listed in the decision records.

### Process

- Establish current truth before proposing a design.
- Compare the pinned upstream revision and record later upstream changes
  separately.
- Trace every planned slice to stable `F-###` findings.
- Keep deterministic structure, orchestration, and episodic knowledge distinct.

### Outputs

- A navigable, internally consistent research/specification/planning set.
- An explicit approval gate before any implementation.

### Verify

- Every Markdown artifact contains this four-part stage contract.
- The research JSON validates strictly against schema version 2.
- Every research finding is covered by at least one slice.
- The worktree diff contains documentation under this directory only.
