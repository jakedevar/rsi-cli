# RSI Remote V1 contracts

This is the inert I0a qualification slice of the accepted revision-3 Remote
design. The canonical Rust module is
[`remote_read.rs`](../../crates/rsi-common/src/remote_read.rs). It is compiled by
[`remote/contracts/v1/rust/Cargo.toml`](../../remote/contracts/v1/rust/Cargo.toml)
through its `[lib] path`, **but is not registered in the production common
crate**. The qualification package is an isolated workspace with its own lock.
There is no gateway, listener, native command, source read, application or
authentication effect from these files.

## Run the qualification

From the repository root, using Rust 1.94.1, Node v26.7.0 and npm 12.0.2:

```sh
CARGO_TARGET_DIR="$PWD/target/remote-contracts" scripts/remote/check-contracts.sh
scripts/check-identity-assertions.sh
git diff --check
```

The script checks generated files, installs the locked TypeScript 5.9.3 compiler
with lifecycle scripts disabled, runs strict typecheck/build/tests, runs the
locked Rust package tests, and compares **both runtimes' emitted values and
display hints** for every shared fixture. It preserves the session's existing
`CARGO_TARGET_DIR`. Temporary parity JSON is removed on exit. Individual commands:

```sh
cargo +1.94.1 test --locked --manifest-path remote/contracts/v1/rust/Cargo.toml
npm --prefix remote/contracts/v1 ci --ignore-scripts
npm --prefix remote/contracts/v1 run typecheck
npm --prefix remote/contracts/v1 run build
npm --prefix remote/contracts/v1 test
```

Run `rustfmt +1.94.1 --edition 2024 --check` on the changed Rust files explicitly;
do not use `cargo fmt` to scope formatting. The package pins serde 1.0.228,
serde_json 1.0.149, uuid 1.20.0 and chrono 0.4.43, matching the root lock's direct
versions. No workspace manifest or root lock was modified. Isolated compilation
is not a production common-crate, daemon or workspace integration pass.

## Schema and codec conventions

The canonical Rust declarations define the structural schema. `wire!` declares
closed objects with required fields, including required nullable fields;
`envelope!` expands the common fields directly into each response. `object_enum!`
requires map input for both adjacent and internal tags, including the `labels!`
expansions. The declaration generator rejects tagged enums that bypass this
macro. Legitimate collection arrays, required nullables and defaults retain
their field semantics. `closed!` and
`labels!` declare finite protocol states and observed source labels respectively.
The small generator reads these declarations and the discriminated Rust enums:

```sh
node remote/contracts/v1/generate.mjs
node remote/contracts/v1/generate.mjs --check
```

It produces the committed `src/generated.ts` runtime schemas and inferred TS
types. There is no second Rust DTO copy. The generator fails on an unfamiliar
enum or field syntax; changes to serde representation need a corresponding
generator update. Cross-field validation is independently implemented in Rust
and `src/semantic.ts`, so shape generation alone cannot establish parity.

Use Rust `decode(&[u8])` / `encode(&WireDocumentV1)` or TypeScript
`decode(string | Uint8Array)` / `encode(WireDocumentV1)` as the byte boundary.
Both producers revalidate constructed values. Raw serde deserialization and
individual generated TS schemas are building blocks, **not substitutes for the
complete byte-boundary codec**. The boundary rejects duplicate object keys,
positional arrays in place of objects (including empty and nested structs),
unknown fields at every depth (including empty union variants), malformed UTF-8
or lone surrogates, non-integer JSON number spellings, excess depth (32) or nodes
(32,768), and size/semantic violations. The request bound is 16 KiB of actual
UTF-8 input and canonical output; response bounds are 512 KiB, 64 KiB per item,
and each envelope's declared projection limits. These are wire admission checks,
not allocator/RSS measurements or an implementation of source allocation caps.
The session-list's named `project` is an item for `item_bytes` accounting and
its own name is subject to `name_bytes`, including selected empty projects.
Every non-`none` selected wrapper, including its ID, state, message and retained
display, must fit both `item_bytes` and `decision_text_bytes`. Decision text
counts all UTF-8 string values, including provenance and protocol labels;
serialized item bytes also count field names and JSON punctuation. The inner
present decision is checked too. `none` carries no selected item. These limits
reject an undersized projection declaration; they do not erase identity to fit it.

