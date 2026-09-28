# Codegraph risk register

Status: proposed, pre-implementation

Scales: likelihood and impact are `Low`, `Medium`, `High`, or `Critical`

## Stage contract

### Inputs

- Findings `F-003` through `F-027`.
- Proposed storage, schema, manager/worker, query, tool, TUI, federation, and
  benchmark contracts.
- Failure patterns observed in pinned and post-pin Graphify history.
- RSI database, worktree, daemon, memory, agent-authorization, and TUI
  invariants.

### Process

- Identify correctness, security, reliability, performance, UX, dependency,
  and product-value failure modes.
- Assign prevention, detection, recovery, and an owning planned slice.
- Treat unresolved high-impact risks as implementation gates.

### Outputs

- Prioritized risk table.
- Cross-cutting release gates and escalation conditions.

### Verify

- Every High/Critical impact risk has prevention, detection, and recovery.
- Security/isolation failures are zero-tolerance, not accepted residual risk.
- Deferred capabilities remain out of MVP until their stated gate passes.

## Register

| ID | Risk | Likelihood | Impact | Prevention | Detection | Recovery / owner |
|---|---|---:|---:|---|---|---|
| R-001 | Syntax-only extraction overstates call/import resolution | High | High | Emit unresolved/ambiguous nodes; deterministic resolver rules; never infer by label | Golden precision/ambiguity tests; compare semantic spike | Correct facts in S3/S4; optional RA/SCIP S10 |
| R-002 | Changed/deleted files leave stale current edges | Medium | Critical | Contribution ownership, one transaction, current-snapshot invariants | Stale-fact invariant query; deletion/change tests | Roll back to last complete; full reconcile; S2/S3 |
| R-003 | Absolute paths or offsets leak into stable IDs | Medium | High | Repository namespace + canonical logical identity contract | Determinism across copied roots/worktrees | Identity namespace bump/mapping; S1 |
| R-004 | Same labels are silently merged or first-match selected | High | High | Typed IDs, candidate resolution, ambiguity error | Same-name fixtures; explain/search tests | Return candidates; rebuild affected identity rules; S3/S4 |
| R-005 | Parallel edges/evidence are destructively deduplicated | Medium | High | Edge occurrence keys and one-to-many evidence tables | Parallel-call/evidence fixtures | Reindex after schema/rule fix; S1/S3 |
| R-006 | Direction is reversed or lost during query conversion | Medium | High | Directional enums, directed default, mapping tests | Incoming/outgoing/path golden cases | Fail query/version profile; S4 |
| R-007 | Dirty worktree query serves canonical or sibling state | Medium | Critical | Effective-workspace IDs bound server-side; per-workspace DB | Conflicting-worktree isolation suite | Deny/quarantine handle; re-resolve workspace; S1/S5 |
| R-008 | Federation link reveals another project's knowledge | Medium | Critical | Mandatory project/workspace scope on link; authorize both endpoints | Adversarial cross-project pivot tests | Fail closed, audit, invalidate link; S8 |
| R-009 | Native tool accepts spoofed scope or database path | Low | Critical | Capture scope at construction; schema omits fields | Tool-schema snapshots and malicious argument tests | Remove tool/deny call; S5 |
| R-010 | Future CLI access expands generic agent RPC authority | Medium | Critical | Purpose-built read-only allowlist verb; token-bound caller | Authz matrix, discovery-surface snapshot, fuzzed params | Disable capability; separate later S11 |
| R-011 | Watcher storm consumes unbounded memory/work | High | High | Debounce, fixed queue, bounded changed set, coalesced full reconcile | Queue/health/load metrics and event-storm test | Cancel/coalesce/reconcile; S2 |
| R-012 | Parser/Cargo job ignores cancellation | Medium | High | Cancellation token, subprocess kill deadline, bounded pool | Cancellation latency test | Kill worker/process; last snapshot serves; S2 |
| R-013 | SQLite writer blocks daemon lifecycle traffic | Low | High | Separate DB, one writer, bounded batches, WAL/read pool | RPC latency and busy-time metrics | Backpressure/cancel/retry; S2 |
| R-014 | Derived DB corruption blocks sessions | Low | High | Separate failure domain, integrity metadata, last-good/rebuild path | Startup integrity checks, fault injection | Quarantine/delete derived DB and rebuild; S2 |
| R-015 | Main `rsi.db` federation migration damages core state | Low | Critical | Normal versioned migration, narrow tables, backup/rollback tests | Migration fixtures across supported versions | Stop rollout/restore; S8 |
| R-016 | Per-worktree DBs consume excessive disk | Medium | High | Quotas, retention, inactive cleanup, measured history | Status disk metrics and many-sandbox test | Prune derived history/DB per policy; S2/S7 |
| R-017 | Historical snapshots grow query/index cost | Medium | Medium | Bounded retention, current indexes, immutable pruning | Size/latency over synthetic history | Prune per approved policy; S7 |
| R-018 | Cargo metadata is slow or fails during edits | High | Medium | Hash cache, subprocess timeout, last-good component, clear degraded health | Manifest edit/load benchmarks | Serve labelled stale Cargo facts; retry after repair; S3 |
| R-019 | Tree-sitter grammar upgrade churns facts/IDs | Medium | High | Producer/rule/version hashes; stable identity contract; rebuild/diff gate | Cross-version fixture comparison | Namespace/mapping or full rebuild; S3 |
| R-020 | rust-analyzer/SCIP becomes hard runtime dependency | Medium | High | Optional importer boundary and feature gate | MVP runs without binaries; dependency audit | Disable augmentation; syntax graph remains; S10 |
| R-021 | Whole-graph loads exceed memory | Medium | High | SQL admission and bounded induced snapshot only | RSS/load test and allocation metrics | Truncate/timeout; tune server caps; S4 |
| R-022 | Recursive SQL traverses explosively | Medium | High | Depth/row/time limits, relation filters, query plans | Malicious dense graph tests | Interrupt query, return bounded status; S4 |
| R-023 | Token renderer exceeds declared budget | Medium | Medium | Central budget-aware serializer, reserve metadata budget | Property tests across Unicode/large evidence | Truncate earlier with exact reason; S4/S5 |
| R-024 | Truncation is misreported as no relation/path | Medium | High | Result state distinguishes complete/partial/unknown | Boundary path fixtures | Return `truncated_unknown`; S4 |
| R-025 | FTS/vector similarity becomes answer evidence | Medium | High | Candidate-seed-only API and provenance rules | Response/schema tests; review query plans | Strip unsupported claim, fix adapter; S4/S9 |
| R-026 | LLM-derived link contaminates structural graph | Medium | Critical | Separate DB/table enums and producer constraints | Integrity query forbids LLM structural producer | Delete invalid derived facts, rebuild; S8/S9 |
| R-027 | Node summaries become stale authoritative context | High | High | Derived table, input hashes, model/prompt versions, default off | Invalidation tests and response labels | Invalidate/regenerate; S9 |
| R-028 | Community detection is nondeterministic/misleading | Medium | Medium | Defer MVP; fixed seeds/order/version if adopted | Repeated-build quality/determinism benchmark | Drop/recompute derived annotations; S9 |
| R-029 | Multi-repo identity/authorization merges unrelated nodes | High | Critical | Defer; repository namespaces and explicit authorized set | Collision/pivot fixtures | Disable global query, rebuild projections; S12 |
| R-030 | Derived memory chunk IDs disappear after reindex | High | Medium | Link target fingerprint and `TargetMissing` state | Memory reindex federation test | Do not retarget by similarity; manual/recomputed link; S8 |
| R-031 | Generalizing Dreamer `source_ids` breaks lineage | Medium | High | Preserve field semantics; separate ledger | Existing observation/Dreamer tests | Reject migration; S8 |
| R-032 | TUI displays stale snapshot as current | Medium | High | Snapshot/health in header and every response; generation IDs | Pending-update UI tests | Force refresh/show degraded state; S6 |
| R-033 | Async TUI response overwrites newer selection | Medium | Medium | Request generation/selection IDs | Delayed response tests | Drop stale response; S6 |
| R-034 | Clickable source opens canonical root, not sandbox | Medium | Critical | Resolve relative location against bound effective root | Conflicting-worktree open-location test | Deny if root/version mismatch; S6 |
| R-035 | Indexing hurts interactive daemon/TUI latency | Medium | High | Bounded concurrency, priorities, backpressure, cancellation | concurrent session/index benchmark | Pause/throttle worker; S2/S7 |
| R-036 | Benchmarks favor graph by excluding build cost | Medium | Medium | Report build and amortized/on-demand costs separately | Benchmark artifact review | Re-run standardized protocol; S0/S7 |
| R-037 | Benchmark mutates or exposes live worktree | Low | Critical | Mandatory temporary copy and path guard | Test harness asserts temp root/rejects live root | Abort run, inspect/restore through git; S0 |
| R-038 | Pinned/later Graphify capabilities are conflated | Medium | Medium | Exact commit in every baseline result; later appendix | Artifact validation | Correct report/re-run pin; S0 |
| R-039 | Graphify license/implementation is copied without notices | Low | High | Reimplement from design/primary sources; track provenance; license review for copied material | Dependency/source review | Remove/replace copied code; S0/S3 |
| R-040 | Unsupported languages/files are silently omitted | Medium | Medium | Health/index inventory reports skipped/error reasons | Corpus coverage report | Add extractor later or show unsupported; S3 |
| R-041 | Symlink/path traversal escapes workspace | Medium | Critical | Canonical containment checks, ignore policy, no absolute API paths | malicious symlink fixtures | Skip/deny and report; S2/S3 |
| R-042 | Secrets/source excerpts leak through events/logs | Medium | Critical | IDs/counts only in events; metadata-only audit; bounded evidence on authorized query | log/event snapshots and secret fixture | redact, rotate logs, disable audit payload; S2/S5 |
| R-043 | Stable node identity cannot survive valid Rust constructs | High | Medium | Versioned identity rules, explicit uncertain supersession | macro/local/anonymous/generic fixture suite | Namespace version/rebuild; S3 |
| R-044 | Snapshot retention deletes versions needed by durable links | Medium | High | Link-aware retention policy or unavailable historical state | prune/link integration test | retain pin or mark unavailable without retargeting; S7/S8 |
| R-045 | No measurable agent benefit despite complexity | Medium | High | Small dogfood slice, D2-style gates, on-demand default | correctness-per-token and task-time benchmarks | Stop/defer program after S7; do not expand |

