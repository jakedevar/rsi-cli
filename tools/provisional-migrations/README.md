# Provisional migration declarations

Add one uniquely named JSON file here in the same commit as a new SQLite
migration. The rolling lander uses it to assign the next version after fetching
the current `rolling` tip. The declaration is bound to the accepted source
commit; it is retained as historical evidence after landing.

```json
{
  "schema_version": 1,
  "version": 130,
  "files": [
    {
      "path": "crates/rsid-store/src/store/migrations/v130.rs",
      "path_template": "crates/rsid-store/src/store/migrations/v${VERSION}.rs",
      "source_blob": "sha256:<64 lowercase hex digits>",
      "sites": [
        {
          "anchor": "fn migrate_v130(",
          "replacement": "fn migrate_v${VERSION}(",
          "scope": "unit"
        },
        {
          "anchor": "if version < 130 {",
          "replacement": "if version < ${VERSION} {",
          "scope": "unit"
        }
      ]
    }
  ]
}
```

A migration is one new file, `crates/rsid-store/src/store/migrations/vNNN.rs`
(three-digit name). The schema head is the highest file, so there is no shared
constant or list to declare; `crates/rsid-store/build.rs` collects the files. The
declaration must name that file with a `v${VERSION}.rs` template and a `unit`
site on its `if version < N {` gate. Other pinned Store files that quote the
provisional number (helpers, `store/tests.rs` rewind constants) are declared as
below. A declaration written against the old inline `store/mod.rs` layout is
refused; port it to a `vNNN.rs` file first.

List every occurrence of the provisional number in each changed source or test
file. Each anchor must occur exactly once in its source file. Its replacement
must render byte for byte to the anchor when `${VERSION}` is set to the
provisional number. Use `scope: "unit"` for migration gates, helper names,
catalog markers, and fixture logic tied to that migration. Use `scope: "head"`
for the latest schema constant and tests that must track the final schema head
after all accepted units land. A versioned filename needs a matching
`path_template` containing `${VERSION}`. The source blob digest is SHA-256 of
the complete file bytes at the accepted source commit.

Every file entry carries exactly `path`, `path_template`, `source_blob` and
`sites` (use the path itself as the template when the name has no version), and
every site exactly `anchor`, `replacement` and `scope`. The pre-push guard
(`scripts/repo-guard released-migrations`) and the renumber tool check this shape
for every declaration a source adds, even when the source already sits at
`rolling` head + 1 and needs no renumbering, and name the declaration, the file
entry and the missing key (#3304).

A Store test assertion of the provisional schema head is a version site with
`scope: "head"`. The renumber path accepts declared Store helpers/test files only
when they are in the source's released-migration inventory. A new unpinned module
is refused with `migration helper or rewind source is not pinned` on that path,
even when the released guard accepts a deferred pin. Validation without a renumber
does not check this pinning restriction. To keep the source renumberable and the
pin lander-derived, put a shared version constant in already pinned
`crates/rsid-store/src/store/tests.rs`, reference it from the new module, and add
this entry to the `tests.rs` declaration's `sites` (with its `source_blob` digest
updated):

```json
{
  "anchor": "pub(super) const EXPECTED_SCHEMA_HEAD: i32 = 130;",
  "replacement": "pub(super) const EXPECTED_SCHEMA_HEAD: i32 = ${VERSION};",
  "scope": "head"
}
```

For example, the new module can assert
`assert_eq!(version, super::tests::EXPECTED_SCHEMA_HEAD);`. For a replay test tied
to this migration rather than the final head, use a migration-specific constant
and `scope: "unit"` instead.

The lander validates the source inventory, immutable released prefix, unique
site anchors and complete version occurrence coverage before making a private
transform. It recomputes the manifest and refuses semantic conflicts. Existing
migration code and fingerprints are never edited after landing.

Two migrations that land together both insert into `store/tests.rs` at the same place,
and the lander resolves that conflict without a hand renumber when each insertion is the
migration's own unit (#3343): its rewind teardown block (`if active >= N && target < N { ... }`,
masked whole, and ordered newest version first so a rewind tears down newest-first), its
run of whole `assert_fixture_matches_claimed_version` statements carrying its
`target_version >= N` gate, and the `#[...]` attribute lines directly above a declared site.
Each fixture query `let` must be followed by an assertion consuming that same identifier;
extra, unpaired or trailing queries are refused.
Any other changed line in the hunk (an extra statement after the block, a torn statement,
a gate on another version) is still refused `unproved migration conflict`.

When invoking `tools/rolling-migration-renumber` directly, pass full commit IDs
for `--base`, `--source` and `--target`. Symbolic refs and abbreviated IDs are
refused with the argument name and advice to use `git rev-parse <ref>`. For example:

```bash
tools/rolling-migration-renumber --repo . \
  --base "$(git merge-base origin/rolling HEAD)" \
  --source "$(git rev-parse HEAD)" --target "$(git rev-parse origin/rolling)"
```