`WireDocumentV1` is a **codec/fixture selector**, not an HTTP/RPC route. Its
`{type,value}` wrapper selects a closed data type. For `request`, `value` is
`{method,params}`; for `response`, it is `{method,result}`. Future adapters must
select one of the six fixed methods and wrap its params/result for validation;
they must not expose this selector as a generic dispatch endpoint. Wrapper bytes
are conservatively included in qualification size limits. There is no arbitrary
JSON DTO field.

`ViewReadRequestV1` is the client-facing read union used by `BoundReadV1` (the
standalone fixture selector is `view_request`). Its project discovery params are
only limit/cursor; it rejects `project_ids`. This reconciles D3's hostname-only
discovery with D2's explicit configured source scope: the future gateway obtains
that scope from its own authenticated policy and constructs the separate daemon
`ReadRequestV1::RemoteListProjectsV1`. The other five read parameter types are
shared. No helper in this slice supplies or authorizes that configured scope.

| Method | Params | Result body |
|---|---|---|
| `RemoteGetInfoV1` | Empty object | `item` with protocol `1.0`, daemon boot UUID, ordered six required capabilities |
| `RemoteListProjectsV1` | Explicit configured `project_ids` (unique, at most 32), limit 25/50, cursor | `items: {id,name}[]`, ascending UUID |
| `RemoteListSessionsV1` | `project_id`, limit 50/100, cursor | Named `project`, ascending `items` of session summaries |
| `RemoteGetSessionV1` | `project_id`, `session_id` | `item` with summary, own detail title, query/model previews, sequence observations and pending coverage |
| `RemoteGetHistoryPageV1` | Project/session IDs, `window`, limit 25/50, cursor | Ordered `items`, project/session IDs, echoed window, head, interval and position/relocation/reset |
| `RemoteGetDecisionsV1` | Project/session IDs, limit 16/32, `mode`, cursor, optional selected ID | `items`, project/session IDs, mode and explicit exact-selected result |

All result bodies have top-level `version`, `daemon_epoch`, `observed_at`,
`next_cursor`, `complete`, `projection_limits`, `degraded`, and `coverage`.
Singular results use `item`; paged results use `items`. `ObservationV1` is a
reusable validation type, not a nested envelope field. A session summary retains
`own_title`, `parent_id` and `continued_from` independently. Detail supplies a
4-KiB own title alongside its 512-byte summary prefix. The named project remains
present when its session list is empty.

Request limits, cursors, decision mode/selected ID, newer `through`, project
selection's session, native binding, ACK foreground activity and preview
`observed_bytes` have only the explicitly declared defaults. Canonical emitted
JSON includes those defaults: limits 25/50/16, mode `attention`, activity `false`,
otherwise `null`. All other fields must be present; nullable does not mean
optional. Only `none` is legal in create-view. Capabilities are emitted in the
six-method table order. Unknown source enums use `{state:"unknown",label}`
(maximum 128 UTF-8 bytes), with the positive text hint `Unknown: <label>`;
they never acquire a known label through fallback. Known values use
`{state:"known",value}` and preserve source spelling, including provider names,
event kinds, `User`/`Assistant`, and legacy `Pending`/`Approved`/`Denied`.

