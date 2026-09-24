# tokio-console

Build with the feature flag:
`cargo build --bin rsid --features tokio-console`

Run with the env var to actually start the subscriber:
`RSI_TOKIO_CONSOLE=1 cargo run --bin rsid --features tokio-console`

Connect the `tokio-console` client (default endpoint `127.0.0.1:6669`):
`tokio-console`

Tasks visible: per-session monitor loops, store_worker, RPC server,
EventBus subscribers. Default daemon (no env var, no feature flag) is
zero-overhead — the gated init is elided at compile time when the
feature is off.

## Double opt-in design

The subscriber is gated at two levels:

1. **Compile time** — `#[cfg(feature = "tokio-console")]` in `main.rs`. Without the feature flag the gated block is completely absent from the binary; `console-subscriber` is not linked.
2. **Runtime** — `RSI_TOKIO_CONSOLE=1` env var. Even with the feature compiled in, the subscriber is only initialized when the var is set. This allows shipping a debug binary that enables the console on demand without recompiling.

`console_subscriber::init()` is called before any other tracing setup so it wins the global dispatcher slot. The standard `tracing_subscriber::fmt` init that follows is skipped when the console subscriber is active (the console crate installs its own fmt layer internally).

## Troubleshooting

- **Port already in use** — `tokio-console` binds `127.0.0.1:6669` by default. Kill any stale daemon or set `TOKIO_CONSOLE_BIND` to an alternate address.
- **No tasks visible** — Ensure the daemon was started with `RSI_TOKIO_CONSOLE=1` and the `tokio-console` feature compiled in. A default build produces no console endpoint.
- **tokio-console client not found** — Install with `cargo install tokio-console`.
