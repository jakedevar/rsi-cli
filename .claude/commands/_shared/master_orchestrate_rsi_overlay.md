# RSI Master Orchestrate Overlay

Project-local tightening for the portable `master_orchestrate` core. Apply
these rules in this repository. Backend policy injected by rsid remains
authoritative.

## Conditional References

Load conditional mechanics only when activated:

- For explicit `mode:program`, read
  `.claude/commands/_shared/master_orchestrate_rsi_program.md` before the
  first RSI control action.
- For Closure-tagged independent review, read
  `.claude/commands/_shared/master_orchestrate_rsi_closure.md` before sealing
  or dispatching review evidence.

Ordinary slice work does not load either reference.

## Repo Identity

This repo is `rsi`, a vim-like TUI for managing multiple AI coding sessions:

- `rsi`: TUI client
- `rsid`: daemon and provider adapters
- `rsi-common`: shared types, JSON-RPC protocol, contracts, validators

Follow `AGENTS.md` for repository, database, branch, formatting, and commit
rules. Use `Jake` only when a project-local command explicitly requires it.

## Stage Contract And Artifacts

The MWP/ICM stage contract from `AGENTS.md` remains binding for worker replies:
`PIPELINE HANDOFF`, then `Inputs`, `Process`, `Outputs`, `Verify` in
canonical order. Research sidecars, when required, use schema version 2 with
stable finding IDs; consuming plan items use `satisfies:`, and applicable
verification manifests use `covers:`.

Create a plan, research sidecar, handoff file, manifest, or ledger only when the
caller, accepted project policy, or an actual downstream consumer requires it.
Do not create a document to prove that no document was needed. Existing required
artifacts remain source inputs and must not be discarded.

When a reply or manifest is required, use the applicable validator:

```bash
cargo run -q -p rsi-common --bin rsi-contract-validate -- \
  <TICKET> --strict-v2 --manifest <manifest_path> < reply.txt
cargo build -p rsi-common --bin rsi-handoff-validate
cargo build -p rsi-common --bin rsi-research-validate
cargo build -p rsi-common --bin rsi-manifest-validate
```

Use `rg --no-ignore` under `thoughts/`. Artifact locations, when required:

- plans: `thoughts/shared/plans/`
- research: `thoughts/shared/research/`
- handoffs: `thoughts/shared/handoffs/`
- reference: `thoughts/shared/reference/`

Any created or modified `thoughts/` file must be committed under the hard
policy in `AGENTS.md`.

## Conflict Domains

Use this vocabulary unless a narrower gate pack overrides it:

- `docs-plans`
- `rsi`
- `rsi-common`
- `rsid-store-rpc`
- `ui-visual`
- `schema-migration`
- `security-boundary`
- `live-execution`
- `other`

Schema changes require an inline versioned migration in
`crates/rsid/src/store/mod.rs` and matching `user_version` bump. Released
migrations and pins remain immutable.

## Verification Defaults

Prefer narrow checks tied to touched behavior. Format only changed Rust files
with direct `rustfmt`; never use scoped-looking `cargo fmt`.

Common focused commands:

```bash
cargo test -p rsid --test <test_name>
cargo test -p rsi-common --bin <validator_name>
scripts/sync-agent-commands.sh --check
tools/check-command-mirrors.sh
```

Run `bash scripts/e2e.sh` when work touches TUI navigation, hierarchy,
session-list rendering, relevant UI paths, or their keybindings. Record one:

```text
tui-e2e: PASS
tui-e2e: SKIPPED - not due: <reason>
tui-e2e: PARTIAL - due but unavailable: <reason>
tui-e2e: BLOCKED - assertion failure: <reason/artifact path>
```

## Documentation

Implementation or fix owner updates due documentation in the same source
revision. Do not dispatch a separate documentation worker by default.

Keybinding changes update `docs/keybindings.md`. Material user-facing RPC,
schema, configuration, public-contract, or behavior changes update human-facing
`docs/`. `AGENTS.md` and `thoughts/` do not substitute for user docs.

Command changes are authored under `.claude/commands/`; run
`scripts/sync-agent-commands.sh` to update Codex and Gemini mirrors.

## Capability Classes

Map portable classes at spawn:

| Class | Claude | Codex |
| --- | --- | --- |
| `architect` | `model=opus effort=xhigh` | `model=gpt-6-astra effort=xhigh` |
| `implementer` | `model=sonnet effort=high` | `model=gpt-6-astra effort=high` |
| `lookup_fast` | `model=haiku` | `model=gpt-6-astra effort=low` |

The daemon resolves provider aliases and rejects invalid effort values. A
cross-provider child receives an explicit provider-valid model/effort; never
carry model spelling across providers. An explicit caller model/effort wins.
Escalation requires concrete capability insufficiency, never authority,
resource, external-state, or formatting failure.
