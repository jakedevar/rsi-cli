# V82 mailbox fingerprint — attribution by experiment

Slice: Issue #21 Phase 2, **P2-06d** (the single batched V82 slice).
Base: `4b1e8393` on `rolling`.

`agent_coordination::v81_schema_fingerprint` is the semantic catalog digest for
the V81-named mailbox object family. It absorbs each object's normalized
`sqlite_master.sql`, each table's ordered `table_info` and foreign-key shape,
the expected inventory counts, **and the LIVE `PRAGMA user_version`**.

## Why the usual two-way experiment is not expressible for V82

Prior slices attributed a digest move by reverting only the catalog hunks and
confirming the pinned-digest test passed again at the OLD digest. **That
procedure cannot be run for V82**, and reporting a two-way result would have
been false.

The reason is the `user_version` term. Every prior slice edited the V81 catalog
while `user_version` stayed at 81, so reverting the catalog hunks restored the
whole input. V82 also advances `user_version` 81 → 82, and that term alone moves
the digest with **zero** catalog edits. Reverting the catalog hunks therefore
lands on a *third* value, not the old pin.

The honest form of the same proof is a decomposition: measure the version term
and each catalog hunk independently.

## Procedure

Both amended DDL sites are emitted by a single builder parameterized by a bool
(`agent_message_delivery_attempts_ddl`, `agent_messages_v81_cas_coherence_ddl`),
so an isolated measurement is a pure flag flip with no hand-editing of SQL.

For each row below, the flags were set in `install_v82_amendments`, the crate
rebuilt, and the digest read from the V82 migration's own mismatch error while
the pin was held at a `sha256:000…0` sentinel. **No pin was ever adjusted to
match emitted output**: the sentinel guarantees every measurement run FAILS, and
the value was pinned only after the decomposition below explained the entire
move. Every non-catalog change in the slice stayed applied throughout.

The source file was restored from a sha256-verified copy afterwards
(`e93bbb9dd79720031c9a9a812f89ce0bd3dc7e08dbd8ebc94908902deebf94c2`, byte-equal
before and after).

## Measurements

| # | Catalog state | `user_version` | Digest |
|---|---|---|---|
| `D_A` | V81, unamended | 81 | `sha256:517b10f62ac51c4229e7129235cce3673e8e89bc73fb6d02e0ae651641759bf3` |
| `D_B` | V81, unamended | 82 | `sha256:d40ced131ef2ff53bb2964f49462d6db58ea98ace29a190324fa5485abf48894` |
| `D_C1` | V81 + backstop CHECK only | 82 | `sha256:555ac8e56e6677affaf6fd906fd642583725a55e99b08ac8476b9085a773d57a` |
| `D_C2` | V81 + sealed-uncertain guard only | 82 | `sha256:d5ccfbb3fe39d5a945447cfd05799781968a69968109d642116db88875ebff7b` |
| `D_D` | **V82 (both amendments)** | 82 | `sha256:1bd9560e1899f4cbaae987f447034cadb3e4bf682afd061df361e939523389d2` |

### Re-measurement after naming the backstop CHECK

The decomposition was run twice. The first pass left the backstop CHECK
unnamed, which made it unattributable in a refusal message (`SQLite` reports
only `CHECK constraint failed: <table>` for an unnamed table CHECK), so the
constraint was given the explicit name
`agent_message_attempts_v82_reconciler_no_effect_backstop` and the affected
rows re-measured.

`D_C1` moved `afc9f9bd…` -> `555ac8e5…` and `D_D` moved `5bea5feb…` ->
`1bd9560e…`, while **`D_B` and `D_C2` reproduced their first-pass values
byte-for-byte** (`d40ced13…`, `d5ccfbb3…`). That reproduction is itself
evidence: it proves the naming edit reached the attempts-table DDL and nothing
else. The table above reports the SECOND pass, which is the shipped state.

`D_D` is the value pinned as `AGENT_MESSAGE_V82_PINNED_FINGERPRINT`.

`D_A` is not a remembered value. It was re-measured at this slice's base and
again after the DDL-extraction refactor (commit `c359fd55`), where
`v81_catalog_fingerprint_is_pinned_and_rejects_the_rejected_digest` passed
unchanged — which is what proves the refactor is token-neutral rather than
merely believed to be.

## What the decomposition proves

- `D_A != D_B` — the `user_version` term alone moves the digest. This is the
  term that makes the two-way experiment impossible, and it is measured, not
  argued.
- `D_B != D_C1` — the backstop CHECK contributes.
- `D_B != D_C2` — the sealed-uncertain guard contributes.
- `D_C1 != D_C2 != D_D`, and all five values are distinct — the two amendments
  are independent and neither masks the other.

Three contributions, three measured deltas, one final digest. Nothing else
drifted into the input.

## Inventory

**RECOUNTED, not assumed.** The canonical inventory is unchanged at exactly
**6 tables / 18 indexes / 32 triggers**. A table-level CHECK is not a catalog
object, and `agent_messages_v81_cas_coherence` was dropped and recreated under
the same name rather than added. The recount is asserted against the live
catalog by `v82_catalog_inventory_is_unchanged_and_recounted`, which counts
`sqlite_master` by type over the V82 head rather than trusting the constant
array lengths.

## The V81 pin is still live code, not history

`AGENT_MESSAGE_V81_PINNED_FINGERPRINT` is unchanged and remains enforced: it is
the V82 block's exact-source preflight. It is therefore re-proved on every fresh
open (0 → 81 → 82) and on the operator's own database at the instant V82 runs.
A drift in the V81 catalog aborts V82 before its first DDL statement.
