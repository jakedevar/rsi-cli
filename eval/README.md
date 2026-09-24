# RSI-006 Eval Corpus

This directory holds the frozen 10-ticket corpus that `rsi-eval` replays
against candidate harnesses to gate harness mutations on metric regression.

## Directory layout

```
eval/
  corpus/
    <ticket-id>/
      ticket.md          # Ticket body (markdown). Required.
      prompt.txt         # Verbatim user query the daemon receives. Required.
      system_prompt.txt  # Optional override for worker_preamble snapshot.
                          # If absent, the loader falls back to
                          # eval/system_prompt_default.txt (or a built-in
                          # default if that is also absent).
      expected.json      # A-priori expectation row. Required.
  baselines/
    <git_sha>.json       # Per-harness baseline snapshots. Written by
                          # `rsi-eval --capture-baseline`. Diffable JSON.
  README.md              # This file.
```

## `expected.json` schema

```json
{
  "kind": "Bug | Feature | Refactor | Research | Standard",
  "expected_completion_status": "Completed | Failed | Interrupted",
  "expected_test_passed": true | false | null,
  "expected_clippy_passed": true | false | null,
  "expected_partial": true | false
}
```

Fields:
- `kind` — `SessionKind` discriminant. Drives the kind-specific
  `worker_preamble_*.md` variant the daemon would normally inject (the
  hermetic harness bypasses the variant in favor of the corpus's own
  `system_prompt.txt`).
- `expected_completion_status` — terminal `SessionStatus` the corpus
  designer expects this ticket to land in.
- `expected_test_passed` / `expected_clippy_passed` — `null` means "this
  session is not expected to invoke the probe" (e.g., a research ticket
  that never touches `cargo test`). The metrics collector treats `null`
  as "do not penalize for missing measurement"; `false` counts as a
  measured failure.
- `expected_partial` — when `true`, the metrics collector excludes the
  ticket from the strict `completion_rate` aggregate and instead reports
  `asked_clarification_rate` as a separate dimension. Used for the
  ambiguous-spec ticket where the desired behavior is "ask, don't guess".

## Frozen content discipline

Once a ticket is fully populated and committed, its body must NOT be
modified between baseline runs. Mutations to corpus content invalidate
all baseline comparisons. To replace a corpus ticket, file a follow-up
ticket; do not edit in place.

## Loader contract (consumed by `crates/rsi-eval`)

```rust
pub struct CorpusTicket {
    pub id: String,                    // directory name
    pub kind: SessionKind,             // from expected.json
    pub prompt: String,                // verbatim prompt.txt body
    pub system_prompt: Option<String>, // verbatim system_prompt.txt if present
    pub expected: CorpusExpected,      // parsed expected.json
}

pub fn load_corpus(name: &str) -> Result<Vec<CorpusTicket>>;
```

The loader walks `eval/corpus/<name>/` (or `eval/corpus/` when `name ==
"default"`), reads each subdirectory in alphabetical order, parses
`expected.json`, reads `prompt.txt`, and optionally reads
`system_prompt.txt`. Errors fail-closed: a single missing file aborts the
entire run with exit 1.

## Validating the corpus

```bash
./scripts/eval-corpus-validate.sh   # checks shape of all 10 directories
```

Runs `jq .` over every `expected.json` and asserts every required file
exists. Run this in CI once `rsi-eval` lands.

## Live reproducibility check

```bash
RSI_DAEMON_SOCKET_PATH=/path/to/isolated/daemon.sock \
  ./scripts/eval-reproducibility-check.sh
```

This is the Phase 5 daemon-autonomous helper from the RSI-006 manifest.
It runs `rsi-eval` three times against the same isolated daemon, captures
artifacts in a temp dir, computes `(max - min) / median` for every
aggregate metric, and fails if any spread exceeds 5%. Pass additional
`rsi-eval` flags after the script name if you need a different timeout or
corpus during manual verification.

## Ticket-shape distribution (10 tickets)

- 3 research-shape (`SessionKind::Research`):
  - `research-001-codebase-survey`
  - `research-002-feature-comparison`
  - `research-003-bug-trace`
- 2 plan-shape (`SessionKind::Standard`):
  - `plan-001-migration-style`
  - `plan-002-tui-surface`
- 5 implement-shape:
  - `impl-001-trivial-bugfix` (Bug — fully populated representative)
  - `impl-002-event-parsing` (Feature)
  - `impl-003-tui-keybinding` (Feature)
  - `impl-004-migration` (Feature)
  - `impl-005-ambiguous-spec` (Standard, expected_partial=true)
