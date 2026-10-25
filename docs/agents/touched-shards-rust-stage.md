# Touched-shard derivation Rust stage (#426)

This is a partial port based on `66b1d435a267a0266771b731930191a6b032acc2`,
not a replacement gate or acceptance route. No canonical caller has switched.
The legacy program and tests were read as text; neither was executed. The legacy
suite declared 239 test methods at the foundation; the merged rolling suite now
declares 244. The Rust stage declares 116 intent tests, including 29 foundation
tests, without claiming one-to-one conversion or full parity.

## Callable interfaces

All modules are under `rsi_repo_guards::touched_shards`:

- `git::Repository`, `git::DiffContext::load`: commit-pinned blob batches, diff
  hunk ranges, one/multiple merge bases, integrated-base paths, merge cleanliness.
  Multiple-base clean merges diff the integrated tree against the base tip;
  conflict fallback intersects the tip delta with merge-base deltas (#1633/#2814).
- `source::SourceTree`: POSIX source/module paths, `#[path]`, `include!`, inline
  mounts, host-gated reachability, Cargo target roots/owners, lexical shard gates,
  sharded test counts, feature-test inventory, changed-module rank records.
  The planner retains its conservative subset of platform cfg syntax. #362 owns
  the shard audit and its richer cfg grammar; this stage does not port that helper.
- `map_shards(tree, affected, ordered_inventory)`: receives #362's authoritative
  shard/package inventory, with its order preserved; maps affected sharded files,
  package-aware fallback and the store-to-daemon compile dependency. Empty,
  duplicated, unknown-owner and source/manifest-inconsistent inventories refuse.
  `affected` must already include `selection::registration_sources` expansion.
- `filters`: named/anchored/direct filter grammar, 256-byte named chunks,
  syntactically proven same-shard containment, package-aware unsharded dedup,
  direct-observer distance ordering and nearest-identity counts. A single
  oversized name stays oversized for the final receipt to report, never truncates.
- `slow`: validated TOML, generated nextest filter, multiline test spans,
  fast-test-only exclusions, conservative triggers and pinned candidate decisions.
- `downstream`: pinned workspace manifest graph (including workspace, renamed,
  target-specific, dev and build path dependencies), public API classification,
  union of old/new graph edges and transitive reverse-dependency compile plan.
  These APIs produce pending compile coverage, never runtime execution evidence.

The S1 registry exposes `rsi-repo-guard check-touched-shards`. Its implemented
CLI modes are `--slow-tier-only` and `--write-slow-tier-filter`; it preserves
compile-mode contradiction refusals. Filter derivation/compile/run requests
return 2 with `incomplete_contract` on stderr and emit no JSON, receipt line or
receipt file. `--filters-only --json` is intentionally blocked, not represented
as a successful empty selection. Help and diagnostics are staged, not full
legacy CLI/output parity.

## Exact integration sequence

1. Finish the contracts named by `touched_shards::UNMET_CONTRACTS`: complete
   diff seed proofs and typed/private/leaf/RPC observer integration; aggregate
   trace budgets and direct observers; fixture/data/include/source/token
   readers; guard/coverage declarations; migration isolation, rewind/catalog and
   source pins; downstream exact runtime observers; e2e and desktop decisions;
   final budgets, receipts, outputs, refusals and compile/filter execution modes.
   Port remaining legacy test intents into Rust. The stage is not safe to wire
   into the queue or #423 until these contracts and tests are complete.
2. Integrate #362's shard-inventory API into the ordered `map_shards` argument.
   Keep helper work in #362; no scoped-test or cargo-slot edits belong here.
3. Assemble the complete Rust derivation result. Expose in-process equivalents
   of `SourceTree`, `shard_packages`, `feature_test_inventory`, `changed_modules`,
   `direct_observer_filters`, `downstream_test_plan`, and `slow_tier_decision` to
   #423. Preserve the complete legacy `--filters-only --json` receipt fields;
   `UNMET_CONTRACTS` must be resolved before that mode can return a usable result.
4. In the coherent caller switch, replace the legacy executable with a shell
   forwarder to `scripts/repo-guard check-touched-shards` and change #423's
   in-process imports to these Rust APIs. Update lander/QA consumers with #363.
   Preserve normal admission and the stamped target. Delete legacy code/tests
   only when all their intents and callers have a working replacement.
5. #423 must materialize a trusted Python-free tool snapshot and obtain a
   daemon-attested full receipt at the exact first-publication HEAD. A green
   native package test or self-authored receipt cannot provide that attestation.
   Use the exact stage base above for the full scoped/diff run when the supported
   Python-free runner exists; do not run the legacy runner, even for comparison.

## Verification boundaries

The 29 Rust tests cover the APIs above, including pinned dirty-worktree reads,
large Git pipe batches, criss-cross integrated-base exclusion, old/new dependency
union, fixture-string shard exclusions, host reachability, path-mounted module
ownership, observer wire limits/ranks, slow-tier boundaries and blocked partial
receipts. They do not establish full derivation, selector or bootstrap acceptance.
The full legacy suite, canonical hooks/runner/queue integration, Linux/Windows
execution and daemon exact-head first publication remain unrun dependencies.

## Continuation checkpoint (after rolling 497f3e6ae)

The allocated foundation was preserved and `origin/rolling` merged at
`d80405945147b513d5a3053ec76073115e003193`. A fresh `git log -S` search found no
landed Rust selector to reuse. The current legacy suite declares **244** methods
(after rolling changes), rather than the foundation's 239. It remains text-only
input, never an executable oracle.

New independently callable components:

- `selection::{registration_sources, changed_lines}` consumes the exact pinned
  old/new snapshots and zero-context Git hunks. It expands registration additions,
  removals, path mounts and public use/alias edits to same-package modules;
  unresolvable declarations or executable changes keep source coverage.
- `selection::{changed_test_observers, changed_test_filters}` keeps edited/added
  tests as priorities across attribute/body changes and all named shards. These
  priorities must survive ordinary coverage merging. Host-excluded tests and
  deletion-only ranges supply no nonexistent candidate test identity.
- `identity::{expand_use, SourceScopes, ItemReferences}` builds one reusable
  tree index for inline scopes, impl owners, explicit/glob imports, aliases,
  module/item reexports, lexical type counts and token frequencies. `callers`
  caches successful and unresolved exported-library identities. Reexport cycles,
  depth overflow, ambiguous suppliers, missing local modules, unknown receiver
  and trait-method calls retain a diagnostic and require broad coverage.
  Associated inherent calls accept type aliases and `Self`. Local bindings,
  module path segments, strings and comments do not supply item call edges.
- `trace::{Seeds, Bounds, trace_tokens}` walks **already-proven** generic seeds,
  preserves nearest direct-observer ranks and returns `tests: None` with the
  reason on function/test bounds, hubs or unresolved identity. Settled priorities
  never replace the broad fallback. `tests: Some([])` means the seeded walk
  settled with no runtime observer; it is distinct from an unresolved walk.

Thirty-one further Rust intent fixtures are in `continuation_tests.rs` (60 total
selector tests with the foundation). Verification results belong to the #426
handoff; a fixture count alone is not evidence that they passed. This remains
partial and the complete CLI continues to refuse every usable receipt shape.

`seeds::{trace_seeds, changed_seed_proof}` now classifies generic old/new hunks,
function attributes/bodies, large-production rare tokens, unit enums, explicit
module/reexport declarations and uniquely resolved common struct fields.
`type_owners`, `enum_item_lines`, `struct_field_owner`, `mechanical_types`,
`unique_private_field_owner` and `initializer_value` expose independent proofs.
Initializer proofs carry the explicit observer-word omission obligation and a
private-field ownership map. The old/new assembler gathers field-shaped changed
rows and proves each against the whole pinned candidate tree, subject to the
function-frequency bound. `seeds::observer_words` removes only proven mechanical
initializations from a per-trace word view; another mention in a read, write,
pattern, comment or literal keeps the word. A constructor type word is removed
only when all its mentions occur on those initializer rows. `trace::trace_proof`
applies the view without mutating the reusable index or its frequencies/caches.
Use that entry point for a hunk proof rather than passing only its seeds to the
generic walker. Unfamiliar declarations and broken lexical inputs refuse.
Token inventories retain literal predicate readers; qualified item-call events
still exclude strings/comments. Real function spans exclude quoted declarations
and braces, and unchanged large inline test helpers remain traceable.

Nine private-field intent fixtures live in `field_tests.rs`, including the
hub-type constructor/reader split, fail-closed ownership, source scanners and
view isolation. Verification evidence belongs to the Issue handoff.

`relocation::{private_move, relocation_lines}` recognizes closed private inline
modules containing former parent structs, then aligns normalized old/new lines
with longest contiguous matches and earliest old/new ties. Only moved item and
private-region indentation is ignored. Alignment has a comparison bound; an
exhausted proof refuses instead of returning a partial diff. The hunk assembler
uses a 2,000,000-comparison limit. Imports must preserve parent bindings, the
exact reexport seam may be ignored, and unrelated declarations remain unproved.
Moved type names must be unique across the candidate tree. `Proof.moved` carries
the concrete receiver obligation into `trace_proof`, which follows cfg(test)
methods via those types and fixture helpers rather than every identically named
method. Ambiguous test-method owners refuse. Multiline literal contents retain
their exact whitespace during alignment. Twelve fixtures in
`relocation_tests.rs` cover these contracts and bounded fallback. Callers still
choose the legacy 60-test bound for relocation (24 for ordinary traces); the
explicit `Bounds` is never silently raised.

`rpc::{dispatch_seeds, dispatch_observers, changed_dispatch_trace}` stages simple
literal and qualified-family method registrations. Old/new arms are proved
separately; dispatcher preludes, unknown arms, side effects and unproved family
predicates require broad coverage. The qualified helper walk excludes the
dispatcher and generic predicate, resolves shared exported helpers through the
item index, and preserves direct ranks when a bound or identity stops it.
`changed_dispatch_trace` returns `Ok(None)` for ordinary non-dispatcher edits,
`Ok(Some(trace))` for the specialization and `Err` for a required broad fallback.

`readers::{code_includes, source_scanner_tests}` supplies lexical include_str!/
include_bytes! source-read edges. Quoted examples and comments are excluded;
named constants lead to their readers, direct function includes name that
function, and anonymous file includes retain the module's tests. The scanner
follows large helpers and same-module/unique-name helper closures. RPC traces
union scanner tests after their runtime bounds, preserving the separate source
read obligation. Fifteen RPC/reader intent fixtures are in `rpc_tests.rs`.
The unresolved-identity fixture uses an unknown qualified free-function path;
an unrelated receiver method is a distinct item and does not cause refusal.

`readers::include_item_tests` stages data files under a crate's src/. Every file
must have a named const/static/function include owner in the traced libraries.
Internal helpers close transitively; external callers stop after one layer and
must name at most 12 functions. Tests are bounded at 60 unique identities. A
missing owner, anonymous include, reader outside the traced libraries, another
literal reader or bound overflow returns `Err`, preserving the wider caller
selection. Eight fixtures are in `reader_tests.rs`.
Other reader contracts (data/assets, fixtures, scanned
tokens and source-reader pins) remain unfinished.

`SourceTree::trace_module` prefixes leaf modules with their crate name, including
crate roots. The item inventory now retains leaf function words/frequencies,
while exported-library identity resolution stays scoped to the sharded
libraries. `Proof.leaf` allows long wrappers during the bounded observer walk;
an ambiguous shared exported leaf function or a leaf module registration
requires broad fallback.

`leaf::daemon_observers` stages retained facade notes for remote-read/harness
and exact-only planning observers. Test-only and comment-only changes add no
daemon facades. An unresolved trace sets `unfiltered`: final assembly MUST
withdraw all gate filters if any leaf path is unfiltered, while preserving its
own package test selections in worker_filters. Facade substring collisions are
anchored. FacadeSelection retains actual selected test counts and broad flags
for final receipt/worker budgets; those records are an integration obligation,
not a waiver. Twelve fixtures in `leaf_tests.rs` cover private/deep/large wrapper
chains, facade scope, ambiguity, package roots, test/comment edits and bounds.

Remaining identity/trace work includes complete receiver/type ownership
and final pinned-tree assembly of these proofs. Do not
wire the generic seeded walk as a substitute for those proofs. Then continue
readers, declarations, migrations, downstream runtime observers and full CLI
contracts from the integration sequence above. Reuse #361's migration policy
APIs rather than duplicating released-policy/declaration/renumber helpers.

PM desktop ruling: rolling removed the lander desktop selection, warm target
and scoped-tool snapshot floor intentionally; preserve those removals. Current
scoped-test still retains its desktop selector/executor and both npm and locked
Cargo test obligations. Follow each retained caller; no gate is waived. #423
owns the runner, #363 owns QA/lander, #362 owns helper/inventory APIs, and hub
#3621 owns daemon trusted bootstrap/materialization/Linux acceptance.

## Assembled derivation checkpoint (#426 continuation 7f04fd80)

The staged derivation is now assembled end to end. It still has **no canonical
caller**: the CLI runs it only with `RSI_TOUCHED_SHARDS_STAGED=1`, and otherwise
keeps refusing with `incomplete_contract`. Do not flip callers or delete the
legacy script until the exact-head, daemon-attested acceptance exists (#423/#3621).

Sources merged rather than copied (ancestry preserved): #362's authoritative
ordered inventory (`rsid_test_shards::{Shard, read_shard_packages}`; `map_shards`
now takes `&[Shard]`), #361's `released_migrations::parse_pending_predecessors`
and `check`, and #363's `desktop_selection` (`lane_selected_with`,
`trigger_inputs`, `static_inputs`).

New modules under `touched_shards`:

- `derive::{Loaded, Options, derive}`: every Git read happens once in
  `Loaded::load[_with|_on]` (head/old crate sources, hunks, added lines, shard
  inventory). `derive` is the legacy `main` after argument handling and produces
  the full receipt in the legacy key order, the summary line, the verdict and the
  stderr lines. `Failure::{Refusal (exit 2), Fatal (exit 1), Interrupted}`.
  Declared-guard errors refuse without a partial receipt. Leaf paths with an
  unprovable trace withdraw **all** gate filters while `worker_filters` keeps them.
- `narrow::narrow_tests`: RPC registration arms, hunk seed proofs (relocation,
  initializers) and the bounded walk joined, with the 24/60-test caps, `tests:
  None` vs `Some([])`, and the nearest-observer `Direct` hints.
- `tree`, `data`, `fixture`, `guards`, `migration`, `diffseq`, `e2e`,
  `downstream_tests`, `modes`: SourceTree test counts and target queries;
  include/asset/data/source-scanner/scanned-token readers; the fail-closed
  synthetic path-list proof (`FixtureFlow`); declared guards, declared coverage and
  source-observer pins; rewind-only, additive and isolated-table migration proofs,
  rewind/catalog callers and the numbering audit; a `difflib`-equivalent sequence
  matcher; e2e surfaces; exact downstream runtime observers; audits, supervised
  compile (SIGTERM partial receipts, `CHECK_TOUCHED_SHARDS_CLEAN`), `--run-filters`
  under tmpfs, receipt file/line writers (ASCII-escaped, atomic).
- cfg: rolling's richer grammar (empty `any()`, compound platform gates, multiline
  attributes) is evaluated through `rsid_test_shards::cfg`, so the planner and the
  runtime inventory answer alike.

