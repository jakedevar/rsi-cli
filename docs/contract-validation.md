# Contract validator CLI

`rsi-contract-validate` reads a worker handoff from stdin and emits JSON on
stdout. Build it with the repository's pinned Rust toolchain:

```sh
cargo build --offline -p rsi-common --bin rsi-contract-validate
rsi-contract-validate RSI-242 --strict-v2 --manifest path/to/manifest.md < handoff.md
```

Use the binary from your Cargo target directory when it is not installed on
`PATH`. Omitting `--strict-v2` selects the legacy handoff parser. Strict mode
also validates status and continuation/blocker fields; it requires a full
pipeline handoff parse.

## Cross-stage manifest admission

Coverage applies only after a successful full pipeline parse, to a
`PIPELINE HANDOFF — VERIFY:` handoff declaring at least one plan/research
linkage key through `satisfies:`, `covers:`, `linkage:`, or `requires:`.
Existing handoff field requirements and the stage-contract block check still
apply before this gate.

Resolve the manifest from `--manifest <path>` first, otherwise from the
handoff's `Manifest path:` (also accepted as `Manifest:`). Relative paths are
relative to the validator process's working directory. An explicit path always
wins: a bad explicit manifest never falls back to a good implicit one, and a
good explicit manifest overrides a bad implicit one.

| Handoff and invocation | Coverage behavior |
| --- | --- |
| Non-VERIFY, or VERIFY without linkage, in either mode | Dormant; the coverage gate does not read a manifest, even with `--manifest`. |
| Linked VERIFY with `--manifest`, in either mode | Require that explicit manifest to read and parse; failure exits 2. |
| Linked VERIFY with `--strict-v2`, without `--manifest` | Require the handoff's implicit manifest to read and parse; failure exits 2. Removing `--manifest` cannot bypass strict admission. |
| Linked VERIFY in legacy mode, without `--manifest` | Preserve unresolved, unreadable, or unparseable implicit-manifest dormancy: exit 0 with a visible `skipping coverage check` diagnostic on stderr. |
| Linked VERIFY with a readable, parseable selected manifest | Run existing coverage: all declared keys covered exits 0; any uncovered key exits 2. |

The full handoff parser can itself reject missing required fields before
coverage runs. Legacy manifest dormancy does not relax those field requirements.
`--first-line-only` checks the marker only; worker reports, closure modes, and
orchestration outcomes do not run this VERIFY coverage gate.

## Process result and output

Always inspect the final process exit status:

- **0**: applicable checks passed, or coverage was dormant as described above.
- **1**: argument error or stdin I/O error. A required manifest read failure is
  a contract violation and uses **2**, even though its cause is an I/O error.
- **2**: contract violation, including required manifest admission failure,
  malformed handoff/stage contract, or uncovered linkage.

Required manifest failures report the selected path, read/parse cause, and
requiring flag on stderr. The CLI preserves its existing stdout protocol: it
can emit the parsed handoff JSON before a later gate fails. Admission failures
leave that envelope intact; uncovered linkage additionally emits the serialized
`uncovered_linkage` error. Stdout can therefore contain more than one JSON value,
and a parsed handoff or its `Status: complete` field does not establish success.

## Separate execution-status limitation (Issue #247)

Manifest admission establishes that the required manifest can be read and
parsed before coverage. Existing coverage uses item linkage without certifying
each item's execution status. FAIL, PENDING, and statusless item success semantics
remain the separate Issue #247; passing this gate does not certify that every
linked verification item executed successfully.