## Top release blockers

The following are zero-tolerance before any dogfood-enabled release:

1. R-002 stale current facts.
2. R-007 worktree leakage.
3. R-008/R-009 scope or federation leakage.
4. R-026 LLM contamination of structural truth.
5. R-034 opening the wrong worktree source.
6. R-037 live-worktree mutation by benchmark tooling.

## Cross-cutting gates

### Before structural foundation merges

- identity/path model reviewed;
- per-workspace authorization tests designed;
- temporary-copy benchmark guard exists;
- exact-span and parallel-evidence fixtures exist.

### Before native tools enable

- all query bounds enforced twice (manager and worker/serializer);
- native schemas prove no scope/write fields;
- cross-worktree/project denial suite passes;
- source/log/event redaction reviewed.

### Before federation

- main-store migration reviewed independently;
- Dreamer lineage unchanged;
- target-kind authorization matrix complete;
- candidate/accepted/default visibility rules tested;
- memory-chunk disappearance and historical link behavior tested.

### Before later enrichments

- rust-analyzer/SCIP, summaries, communities, auto-context, multi-repo, and broad
  export each pass their explicit expansion gate; none arrive transitively as
  an “easy extra.”

## Residual-risk policy

- Security and cross-project/worktree leakage: no accepted residual risk.
- Stale structural facts presented as current: no accepted residual risk.
- Honest unresolved/ambiguous facts: acceptable and expected.
- Degraded last-good manifest/semantic facts: acceptable only when labelled
  with health and snapshot metadata.
- Bounded/truncated query: acceptable only when partial/unknown semantics are
  explicit.
- Missing communities/summaries/export/multi-repo: intentionally acceptable in
  MVP.