All i64/u64 observations are canonical decimal strings, including event IDs,
byte counts, omitted counts, lower bounds, generations, revisions and stream
sequences. Event IDs must be positive i64; sequence remains the full signed i32
JSON integer range. Other JSON numbers are bounded u32s. Leading zeroes, plus
signs, `-0`, fractions and exponent spellings are rejected. UUIDs must be
lowercase hyphenated canonical strings. Timestamps require RFC3339's uppercase
`T`, nine fractional digits, valid calendar/time fields and `Z` or a numeric
offset. String caps measure UTF-8 bytes, not JS UTF-16 units.

## Windows, completeness and pending identity

Cursors are a closed `kind=projects|sessions|history|decisions` union with a
nonempty bounded opaque base64url-alphabet token (at most 2 KiB). The codec
checks the method discriminator; it neither authenticates nor interprets a MAC,
expiry or source position. History windows are exactly `latest`, `older{anchor}`,
`newer{anchor,through}`, `interval{lower_exclusive,upper_inclusive}`, or
`locate{event_id,old_key,offset}`. Intervals cannot be inverted or combined.
Keys order by signed `(sequence,id)` without JS number coercion. Locate offsets
are UTF-8 byte offsets, at most 8192; a returned relocation must identify the
same event at its new key and clamp the offset to a UTF-8 boundary in its text.
For a locate window, relocation must also match the requested event ID and old
key. `unchanged` requires that event at that old key among the returned items;
an absent or changed anchor needs valid relocation or an explicit reset. Both
the response decoder and exchange correlation enforce this rule.
Relocation/reset is explicit; no helper mutates a retained history.

Completeness describes an observed bounded interval, not an immutable snapshot.
Coverage names each source, its `complete|busy|limited|unavailable` state,
observation time/order, continuation and lossless lower bound. Complete pages
must have completed sources and agree with continuation presence. Incomplete
pages require degradation. Pending pages require all eight source coverage
entries; zero complete rows and incomplete empty rows are distinct fixtures.
Empty configured discovery may contain only the completed zero
`configured_projects` observation, without claiming a Store read. Filtered
empty session pages may successfully continue past examined candidates.
Failed project/session/history reads use typed errors; incomplete success is
reserved for detail/pending coverage. Coverage refusal reasons must agree with
the degradation labels, and an unavailable exact-selected reread is incomplete.
Nested pending coverage has unique source and observation-order entries.
A current nested refusal requires `complete=false` and its matching degradation;
detail also requires `attention.incomplete=true` and local inspection. Decision
rows and non-stale exact-selected cards are current observations for this check.
An explicitly `stale:true` selected card requires the `stale` degradation; its
retained source observations do not claim a current read and do not override a
successful current scan. Nullable native evidence remains independent of source
scan completeness. Different observation times are not collapsed into a snapshot.

Decision IDs remain kind-qualified: publication `question:<uuid>` or
`native:<uuid>`, legacy `legacy:<uuid>`, runtime slot
`question-slot:<session>:<decimal-generation-or-completed>:<tracked-or-session>`,
or durable slot `question-fallback:<session>`. Slot identity supplies the hint
`Occurrence unknown`; text equality does not confer occurrence identity.
Generic publication IDs require a `question_publications` witness; durable
fallback IDs require `durable_question_fallback`. They cannot absorb runtime
slots. An equal fully decoded durable mirror may share the publication's card:
retain both `question_publications` and `durable_question_fallback` observations,
the publication ID, and the complete ordered questions/headers/options and
multi-select flags. Grouping requires complete observations and details, zero
question/option/source omissions, and no disagreement or display alternative.
This also applies to an exact-selected present card. An inconsistent or partial
fallback keeps its own slot/projection and explicit disagreement or incomplete
state; a displayed prefix cannot prove equality. The future producer must
establish full source equality before grouping; the codecs check representability
and consistency, without reading or comparing a live durable source.
Runtime IDs require their named tracked/session primary source; any
present generation must equal the numeric generation in the ID. `completed`
does not invent a numeric generation, and null witnesses stay unknown. The
additional runtime mirror is representable only with complete, untruncated,
non-disagreeing observations and questions. Equality of original runtime data
must still be established by the future producer before combining mirrors;
the wire cannot establish that source fact. Native IDs permit native runtime,
publication and historical witnesses; legacy IDs require legacy provenance.
Publication, closure, delivery, native witness, resolution observed/persisted,
writer liveness/capacity, source coverage and display are independent fields.
Known generic states are `unresolved|published|cleared`; the first two require
open closure/inspection, and `cleared` requires closed closure. Known legacy
states are `Pending|Approved|Denied`: pending requires open closure/inspection,
and approved/denied require closed closure. Known native states are
`unresolved|published|enqueued|expired|superseded`. Native publication, closure
and delivery remain independent; no combination proves answer consumption.
Bounded unknown publication labels remain visible and receive no known mapping.
Historical source observations cannot assert a live writer. All cards have
`can_answer=false`; local action means inspection, not answer authority.

