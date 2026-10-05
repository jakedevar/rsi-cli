# RSI Desktop

A desktop UI for rsi: a Tauri v2 window with a TypeScript frontend and a Rust
backend that talks to `rsid` over its JSON-RPC socket (`~/.rsi/daemon.sock`,
override with `RSI_DAEMON_SOCKET`). It runs alongside the TUI against the same
daemon.

```bash
./scripts/desktop.sh          # build frontend + run (debug)
./scripts/desktop.sh release  # optimized binary
cd desktop && npm test        # frontend model tests
cd desktop/src-tauri && cargo test
```

Needs `webkit2gtk-4.1` and node. `cargo tauri build` (in `desktop/src-tauri`)
produces `.deb`/AppImage bundles.

## Install

`make release-install` (and `release-install-no-restart`) also builds the
desktop UI and installs it via `scripts/install-desktop.sh`:

- `~/.local/bin/rsi-desktop` (symlink to an atomic copy in `~/.rsi/install/`),
- `~/.local/share/applications/rsi-desktop.desktop` (the app-menu entry, from
  `packaging/rsi-desktop.desktop` with an absolute `Exec`),
- icons in `~/.local/share/icons/hicolor/{32x32,128x128,512x512}/apps/`.

If node or `webkit2gtk-4.1` is missing, or the desktop build fails, that step
only warns; the rsid install is unaffected. `make desktop` builds only and
`make desktop-install` builds and installs, both failing loudly on a missing
toolchain. Set `HOME` (or `RSI_INSTALL_BIN_DIR`/`XDG_DATA_HOME`) to a temp dir
to try the install without touching your real one.

Frontend assets are embedded at compile time; `build.rs` watches `dist/`, so a
plain `cargo build` picks up frontend-only changes after `npm run build`.

## Layout

- `src-tauri/`: the Rust backend. It is a standalone Cargo workspace, so the
  root workspace build never pulls in Tauri/webkit.
  - `daemon.rs`: line-framed JSON-RPC client. Each call is bounded by a 20 s
    deadline and a 64 MiB response limit, and never sends a session token.
  - `lib.rs`: the closed command set. Each command validates its input and
    forwards exactly one daemon method: `GetHealthStatus`, `ListProjects`,
    `ListSessions`, `GetSession`, `GetConversation`, `ContinueSession` (the
    daemon queues it when a turn is active), `InterruptSession` (soft pause)
    and `LaunchSession`. The webview has no generic passthrough.
- `src/`: the TypeScript frontend (`api.ts` typed command adapter, `model.ts`
  pure view logic, `main.ts` DOM). It has no framework and no runtime npm
  dependencies; it uses the global `__TAURI__` bridge. All daemon text is
  rendered as text, never as HTML.
- `static/`: the HTML shell and CSS (Catppuccin Mocha, matching the TUI).

## v1 features

- Session list: polled every 3 s, active sessions first; filters for project,
  text and show completed.
- Conversation view: incremental `since_sequence` polling every 1.5 s. Tool
  calls, results and thinking are collapsed; base64 blobs are elided and long
  messages folded. Older events load in pages.
- Send a message, interrupt, and launch a new session (choose provider, model,
  effort, project and working dir).
- Daemon connection indicator.
