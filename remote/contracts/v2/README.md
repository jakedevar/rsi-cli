# RSI Remote API v2 fixtures

`fixtures/*.json` are the request/response pairs of the v2 contract
([`docs/rsi-remote/contracts-v2.md`](../../../docs/rsi-remote/contracts-v2.md)).
Each file is `{"name", "request", "response"}`; `request.headers` use symbolic
values (`<session cookie>`, `<csrf token>`, `<canonical_host>`) and
`response.body` is `null` for a bare status (the gateway sends a short
`text/plain` body naming only the status).

They are generated, not written: `crates/rsi-remote/src/managers.rs` runs every
case through the real gateway pipeline and fails when a file differs, is missing
or is not produced by any case.

```sh
# check
cargo test -p rsi-remote --lib -- --exact managers::tests::contract_fixtures_are_produced_by_the_code
# regenerate after an intended contract change, then review the diff
RSI_REGEN_CONTRACT_FIXTURES=1 cargo test -p rsi-remote --lib -- --exact managers::tests::contract_fixtures_are_produced_by_the_code
```

The three `projects_*_200` bodies carry the top-level `messages_enabled`
boolean (the policy's `allow_messages`): `projects_with_global_200` and
`projects_empty_200` show it `false`, `projects_without_global_200` `true`.

The `dashboard_*` files are `GET /api/v2/dashboard` (hub #3287): a populated
body (an unreachable satellite with `null` build and count, a reachable one with
both, and an agent group with `effort: null`), an empty one, one past the
running-agent cap (`agents_incomplete: true`, `agents_total` 1024), and the
`busy` retry.

The `providers_*` files are `GET /api/v2/providers` (hub #3796): a populated body (a
Claude row with two windows, spend and models, a Codex row with `stale_reason`), one
with no data (`windows_reason`, `spend_reason` and null blocks) and the `busy` retry.

The iOS app's decoding tests read these files.
