# Remote read core (`rsid-remote-read`)

Bounded SQLite source reads, cursor signing and response projection for Remote V1,
moved out of `rsid` for issue #2410 so a Remote source or projection edit builds and
tests without the daemon's test harness. `rsid::remote_read` re-exports this crate's
public API at its old paths.

Facts that bite:

- The leaf takes a borrowed `rusqlite::Connection` and value observations. It never
  acquires a Store lock, authorizes a request, owns a transport permit or reads a
  runtime map. Those stay in `rsid` (`remote_read/{owner,permit,store_budget,rpc_owner,
  decision_targets}.rs`): admission, project authorization, blocking-worker scheduling,
  response-lifetime permits and the one `Arc<RemoteCursorSigner>` per daemon boot.
- Dependencies are `rsi-common` and third-party libraries only. Never add `rsid`,
  `rsid-store` or `rsid-core`: `crates/rsid/tests/store_crate_seal.rs` keeps the
  store/core authority sealed and fails on a new dependent.
- Export only what the daemon-side owner needs, one name in `lib.rs`'s `pub use`
  lists. Do not make a module `pub`. A new exported name is a `lib.rs` edit, which the
  lander treats as a crate-root change (every shard); keep helpers and tests in the
  modules, not in `lib.rs`.
- `RemoteCursorSigner` keeps its key private and zeroized on drop; the policy-scope
  digest is supplied by `rsid` on every operation. Wire bytes, TTL, size limits and SQL
  bounds are contracts: change them only with the Remote V1 contract.
- Tests here are ordinary `#[test]`s: this crate has no `test-shard-*` features, so
  `cargo test -p rsid-remote-read --lib` runs all of them. The owner/permit/RPC tests
  that need the Store or `SessionManager` stay in `rsid` (`remote_read::`).
- Selection: a production edit here also selects the `rsid` facade tests and any sharded
  test that names the change (`scripts/check-touched-shards` `LEAF_FACADES`, receipt
  `leaf_observers`; the lander's `test_impact::LEAF_PACKAGES`). A leaf filter alone is
  not evidence for a production change. The trace follows private leaf helpers through
  their public wrappers into `rsid`; when it cannot be proven the derivation emits no
  filter (receipt `unfiltered_leaf_paths`) so the lander's complete closure applies (#2441).
- Code here must build on Linux, macOS and Windows.
- Cold-storage archive (#2384): a cold session's bulky `conversation_events` rows are in an
  archive file, not the live table. The history and detail readers (`history_page`,
  `observed_history_page`, `session_detail_source`) call `require_session_project`, which refuses
  `Archived`/`Deleted` sessions, and only those can be cold (`cold_archive/read_classes.rs`
  classifies each as status-disjoint; `tool_pair_ambiguous` only runs after the page it pairs was
  accepted; `rsid` `rpc::tests::cold_s3` pins both with a really moved session). Never relax that
  gate, or add a reader of a session's events that does not call it, without first making the
  reader merge the archive (the leaf cannot decode a chunk: it takes a borrowed `Connection` and
  never the Store).
