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
`RSI_SESSION_TOKEN`) cannot push to `refs/heads/main` or make an unmarked push
to `refs/heads/rolling`. Other refs and operator pushes are unaffected. The
guarded `rsi-rolling-land` publisher uses a private clone and sets
`RSI_ROLLING_LANDER=1` for its validated rolling push. This hook is a cooperative
guard rail, not a security boundary: any agent can spoof the marker or bypass
the hook. The lander enforces test gates and fast-forward publication.

An operator push to `refs/heads/rolling` that the lander did not mark runs the
fast static guards on the pushed commit (about a second): the Python syntax of
the `scripts/` and `tools/` files the push changes, `check-released-migrations.py`
against the remote tip, the identity-assertion guard, and the portable-shell guard. It refuses on a failure
and always prints the one-line queue command (`rsi land <session-id|branch>`,
#2055), because a manual merge and push skips every gate. `RSI_SKIP_STATIC_GUARDS=1`
is the documented operator override.

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

The inventory is derived output: never hand-edit it. When a merge of
`origin/rolling` conflicts on `tools/released-migrations.json`, leave the
conflict markers and run the same command: `--refresh` unions the pins of both
merge sides and appends the pins for your new versions from source (a version
both sides pin differently still fails).

When the migration adds a new helper file, include it in the refresh so its
marked sections are pinned:

```bash
python3 tools/check-released-migrations.py --refresh --include-path crates/rsid-store/src/store/new_migration.rs
```

Released migration DDL, catalog projections, and fingerprints are immutable.
Never repair them in place; add a forward migration.

## Scripts

- **`validate-artifacts.sh {changed [BASE] | corpus}`** — run the validators over
  pipeline artifacts. `changed` is a strict hard gate over the diff vs BASE
  (default `main`); `corpus` is a lenient advisory sweep that always exits 0.
- **`check-released-migrations.py [BASE [HEAD]]`** — hard-fail retroactive
  edits to released migration blocks or protected catalog/helper regions.
- **`check_starter_pins.py [BASE [HEAD]]`** (#3070) — hard-fail a changed starter
  topology JSON (`crates/rsid/src/topology/starters/*.json`) whose digest fixture
  in `starters_tests.rs` was not moved or whose replaced digest was not appended
  to `SHIPPED_BEFORE` in `starters.rs`. A text-only comparison, no build; run by
  `scripts/scoped-test` as the `starter_pins` static guard and by the pre-push
  hook. Tests: `python3 -m unittest tools/test_check_starter_pins.py`.
- **`check-command-mirrors.sh`** — CI entrypoint for the single command-mirror
  pipeline. It delegates to `scripts/sync-agent-commands.sh --check`, which
  renders every top-level Claude-canonical command as a Codex adapter
  (first-block routing keys removed plus committed argument hints), Gemini TOML
  wrapper, and Codex-derived `.agents` skill. It also mirrors project-native
  `.claude/skills` files into `.agents/skills`. It exits nonzero on missing
  files or rendered-content drift, so new Claude commands or skills cannot
  silently lack Codex equivalents.
- **`install-hooks.sh`** — activate the git hooks (`core.hooksPath`).

## Portable-shell guard — `check_portable_shell.py`

Scripts under `scripts/` run on the Linux hub and the macOS satellite (bash 3.2, BSD
userland), so a BSD-only or GNU-only form breaks one of them (#2936, local #322).
`tools/check_portable_shell.py` (wrapper `scripts/check-portable-shell.sh`) flags
`mktemp -t` without X's, `sed -i ''`, bare `sed -i`, `stat -f`/`-c`, `date -j`/`-d`,
`readlink -f`, `grep -P`, `xargs -r` and bash 4+ (`declare -A`, `mapfile`, `${var,,}`,
`wait -n`), and each finding names the portable alternative. It scans `*.sh`/`*.bash`
and shebang-shell files under `scripts/`, including heredoc bodies that feed a shell
(`ssh host bash -s <<EOF`, `cat > x.sh <<EOF`); heredocs to Python or plain text are skipped.

A form is allowed inside an `if`/`case` that probes `uname`/`$OSTYPE`, when a BSD and a
GNU form of one tool are joined by `||` in one command list (the `stat -c ... || stat -f ...`
fallback, including a line ending in `||` or `\\` then `||`; adjacent lines alone are not),
or under a marker: `# portability: linux-only` exempts GNU-only forms and
`# portability: macos-only` exempts BSD-only forms, in the file's leading comment block
(whole file), on the line, or on the comment line above it.

Existing uses are counted per file in `tools/portable-shell-baseline.json`. The guard
fails on a file above its count, on a stale (too high) count, and, with `--base REF`, on a
baseline that raises any count over REF's. Lower the baseline after fixing a use with
`python3 tools/check_portable_shell.py --refresh`; it refuses to raise it. It runs in the
pre-commit hook (staged `scripts/` files), the pre-push static guards, and
`scripts/scoped-test` (the lander's static guards, with the base comparison). Tests:
`python3 -m unittest scripts/tests/test_check_portable_shell.py`.
