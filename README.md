# rsi

[![CI](https://github.com/jakedevar/rsi-cli/actions/workflows/ci.yml/badge.svg)](https://github.com/jakedevar/rsi-cli/actions/workflows/ci.yml)
[![License: AGPL v3](https://img.shields.io/badge/license-AGPL--3.0--only-blue.svg)](LICENSE)

![rsi splash screen](Images/rsi_splash.png)

A vim-native terminal UI for running many AI coding agents at once (Claude
Code, Codex, Antigravity, local models, or a direct API harness) from one
keyboard-driven screen.

> **Status:** alpha. Expect rough edges and breaking changes.

## Why rsi

Running one coding agent is easy. Running five at once usually means a pile of
terminal tabs, agents editing the same checkout, and no idea which one is
waiting on you. rsi gives each agent its own git worktree, keeps every session
alive in a background daemon, and puts all of them on one screen where you can
see what each is doing, what it costs, and which ones need an answer. You drive
it from the keyboard, the way you drive vim.

## Quick start

```bash
git clone https://github.com/jakedevar/rsi-cli.git && cd rsi-cli
make release-install        # builds and links rsi, rsid, rsi-rpc, rsi-agent-mcp into ~/.local/bin
rsi                         # starts the daemon and opens the TUI
```

You need Rust (rustup) and at least one signed-in agent CLI, such as
[Claude Code](https://github.com/anthropics/claude-code) or
[Codex](https://github.com/openai/codex). No subscription? Point the `Local`
provider at [Ollama](https://ollama.com). Then press `<Space>p` to add a
project, `<Space>n` to start a session, and `?` for help anywhere. Details are
under [Install](#install) and [Run](#run).

## What it does

- **One control plane for many agents.** A long-running daemon (`rsid`) owns
  every agent process, transcript, and status. The TUI (`rsi`) attaches over a
  Unix socket, so sessions keep running when you close the terminal.
- **Many providers.** Claude, Codex, Pioneer, Local (Ollama or any
  OpenAI-compatible server), Antigravity, the Codex app server, and a built-in
  direct-API harness. Pick the provider, model, and effort per session.
- **Isolated sandboxes.** Code-changing sessions can run in their own git
  worktree, so parallel agents never step on each other or on your checkout.
- **Structure when you need it.** Organize work into projects, Groups, and
  Epics; let an Epic lead coordinate child agents; attach a dependency DAG for
  multi-step work.
- **Built for long work.** Durable transcripts and costs in SQLite, context
  rotation through structured handoffs, scheduled wakes, and a
  research → plan → implement pipeline checked by contract validators.
- **Vim all the way down.** Modal input, `:` commands, dense layouts, and
  contextual help on `?`.

## Dependencies

rsi runs on Linux and macOS.

### To build

- **Rust**, through [rustup](https://rustup.rs). `rust-toolchain.toml` pins
  the version, and rustup installs it on the first build.
- **A C compiler and `make`.** SQLite and sqlite-vec are compiled from source.
  TLS uses rustls, so no OpenSSL headers are needed.
- **`git`**

One-time setup:

- Arch: `sudo pacman -S --needed base-devel git rustup`
- Debian / Ubuntu: `sudo apt install build-essential git curl`, then install rustup
- macOS: `xcode-select --install`, then install rustup

```bash
# rustup, where your OS package manager doesn't provide it
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

### To run

At least one agent provider, installed and signed in:

| Provider | Needs |
|---|---|
| Claude | [Claude Code](https://github.com/anthropics/claude-code) CLI (`claude`) |
| Codex, Pioneer, Codex app server | [Codex](https://github.com/openai/codex) CLI (`codex`); Pioneer also needs a key in `~/.rsi/.env` |
| Antigravity | Antigravity CLI (`agy`) |
| Local | [Ollama](https://ollama.com) or any OpenAI-compatible server |
| Harness (direct API) | An API key in `~/.rsi/.env` |

`git` is also needed at run time: sandboxes are git worktrees.

RSI runs Claude Code in print mode, with a new CLI process for each resumed
turn. It sets [`CLAUDE_CODE_DISABLE_BACKGROUND_TASKS=1`](https://code.claude.com/docs/en/env-vars)
so Bash commands and subagents stay in the foreground. Claude's built-in
background tasks end with the CLI process, and restoring their stopped-task
notifications can abort the next follow-up without a reply. Use RSI-owned jobs
and wakes for long builds and tests; ordinary commands can run in the foreground.

### Optional

- **systemd user session** (Linux): lets `rsi` start `rsid` for you.
- **`xclip`** (Linux on X11): clipboard support. Wayland needs nothing extra.
- **`pandoc`**: exports the built-in manual to PDF.
- **`aws` CLI**: supplies the default region for Amazon Bedrock when
  `AWS_REGION` is unset.
- **`openclaw`**: lists the live Antigravity model catalog; a built-in list is
  used without it.

### To develop

- `cargo-watch` (installed by `./scripts/dev-tui.sh` if missing)
- `cargo-nextest` for `make test-fast`
- `cargo-deny` for license and advisory checks (`cargo deny check`)
- `python3` for the landing and helper scripts

### Main Rust libraries

ratatui, crossterm, and modalkit (TUI); tokio (async runtime); rusqlite with
bundled SQLite and a vendored sqlite-vec (storage); serde (protocol). Every
crate is pinned in `Cargo.lock`, and `deny.toml` limits third-party
licenses to permissive ones plus MPL-2.0.

## Install

Prebuilt binaries for Linux (x86_64, arm64) and macOS (Apple Silicon), when
available, are attached to
[GitHub Releases](https://github.com/jakedevar/rsi-cli/releases): unpack the
archive and put its four binaries on your `PATH`.

To build from a clone of this repository:

```bash
make release-install
```

This builds release binaries and links `rsi`, `rsid`, `rsi-rpc`, and
`rsi-agent-mcp` into `~/.local/bin` (set `RSI_INSTALL_BIN_DIR` to change it).
Make sure that directory is on your `PATH`. Running it again after an update
restarts a running `rsid` on the new build: in a bounded systemd user scope on
Linux, or as a separate process on macOS.

API keys and other daemon settings go in `~/.rsi/.env`, which `rsid` reads at
startup; copy what you need from [`.env.example`](.env.example). CLI providers
use their own login instead.

## Run

Run the TUI to start `rsid` automatically if it is not already accepting
connections:

```bash
rsi
```

You can also start the daemon yourself before opening the TUI:

```bash
rsid
rsi
```

On Linux, the TUI launches `rsid` in a bounded systemd user scope with a unique
name for each launch. On macOS, it starts `rsid` as a separate process. Both
write daemon output to `~/.rsi/daemon.log`. Set
`RSI_TUI_NO_AUTO_START_DAEMON=1` to disable TUI auto-start.

First steps:

1. `<Space>p` (or `:projects`): create or pick a project for your repository.
2. `<Space>n` (or `:blank <objective>`): start a session. `<Space>o` or
   `:task <objective>` starts a one-shot task.
3. `Enter` opens a session, `F3` shows exactly what it launched with, `x`
   stops it, `<Space>a` archives it, and `<Space>N` (or `:alerts`) opens notifications.
4. `?` lists the keys available wherever you are.

The [operator manual](docs/agent-harness-operator-manual.md) covers daily use,
and [docs/keybindings.md](docs/keybindings.md) is the full key reference.
State lives in `~/.rsi/`: `rsi.db` (SQLite), `daemon.sock`, and `logs/`.

## Develop

```bash
./scripts/dev-daemon.sh   # cargo run --bin rsid
./scripts/dev-tui.sh      # rebuilds and relaunches the TUI on change (cargo-watch)
make test-fast            # daemon unit tests (needs cargo-nextest)
```

The main crates are `rsi` (TUI, built on ratatui and modalkit), `rsid`
(daemon), and `rsi-common` (shared types, JSON-RPC protocol, validators);
supporting crates live under `crates/`. [AGENTS.md](AGENTS.md) holds the
conventions AI agents follow when working in this repository.

## License

rsi is free software under the
[GNU Affero General Public License v3.0 only](LICENSE). Commercial licenses
are available on request. See [NOTICE](NOTICE) for copyright, credits, and
third-party licenses, and [CONTRIBUTING.md](CONTRIBUTING.md) before sending
code.

## Thanks

rsi grew out of ideas from [HumanLayer](https://github.com/humanlayer/humanlayer),
Jake Van Clief and David McDermott's
[Interpretable Context Methodology](https://arxiv.org/abs/2603.16021), and
Stanford's [Meta-Harness](https://arxiv.org/abs/2603.28052) paper.