Display is `generic_questions`, `native_approval{method,description}`, or
`legacy_approval{tool_name}`. Fields retain `text,state,source_field,
source_extent,observed_bytes`. Method/tool name cap at 512 bytes; description at
2 KiB. All native fields are always `bounded_snapshot` with the hint
`Source-provided preview`; their observed bytes describe that snapshot.
Persistence into native publications or historical fallback, display alternatives,
and retention in a selected tombstone/unavailable result preserve that extent.
Eligible legacy tool-name fields retain `full_field` semantics.
`truncated` produces `Truncated preview`. Unavailable fields use their exact
positive named replacements, including `Native approval — method unavailable`
and `Legacy approval — tool name unavailable`. Bounded display alternatives
require disagreement. No params, target JSON, reply handle or synthetic
approval questions/options are accepted.

Generic questions retain up to eight ordered questions and eight options each,
headers, multi-select meaning and omitted counts. Their combined decision text,
including provenance and labels, is capped at 8 KiB. Unreadable questions retain
`Question details unavailable`; a pending snapshot supplies `Answer not
observed`, independently of ordinary saved user-answer events. Selected decisions
use `none|present|tombstone|unavailable`; stale labelled data and the selected ID
survive page movement, closure and disappearance. A tombstone retains its ID and
`Decision no longer available`, plus the last display when available.
`last_display` uses `RetainedDecisionDisplayV1`. Its generic variant contains
`questions`, `omitted_questions` and `details_state`, reusing the ordered bounded
question/option types, headers and `multi_select` flags. Complete projections
have no omitted questions/options and include at least one question; truncated
projections preserve omission counts. Unavailable details can remain empty with
an explicit unavailable state. Live summaries retain their existing separate
question array without duplicating it in `display`. Retained generic data fits
the same 8-KiB text bound and selected wrapper's declared caps; it supplies no
invented approval options or saved user answers.

History retains text/content-byte/truncation/offload states, event identity and
ordering. A complete nonempty tool key of at most 256 bytes is separate from the
display prefix. `exact` means the full key is available, **not that a matching
call/result was observed**; reused keys can be `ambiguous`. Oversized keys have
null pairing keys and bounded displays; shared prefixes cannot become keys.
Unmatched results remain ordinary visible events.

## Ephemeral bindings and errors

Create/allocate, attach/ACK, select/ACK, view-state, ready/selection-reset,
body-free notices, lease ACK/close, bound reads/responses and native
window/connection bindings are typed. Bindings include gateway/policy/view/cache
epochs and decimal selection/attachment generations. Initial allocation is
none/generation 1/attachment 0 with a 10-second lease; ready requires an attached
generation and a 25-second lease. Barriers carry at most eight unique role slots
and page keys.
None selection allows only `info|projects`; project-only selection allows only
`info|projects|sessions`. Ready, selection ACK and selection-reset carry the same
checked view state. Session selections retain the full declared role set. These
checks constrain wire barriers, not actual subscriptions or normalized page keys.
Revisions and entry incarnations are positive checked u64 strings;
revision exceeds the entry's initial stamp. Page keys are bounded opaque
normalized keys supplied by a future gateway; no codec derives routing from
their text. The generator also includes D5 nonce/session/CSRF response types;
fixture tokens are synthetic, and no credential is issued or persisted.

