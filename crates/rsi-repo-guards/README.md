# rsi-repo-guards

The repository's static guards in Rust (#360, umbrella #358: no Python in the repo).
One crate, one binary (`rsi-repo-guard`), one subcommand per guard. Callers never
run the binary from a fixed path: they go through `scripts/repo-guard`.

```sh
scripts/repo-guard <guard> [args...]   # run a guard in the current directory
scripts/repo-guard --bin [guard]       # resolve a binary capable of this guard
scripts/repo-guard --list              # every guard and its source file
cargo test -p rsi-repo-guards          # the guards' tests
```

`tests/legacy_golden.rs` replays 78 synthetic identity/privacy CLI cases against
the legacy outputs captured in daemon job `1e9fcd5b-2da0-43b7-8543-ba19293aac08`.
The fixture README records the exact legacy source and normalization. Replay
executes Rust only. `tests/pre_push.rs` exercises the hook with no Python on
PATH, including scanner findings and an unavailable Rust binary.

| Guard | Replaces | Called by |
| --- | --- | --- |
| `new-python` | new tracked Python prevention (#407, #583, directive #358) | pre-commit, pre-push, scoped-test (legacy through `starter-pins`; Rust runner directly) |
| `identity-assertions` | `tools/check_identity_assertions.py` | pre-commit, pre-push, scoped-test, `scripts/check-identity-assertions.sh`, CI |
| `portable-shell` | `tools/check_portable_shell.py` | pre-commit, pre-push, scoped-test, `scripts/check-portable-shell.sh` |
| `theme-test-guards` | `tools/check_theme_test_guards.py` | pre-commit, `scripts/check-theme-test-guards.sh`, CI |
| `personal-paths` | `tools/check_personal_paths.py` | pre-commit, pre-push, scoped-test, `scripts/check-personal-paths.sh`, `scripts/export-rsi-cli.sh`, CI |
| `operator-identifiers` | `tools/check_operator_identifiers.py` | pre-commit, pre-push, `scripts/check-operator-identifiers.sh` |
| `starter-pins` | `tools/check_starter_pins.py` | pre-push, scoped-test |
| `settings-preflight` | `tools/check_settings_preflight.py` | scoped-test |
| `test-env-writes` | `tools/check_test_env_writes.py` | pre-push, scoped-test, CI |
| `export-includes` | `tools/check_export_includes.py` | `scripts/check-export-includes.sh`, CI |
| `bash32-arrays` | `tools/check_bash32_arrays.py` | tests only (`audited_macos_scripts_are_clean`) |
| `agents-md-size` | `tools/check_agents_md_size.py` (#3450) | pre-push, scoped-test |
| `release-target` | the embedded python block of `release_guard_receipt_target` in `scripts/release-target-guard.sh` (hub #3715, #3716) | `scripts/install-release.sh --build-receipt` (through `release_guard_receipt_target`) |
| `store-boundary` | staged port of `scripts/check-store-boundary.py` (#362) | API/shell entry available; canonical caller switch pending |
| `target-reclaim` | staged port of `scripts/check-target-reclaim.py` (#362, #424) | API/shell entry available; canonical caller switch pending |
| `scoped-test-failure` | staged port of `scripts/scoped-test-failure.py` (#362) | in-process receipt classifier / receipt JSON shell entry |
| `recent-landing-check` | staged port of `scripts/recent-landing-check.py` (#362) | in-process advisory / JSON shell entry; equivalence gaps documented |
| `rsid-test-shards` | staged port of `scripts/check-rsid-test-shards.py` (#362) | ordered inventory, module/cfg and Nextest API; caller switch pending |
| `guard-regressions` | staged port of `scripts/check-guard-regressions.py` (#362) | exact committed snapshot and historical blob adapters; hard-deadline worker/caller switch pending |
| `rsid-health-probe` | the `python3 -` GetHealthStatus probe of `scripts/rsid-health.sh` (#465) | `rsid_rpc_ready` (install-release.sh, rsid-start-satellite) |
| `rsid-quiet-restart` | the `RequestOperatorRestart` heredoc of `scripts/rsid-quiet-restart.sh` (#465) | `request_quiet_restart` (install-release.sh) |
| `release-guard` | the flock, symlink, keep/prune and receipt-status heredocs of `scripts/release-target-guard.sh` (#465) | install-release.sh, install-desktop.sh |

The #362 integration boundary and precise #423 wiring are documented in
[`python-free-static-helper-bootstrap.md`](../../docs/agents/python-free-static-helper-bootstrap.md).

## The wrapper

`scripts/repo-guard` builds this crate on demand from the checkout that holds the
script, with the caller's own Cargo setup (an rsi sandbox's stamped
`CARGO_TARGET_DIR` and `RUSTC_WRAPPER`, or the checkout's `target/`), in the release
profile. It never picks a target of its own. By default a built binary newer than
the crate, `Cargo.toml` and `Cargo.lock` is reused if its `--list` capability probe
includes the requested command and at least as many guards as this tree declares.
Otherwise the wrapper builds it. When Cargo is missing or the build fails it
prints why and exits 2: a guard that cannot run never reads as a pass.

For a literal no-build scope, set `RSI_REPO_GUARD_NO_BUILD=1` on the command, for
example `RSI_REPO_GUARD_NO_BUILD=1 git commit -m 'fix: ...'`. The environment is
inherited by every pre-commit and pre-push guard resolution, including `--bin`,
`--list` and nested wrappers; it does not change persistent Git configuration.
Unset or `0` preserves the build-enabled default; other values refuse. Pre-push
requests each command with `--bin <guard>`, so resolving an earlier guard never
waives a later command's capability check. Assigned source-branch pushes retain
their existing dispatch; protected-ref checks remain mandatory.

No-build resolution reuses the admission bootstrap's `--existing-bin` machinery:
it searches the caller's target for release/debug native or `.exe` binaries and
runs `cargo-slot verify-build` in the candidate binary. Despite that subcommand's
name, verification starts no Cargo: it compares the embedded content stamp with
this checkout's root manifests, lock and toolchain, the guard manifest/build.rs,
and recursive Rust sources under `src/`. It also requires the capability probe.
An external integration-test-only change under `tests/` preserves the stamp;
changed compiled inputs do not. File times are not evidence in this mode.

A matching prebuilt runs the actual requested guard with its original arguments
(including `new-python --staged`, which judges index blobs). Missing, cold,
stale, incompatible or incapable binaries refuse nonzero with no Cargo fallback;
guard failures keep their nonzero verdict. None produces a success receipt or
deferred-check waiver. Cold targets and changed compiled inputs require a
separately authorized, platform-compatible prebuild; a Linux executable cannot
serve a Mac hook. This is not full cold-worker deferred publication.

“No hand-run builds” can allow existing automatic hooks and their compilation
when the task expressly permits them. Literal “no builds” instead requires the
matching prebuilt or refusal described above. “No scripts/guards executed” also
forbids running hook checks, so it cannot commit through executing hooks, even
with no-build mode. Never suppress hooks, add skip flags or alter shared hook
configuration to resolve that scope. Publication is not acceptance: UNRUN hub
checks remain UNRUN until the existing exact-final static/scoped gate and
acceptance owner verify them. This mode changes no receipt ownership or gates.

The git hooks find the wrapper next to themselves (`tools/git-hooks/../../scripts`),
so the guard code always comes from the same checkout as the hook. Where a hook or
`scripts/scoped-test` used to check that the judged tree carries the guard's `.py`
file (a tree that predates a guard skips it), it now checks for the guard's module
source, `crates/rsi-repo-guards/src/<guard>.rs` (the third column of `GUARDS` in
`src/lib.rs`).

## Adding a guard (the pattern for later slices)

New static guards belong in this crate, called through `scripts/repo-guard`;
never add a Python guard or Python tests. `scripts/repo-guard new-python BASE HEAD`
refuses added tracked `.py`, `.pyw`, `.pyi` paths (case insensitive), including
rename/copy destinations, and Python/PyPy shebangs with direct or `env` interpreters,
regardless of executable bit. It reads bounded prefixes of exact Git blobs and
never executes source. Existing edits and deletions remain allowed for the
conversion slices. Only non-executable regular archived data under `thoughts/`
without a Python shebang is exempt. Symlinks resolve through the candidate Git
tree, including directory components, and receive no archive exemption; external,
dangling, cyclic or malformed targets fail closed, as does unavailable blob evidence.
`--staged` pins the index tree, including an initial commit, and ignores
untracked/unstaged files. Revision mode pins the requested commit tree.

The unchanged legacy `scripts/scoped-test` calls `starter-pins BASE HEAD` as a
mandatory static check. That Rust entry point explicitly runs `new-python` first
in revision mode, so prevention ships without editing Python or switching the
#423/#426 callers. Its `--tree` starter-only audit stays unchanged. The Rust scoped
runner also records its own mandatory `new_python` verdict. Keep this compatibility
call until the legacy runner is retired; do not drop the other static checks.

1. Add `src/<guard>.rs` with `pub fn main(args: &[String], ctx: &Ctx, out: &mut dyn
   Write, err: &mut dyn Write) -> i32`, and register it in `GUARDS` (`src/lib.rs`)
   with its subcommand name and module path.
2. Keep the replaced script's behaviour byte for byte: the same output on the same
   stream, the same exit codes (argparse usage errors are 2, an uncaught Python
   exception is 1), the same baseline and allowlist files. Reuse `py` for Python
   semantics (`splitlines`, `repr`, `strip`, `normpath`, text-mode newline
   translation) and `re::Re` for the patterns: `fancy-regex` takes Python's
   look-around and back-references verbatim. A message that names the old `.py`
   file names the Rust module or `scripts/repo-guard <guard>` instead.
3. Read the environment and the working directory only through `Ctx`; tests build
   their own `Ctx`, so no test writes the process environment.
4. Port every case of the Python test file into the module's tests (or a
   `#[path = "<guard>_tests.rs"]` file when they are long): same inputs, same
   expected refusal text and exit code. Fixtures must not trip the repository's own
   guards when they scan this crate: build personal names, public IP literals and
   identity-assertion pairs at run time (see `personal_paths.rs`, `identity_assertions.rs`).
5. Switch every caller to `scripts/repo-guard <guard>`, then delete the `.py` and its
   Python tests in the same change.
