# tools/

Repo tooling for the rsi workspace. The MWP/ICM contract validators
(`rsi-contract/research/manifest/handoff-validate`, built from `rsi-common`)
are wired to run automatically at two gates:

## Pre-commit hook — `git-hooks/pre-commit`

Installed via `./tools/install-hooks.sh` (sets `core.hooksPath` for all
worktrees). Two responsibilities:

1. **RSI-021 branch/path invariants** — keyed on `$CLAUDE_AGENT_ROLE`. An empty
   role (human/master commit) exits 0 with **zero enforcement** — this invariant
   is load-bearing; do not break it.
2. **A3 staged-artifact validation** (non-empty roles only) — maps each staged
   pipeline artifact to its validator and blocks the commit on a genuine
   failure:
   - `thoughts/shared/research/*.json` → `rsi-research-validate --strict`
   - `thoughts/shared/handoffs/**/*.md` → `rsi-handoff-validate --strict`
   - `*.md` with `phases_sealed:` in frontmatter → `rsi-manifest-validate`

   Uses a **pre-built** validator binary (resolved via `$RSI_VALIDATE_BIN_DIR`
   → `PATH` → `$CARGO_TARGET_DIR` → cargo-config `target-dir` → repo `target/`);
   if none is found it **skips with a warning** (never forces a compile). Build
   them once with `cargo build --release -p rsi-common --bins`.

## Pre-push hook — `git-hooks/pre-push`

Installed by the same script. An agent session (marked by `RSI_SESSION_ID` or
`RSI_SESSION_TOKEN`) cannot push an update to
`refs/heads/rolling` or `refs/heads/main`. Other refs and operator pushes are
unaffected. The guarded `rsi-rolling-land` publisher uses a private clone and
disables hooks for its validated push.

Run `python3 tools/test_pre_push.py` for real Git push checks.

## CI — `.github/workflows/test.yml` job `validators`

Always-on gate, independent of local hook install:
- **command-mirror drift** (hard) — `check-command-mirrors.sh`
- **changed-artifact strict** (hard) — `validate-artifacts.sh changed main`
  (validates only artifacts changed vs `main`; zero legacy-corpus noise)
- **corpus lenient sweep** (advisory, `continue-on-error`) —
  `validate-artifacts.sh corpus` (surfaces the legacy validation backlog)

The separate required `Released migration immutability` job runs
`check-released-migrations.py BASE HEAD`. Its committed inventory pins every
released `if version < N` block and each marked migration-owned catalog/helper
region. Existing base pins are append-only, so changing source and refreshing
the corresponding hash in one PR still fails. A new migration is accepted only
when `LATEST_SCHEMA_VERSION` advances and the inventory appends one block for
every new version. Refresh after adding a migration with:

```bash
python3 tools/check-released-migrations.py --refresh
```

When the migration adds a new helper file, include it in the refresh so its
marked sections are pinned:

```bash
python3 tools/check-released-migrations.py --refresh --include-path crates/rsid/src/store/new_migration.rs
```

Released migration DDL, catalog projections, and fingerprints are immutable.
Never repair them in place; add a forward migration.

## Scripts

- **`validate-artifacts.sh {changed [BASE] | corpus}`** — run the validators over
  pipeline artifacts. `changed` is a strict hard gate over the diff vs BASE
  (default `main`); `corpus` is a lenient advisory sweep that always exits 0.
- **`check-released-migrations.py [BASE [HEAD]]`** — hard-fail retroactive
  edits to released migration blocks or protected catalog/helper regions.
- **`check-command-mirrors.sh`** — CI entrypoint for the single command-mirror
  pipeline. It delegates to `scripts/sync-agent-commands.sh --check`, which
  renders every top-level Claude-canonical command as a Codex adapter
  (first-block routing keys removed plus committed argument hints), Gemini TOML
  wrapper, and Codex-derived `.agents` skill. It also mirrors project-native
  `.claude/skills` files into `.agents/skills`. It exits nonzero on missing
  files or rendered-content drift, so new Claude commands or skills cannot
  silently lack Codex equivalents.
- **`install-hooks.sh`** — activate the git hooks (`core.hooksPath`).
