# Harness value core

This unpublished crate holds pure conversation values, transformations, the pure tools
and the turn loop extracted from `rsid::session::harness` (#2611, #2616, #2648). The
daemon re-exports their old paths.

- `types`, `normalize`, `arguments`, `tools::*` (pure tools), `errors`, `retry`, `sse`,
  `bedrock_stream` (stream readers take any byte stream), `providers::*` (request and
  response bodies for Anthropic, Chat Completions and Responses), `compaction` and
  `agent_loop` (the turn loop).
- `ports` is the only way the loop reaches daemon authority: `ModelPort`/`Admission`
  (provider, credentials, model-call admission and settlement), `LoopTools` (tool registry,
  policy, process custody, completion gates, worktree digest), `MailBoundary`. The daemon
  implements them in `rsid::session::harness::loop_ports`; add a method there only when
  the loop needs a new fact, never a credential, URL or admission object.

- Depend only on third-party libraries or `rsi-common`, never `rsid`, `rsid-store`
  or `rsid-core`. Provider capabilities, credentials, policy admission, session
  ownership and process execution remain in the daemon.
- Preserve serialized conversation history, normalization order and argument
  repair/validation behavior. Move the associated tests with an implementation.
- Tests here are ordinary `#[test]`s; there are no daemon shard features.
- Production changes select daemon observers too: `LEAF_FACADES` in
  `scripts/check-touched-shards` and `LIB_PACKAGES` / `LEAF_PACKAGES` in
  `crates/rsid/src/bin/rsi-rolling-land/test_impact.rs` register this crate.
