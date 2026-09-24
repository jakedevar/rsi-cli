# Research-doc validation contract (`rsi-research-validate`)

This document describes the validation contract for `<doc>.json` research
sidecars (RSI-014) as enforced by `rsi-research-validate`. Source of truth:
`crates/rsi-common/src/research_schema/` (`schema.rs` shape, `rules.rs`
rule engine, `mod.rs` public surface, `error.rs` result types).

## Usage

```bash
rsi-research-validate <path> [--strict]
```

Exit codes:

- `0` — the JSON validates in the selected mode.
- `1` — I/O error (missing or unreadable file).
- `2` — the JSON is invalid (parse error, schema violation, or version
  mismatch), or a CLI usage error.

Stdout is pretty-printed `Validation` JSON (`{ valid, errors[], schema_version,
mode }`). Stderr carries one human-readable line per error:
`Field <field> failed rule <rule>: <message>`.

## Modes

The default mode is **lenient** (resume-time tolerance for the historical
corpus); `--strict` selects the **strict** write-time gate.

- **Lenient floor (both modes):** parse, version pin, `research_question`
  non-empty, `areas` non-empty.
- **Strict only (write-time hygiene):** the per-finding/per-question rules
  below. Legacy docs predate them; lenient mode never fires them.

## Rules

`rule` values reported in `Validation.errors[]`:

| Rule | Field | Fires when |
| --- | --- | --- |
| `Parse` | `<root>` | the document is not valid JSON / does not fit the typed schema |
| `VersionPin` | `version` | `version` is `< 1` or `> RESEARCH_SCHEMA_VERSION` (currently 2) |
| `Presence` | `research_question`, `areas`, `file_refs[i].path`, `findings[i].id` | a required value is missing/empty |
| `WordCap` | `findings[i].summary`, `open_questions[i].summary` | a summary exceeds the 25-word cap (strict only) |
| `FormatRegex` | `findings[i].file_ref` | `file_ref` is not `path:line` or `path:start-end` (strict only) |
| `RangeCheck` | `file_refs[i].lines` | `lines[0] > lines[1]` (strict only) |
| `UniqueID` | `findings[i].id` | two findings carry the same present `id` (strict only) |

## Strict v2 provenance IDs (`Finding.id`)

`Finding.id` is the optional v2 provenance join key referenced by downstream
plan `satisfies:` lines and verification coverage. Strict mode enforces the
following decisions on **present** ids only:

- **Missing (`id` absent)** — legal in every mode. v1 docs omit the key;
  deserializing to `None` keeps v1 output byte-stable and green under the v2
  pin (strict superset, zero forced migration).
- **Malformed (present but empty or whitespace-only)** — rejected under
  strict with `Presence` on `findings[i].id` ("id must be non-empty when
  present"). Lenient tolerates it, like every other strict-only hygiene rule.
- **Duplicate** — the first occurrence of an id wins; every later occurrence
  fails `UniqueID` on `findings[i].id` with a bounded message naming the id
  and the first index: `duplicate finding id "F-001" (first declared at
  findings[0])`. An echoed id is capped at 32 chars, so oversized input cannot
  bloat validator output.
- **Gating** — duplicate detection keys on presence, not on the declared
  `version`: a doc that carries duplicate ids is out of contract regardless of
  its version label. Genuine v1 docs never carry ids, so legacy output is
  never affected.
- **Matching** — duplicate detection is exact-string scoped: no normalization,
  no case folding (`F-001` and `f-001` are distinct).

The writer skill (`/research`) already mints monotonic `F-001`, `F-002`, …
provenance keys and must never reuse or renumber them across a revision; the
`UniqueID` rule makes that invariant machine-enforced for strict v2 runs.