`validate_exchange`, `validate_bound_read`, `validate_bound_response` (TS camel
case equivalents) check typed method/parameter, ready/selection, binding,
request-ID, page-key and native-generation correlation. They perform no reads,
allocation, CAS, freshness lookup, authentication or authorization. A caller
must independently establish all D4/D5 ownership, policy, Origin, identity,
source-membership and delivery fences. Source cursor interpretation, authenticated
replay, actual CAS/idempotency, lease expiry and page-cache ownership remain
later implementations. Equal decoded fields are not evidence those gates ran.

Safe errors contain only a closed code, correlation UUID and bounded retry
action/delay (maximum 30 seconds), never raw diagnostics. D2 HTTP mappings are
preserved; wire-only view conflicts/selection-required use 409 and
selection-unavailable uses 404. Authentication errors remain 401/403.

## Shared fixture format and evidence limits

`fixtures/corpus.json` has `schema_version:1` and an ordered `cases` array. Each
case has a unique ID and expected `valid` boolean. It supplies either an `input`
document, literal `raw` JSON, hexadecimal `raw_hex` bytes, or a `base` case plus ordered `patches` with
`op:set|remove`, a path array and a set value. Positive `expected` defaults to the
authored input; explicitly authored normalization expectations cover defaults.
`visible` lists exact positive identity/text hints. Optional `against` and `view`
references exercise structural correlation helpers. Optional `source_tool_ids`
records the synthetic original oversized IDs; it is fixture metadata outside
the wire and makes no source-projection claim.

Regenerate/check the authored corpus with
`node remote/contracts/v1/fixtures/generate.mjs [--check]`. Neither generator
uses decoder output as expected data. Both runners independently materialize
patches, decode, assert the validity disposition, compare the entire emitted
JSON value with the authored expectation, verify round-trip equality and assert
positive hints. Parity then compares the two emitted result arrays, including
hint strings and negative dispositions. Hints are a diagnostic text projection,
not a browser/native/TUI rendering test. This is wire-contract evidence only.
The `r001-` through `r009-` fixture prefixes identify regressions for the nine
initial I0a source findings: object shape, locate correlation, native preview
extent, identity witnesses, publication mapping, nested coverage, auxiliary
limits, retained generic questions and project-only barrier roles. Original
positive identity/unknown-label/history fixtures remain in the same corpus.
The `c001-` controls pair every tagged-union variant with its positional-array
counterexample, including all known/unknown label families, nested selections,
history positions/windows, selected states and retained displays. Collection
arrays and nullable/default controls remain positive. The `c002-` cases retain
complete durable publication mirrors and separately represented inconsistent
fallbacks while rejecting partial grouping and missing/wrong identity witnesses.
Whole emitted values assert question order, option descriptions and boolean
multi-select meaning in addition to positive identity/provenance text hints.

The scoped joins are P04 (F-008/F-009/F-010/F-011/F-022/F-043/F-056), P10
(F-019/F-021/F-023/F-024/F-040), and P17 (F-052/F-053/F-055), only for wire types,
fixtures and locked qualification. Production source reads, authorization/CAS,
RPC catalogs, browser/native transport, actual allocator/RSS/admission, growing
DTO replacement, 10,001 retained inventory identities, history retention and
private RPC permit settlement are **deferred**. Runtime/device/resource/security,
accessibility, independent source review, integration and release gates remain
open. Other roadmap joins are deferred. This package is no proof of native,
browser, daemon, live Tailscale or application operation.

`tui-e2e: SKIPPED - not due: new inert wire-contract module and fixture package; no TUI source change.`