The extensionless author commands run the Rust guard crate. The older Python
files are retained as source specifications until all queue, lander and trusted
runner callers converge; do not execute them. Caller migration must preserve
committed-tool identity and bounded execution as described in
[`python-free-migration-bootstrap.md`](../../docs/agents/python-free-migration-bootstrap.md).
For an explicit-base policy check use
`tools/check-released-migrations origin/rolling HEAD`.

## Refreshing digests after merging rolling

A merge can change unrelated text in a declared file, leaving its complete-file
`source_blob` stale even though every migration site still matches. After
`git fetch origin` and `git merge origin/rolling`, run from the worktree root:

```bash
tools/refresh-migration-declaration-digests tools/provisional-migrations/<your-declaration>.json --check
tools/refresh-migration-declaration-digests tools/provisional-migrations/<your-declaration>.json
git diff -- tools/provisional-migrations/<your-declaration>.json
```

`--check` validates without writing (exit 0 for current digests, 1 for stale
digests, 2 for refusal). The refresh validates every file and site before writing,
including entries whose digests are already current. Each anchor must still
occur exactly once in its original declared path, with a pure version
substitution and no overlapping sites. Missing, changed, duplicated or moved
anchors are refused; moving an anchor to another file does not satisfy its
declaration. Unrelated edits can shift an anchor's byte offset within its file.

Only stale `source_blob` string values change. Other entries, formatting,
`path`, `path_template`, and `sites` remain byte-identical. Review and commit the
digest update with the merged source, then submit the resulting SHA for landing.
If a site changed, reconcile the source and declaration deliberately before
retrying; the command never guesses replacement sites. Refresh only your active
provisional declaration, not retained historical declarations. This command
checks worktree sites; the lander still validates the committed source inventory,
version coverage and released prefix without relaxing its source binding.

## Numbering after an in-flight sibling (`pending_predecessors`)

A slice that follows a sibling migration which is reviewed but not yet on
`rolling`, and omits that sibling's migration from its own tree (for example
v156 while v155 is pending, #1285), names the numbers it skips in its declaration:

```json
{ "schema_version": 1, "version": 156, "pending_predecessors": [155], "files": [ ... ] }
```

`pending_predecessors` is optional: a distinct, ascending-or-not list of at most
8 integers, each above the base's schema head and below `version`. With it,
`scripts/repo-guard released-migrations` and `scripts/check-touched-shards`
(`numbering_conflict`) accept the branch's new files as the base head + 1 ...
`version` minus the declared numbers, and the rewind fixtures keep their
"highest step below N" lookup. The rolling lander refuses the source with
`pending predecessor` until `rolling` already carries the sibling (its schema head
is at least `version - 1`), then renumbers the slice to the head + 1 as usual.
Entries at or below the base head are ignored, so a retained declaration goes
inert once the sibling has landed. Without the field the old rule stands: the
new files must be exactly base head + 1.

## Preflight when the source contains an unlanded sibling

A merged source can contain both the sibling's complete migration and its own
migration. For example, with rolling at v154, a source containing v155 and v156
adds two migration units relative to rolling. An origin/rolling-based
`rolling-migration-renumber --transform` refuses that source with
`accepted source has more than one provisional migration`, even if
`released-migrations origin/rolling HEAD` passes its append-only check
(#545). A passing released-migration guard does not prove a single renumberable
unit. `pending_predecessors` describes absent numbers only: adding `[155]` when
v155 is present makes the append-only inventory proof fail.

Use this sequence for that stack:

1. The worker names the sibling dependency and this preflight limitation in the
   handoff; the PM lands the sibling through its own merge-queue gate first.
2. After the sibling is on rolling, the worker fetches and merges
   `origin/rolling`, preserving source ancestry and rolling's released migration.
   Reconcile the active declaration's sites and refresh its digests as described
   above. Keep historical declarations unchanged and never renumber by hand.
3. Run `tools/check-released-migrations origin/rolling HEAD`, then
   `tools/rolling-migration-renumber --transform` with full commit IDs: use
   `git merge-base origin/rolling HEAD` for `--base`, `git rev-parse HEAD` for
   `--source`, and `git rev-parse origin/rolling` for `--target`. The source must
   now contain only its own new migration relative to that base. Verify the final
   committed source with the worker contract's `scoped_test` before submitting it.

Using the unlanded sibling's head as both base and target is not a general
renumber-tool workaround: its deferred pin is rejected by the tool's strict
base/target inventory validation. The released-migration guard can validate a
declared provisional base, but that separate check does not establish transform
or landing readiness. Do not refresh a sibling's pin, drop declarations, or
relax digest binding or append-only proofs to bypass the refusal. An accepted
source with new migrations still introduces only one provisional migration unit
per landing.