Documented deviations (all conservative or cosmetic):

- `isolated_table_migration` does not embed SQLite. A strict token proof stands in
  for the authorizer: it accepts only new-table `CREATE TABLE/INDEX/TRIGGER` DDL
  (indexes/triggers only on tables created earlier in the same batch, triggers that
  only `RAISE`); anything else retains every rewind caller.
- A missing shard manifest refuses (`shard package inventory is unavailable`) where
  the legacy planner fell back to a single `store-01` shard.
- `downstream_tests.selections` is ordered by path, not by the legacy mount BFS;
  `filters` (sorted, unique) is identical.
- Unknown ordering of `shards.map` shard keys is sorted (the legacy set order was
  hash-dependent).
- `rsi-test-impact` is invoked only outside `--filters-only`, as before.

Tests: the legacy suite's 244 method names are covered by Rust intents in
`derive_tests` (real temporary Git repositories), `unit_tests`, `cli_tests` and
the earlier component files; the few unported names are renames, a Python-only
memo-identity check, a Python function lookup, and the real-fixture sanity check
folded into its chain test. Verification evidence belongs to the #426 handoff.

Performance note: a real-repository smoke run (`--filters-only --json` over a
169-file, 58k-line diff, unoptimized debug binary) completes in about six minutes
and exits through the normal receipt path. The first profile found per-call
regex compilation and per-query test inventories dominating; `re::Re` now caches
compiled patterns (bounded) and `SourceTree` computes its test inventory and
shard counts once. A release build is expected to be roughly an order of
magnitude faster; the queue-facing time budget is part of #423's integration.
