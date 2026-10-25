# Project profile, version 1

The daemon's `agent_jobs::project_profile::resolve(custody_root)` reads
`.rsi/project.toml` from one caller-proven worktree root. This contract requires
no RSI scripts or repository documentation. Job submission and candidate checks
consume this same resolver (#413). Publication, bootstrap and resource execution
remain separate work in #414–#415 and #408. Existing `.rsi/jobs.toml` recipes
remain available through the resolved recipe map.

A Makefile-only project can use this complete profile:

```toml
version = 1
verification = ["test"]
build = "build"
warnings = "generic"

[integration]
branch = "develop"
mode = "fast_forward"

[recipes.test]
runner = "make"
target = "test"
timeout_minutes = 20
cpu_quota_percent = 200

[recipes.build]
runner = "make"
target = "build"
timeout_minutes = 20
cpu_quota_percent = 200
```

This is also a usable bootstrap skeleton for a project with those Makefile
targets; the resolver does not create files. `version = 1` alone is a valid
declaration with an explicit `no_tests` verification result and no integration.
An invalid or unsupported declaration fails closed; it never selects fallback.

Recipes reuse the legacy jobs validator: up to 64 names, runner `make` or `just`,
literal target tokens, timeout 5–180 minutes and CPU quota 1–1600 percent.
No shell text, executable paths, arguments or environment declarations are
accepted. Verification is an ordered list of at most 64 recipe names; `build`
and `cleanup` each name one recipe. Every referenced recipe must exist and its
local `Makefile` or `justfile` must be a regular file. Retired `scoped-test` and
`scoped-test-tmpfs` roles or target aliases are refused; RSI's existing typed
scoped-test path continues to own those gates.

The resolver merges `.rsi/jobs.toml` recipes into the same recipe map. Equal
duplicate declarations are allowed; a conflicting duplicate is an error.
Legacy manifests keep version 1 validation, including their existing caps.
Unreferenced legacy recipes have no inferred role except the exact names
`test` and `build` when no profile exists.

`integration` is optional, with a portable Git branch name and mode `queue` or
`fast_forward`. It is configuration, never permission to publish or bypass
main/release/custody gates. `warnings` is `generic` (default; no Rust baseline is
presumed) or `no_new_rust_warnings`. Declarations alone are never successful
warning verdicts; verification records the applicable verdict below.

Optional per-worktree resources use recipe references:

```toml
# At the top level, before any table:
# cleanup = "clean"

[resources.build-output]
path = "build/output"
setup = "build"
cleanup = "clean"

[recipes.clean]
runner = "make"
target = "clean"
timeout_minutes = 20
cpu_quota_percent = 200
```

Up to 32 resources are allowed. Paths name directories relative to the exact
custody root, using `/` on every platform. Components use ASCII letters,
digits, `-`, `_` and `.`, with no leading/trailing dot, reserved Windows device
name, empty component or traversal. Absolute paths, drive prefixes,
backslashes, symlink components (including internal and dangling aliases),
existing files, and nested repository roots are refused. Resources may not
overlap, including after ASCII case folding. Missing directory components are
allowed for setup. The resolver rejects symlinked declaration/runner files
and reads at most 64 KiB from each input.

These checks establish path shape at resolution time, not deletion authority.
An execution adapter must prove live sandbox custody, revalidate paths at apply
and resume, protect against filesystem races, and enforce user-data and active
job protections. Declarations are project code under existing job authority;
they grant no credentials, publishing or cleanup authority.

When no profile exists, resolution is conservative and deterministic:

1. Prefer legacy recipe roles named exactly `test` and `build`.
2. Otherwise recognize an unindented literal `test:` / `build:` rule in the
   local `Makefile`, then local `justfile`. No includes, variable expansion,
   ancestor discovery, parameterized recipes or project code are evaluated.
   Inferred recipes use 20 minutes and 200 percent CPU as declaration caps;
   execution policy must still clamp to daemon limits.
3. Otherwise a local parsed `Cargo.toml` containing a package/workspace table
   selects typed Cargo test/build roles and the Rust warning policy.
4. Return `unsupported` when local build evidence exists but no supported test
   role can be established; an empty directory returns `no_tests`.

Fallback never infers an integration branch, landing mode, resource or cleanup
action. `available` is a command selection, not evidence that any tests ran or
passed.

## Verification evidence (#413)

Submit a durable test job with `params.scoped_test.base` naming a fetched commit
or ref. A non-Rust project runs every resolved verification recipe in order;
no `scripts/scoped-test` or RSI documentation is required. A Makefile containing
a literal `test:` target is sufficient. Alternatively `params.recipe:"test"`
produces the same evidence when that recipe is the entire resolved verification
role, without focused filters. Other recipe jobs remain diagnostic jobs; one
recipe from a multi-command role does not verify the whole role.
Existing recipe jobs outside a Git repository remain diagnostic jobs; without
a source commit they cannot produce candidate verification evidence.

The existing Rust adapter still owns projects containing `Cargo.toml` or the
legacy `scripts/scoped-test` entry point, including no-new-failure comparisons,
trusted rolling-tool selection and the Rust warning baseline. A generic
declaration cannot waive that gate. A non-Rust profile requesting
`no_new_rust_warnings` is refused `project_verification_warning_policy_unsupported`.

The daemon records `result.profile_verification` in durable job storage before
launch. Its versioned binding contains the exact source HEAD, canonical worktree,
clean-tree policy, raw and resolved profile digest, exact command argv and SHA-256
identities of the daemon, shell and selected make/just binaries. It rechecks at
launch, while running, at finish and when judging a candidate. HEAD movement,
dirtiness, changed profile/recipe bytes, changed runner or an unreadable binding
fails closed. Cleanliness includes tracked and untracked changes and index
visibility checks; Git-ignored outputs and the authenticated root daemon build
stamp follow existing custody observation policy. Declare generated output in
the project's ignore rules. Generic jobs create no Rust target/build stamp.

Only successful daemon-observed process completion counts. Project-authored
`SCOPED_TEST_RECEIPT`, JSON, warning claims and hand-run scripts cannot supply
this evidence. The generic warning verdict is explicitly `generic_command_exit`:
the declared commands succeeded and no Rust package baseline applies. This does
not claim to parse arbitrary compiler warning output. Receipts count completed
verification commands, not individual tests inside opaque make/just recipes;
`tests_completed` remains zero rather than inventing a test count. Empty roles,
unsupported roles and failed/incomplete jobs never pass. This adapter cannot
prove that a project-authored test target contains meaningful assertions.

All commands share the tightest declared recipe timeout, capped further by the
admitted job timeout, and the tightest declared CPU budget, capped by daemon
policy. This conservative aggregate budget avoids borrowing a longer recipe's
allowance for another command.

`AgentCheckLandingCandidate` accepts exact-head profile evidence. A manager may
name its own verification job with the worker's `source_session_id` only while
the manager still reaches that worker, the caller reaches both identities, the
stored job project matches, and the canonical job directory is exactly the
worker's registered sandbox. Without a job id, lookup also considers the current
caller's newest 100 jobs, subject to those same checks. A job id alone cannot
attribute a manager's foreign-worktree job to the manager's own sandbox. Stale
evidence reports `candidate_profile_stale`; zero-command evidence reports
`candidate_no_verification_commands`. Admission and execution failures retain
their durable job refusal. Evidence never grants publication authority.

Linux and macOS use the existing durable job backends. Other hosts return an
explicit unsupported-platform error. Source and tool checks are bounded
observations, not filesystem isolation against changes between observations;
as with existing build jobs, keep the source unchanged throughout verification.
