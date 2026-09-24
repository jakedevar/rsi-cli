# Recursive DAG Operator Guide

This guide describes the recursive DAG feature as it exists today. It is for
operators and developers who want to inspect, dogfood, and safely exercise the
fake/manual recursive DAG workflow and the explicitly gated ordinary live smoke
path.

## Current Status

Fake/manual recursive DAG dogfooding is ready. Bounded ordinary live dogfood
smokes are also covered for isolated, intentional runs through
`RunRecursiveLiveScheduler` followed by `CommitRecursiveLiveAttemptOutput`.

The ready surface is:

- daemon persistence and recovery for recursive DAG graphs, tasks, edges,
  attempts, scheduler runs, cancellation requests, recovery passes, artifacts,
  validation rows, live-attempt readbacks, and typed inspector foundations
- explicit fake scheduler control with bounded `max_steps`
- explicit ordinary live scheduler control with bounded `max_steps` when
  `recursive_dag_live_scheduler_control_enabled=true`
- explicit completed live-attempt output commit with
  `CommitRecursiveLiveAttemptOutput` under the same control gate
- graph and scheduler-run cancellation controls
- manual recovery continuation controls
- topology-linked fake recursive graph readbacks and fake scheduler wrapper
- `:dag` TUI browser and operator controls
- artifact list/detail/load-more and bounded inline/database-owned preview
- fixture-backed smoke workflow for copied or generated DBs
- ordered `make dev-*-live-dogfood` developer workflow that wraps the fixture,
  daemon, live scheduler control gate, TUI, Claude login, and output commit

The not-ready surface is:

- background recursive scheduling or recovery loops
- topology live delegation
- provider/model-backed recursive master-implement loops
- task-runtime cancellation
- safe local file or URI opening from artifact rows
- trusted typed diff hunk text

Ordinary live scheduler reachability is disabled by default and must be enabled
explicitly. `RunRecursiveLiveScheduler` is gated by
`recursive_dag_live_scheduler_control_enabled`. The manual
`CommitRecursiveLiveAttemptOutput` path is gated by the same flag, requires the
linked provider session to be `Completed`, and never launches a provider
session. Background scheduling and topology live delegation remain unavailable.

## Safety Rules

- Use fake/manual recursive DAG flows by default.
- Enable the ordinary live scheduler only for an intentional bounded run; do not
  enable standalone live executor or background loop config.
- Do not run recursive DAG smoke or dogfood flows against the production DB
  unless you are intentionally testing production state.
- Prefer a generated fixture home or copied DB home.
- Use `RSI_SMOKE_SUPPRESS_RETRY_RESTORE=true` for copied DB smoke runs so
  retryable ordinary sessions do not relaunch provider processes.
- Use `RSI_TUI_NO_AUTO_START_DAEMON=true` when you need the TUI to stay
  disconnected for smoke checks.
- Artifact preview reads only inline/database-owned content. Path, URI, and
  external-source artifacts are blocked with structured unavailable metadata.

## Ordered Make Workflow For Live Dogfood

For the common bounded ordinary live dogfood loop there is an ordered set of
Make targets that wrap the fixture generator, daemon, gate, TUI, Claude login,
commit, and status steps. They all delegate to
`scripts/recursive-dag-live-dogfood.sh` and store environment in
`target/recursive-dag-live-dogfood.env`, so each terminal picks up the same
temp fixture home, socket, and live dogfood graph id.

The high-level ordered targets are:

```bash
# terminal 1: create a fresh temp fixture and start rsid against it
make dev-rsid-live-dogfood        # alias: make dev-rside-live-dogfood

# terminal 2: log Claude into the temp fixture HOME, only when needed
make dev-claude-live-dogfood-login

# terminal 2: enable only the live scheduler control gate, print status,
#             then start rsi against the same fixture
make dev-tui-live-dogfood

# after the linked provider session reaches Completed in :dag:
make dev-commit-live-dogfood      # commit latest live attempt, then print status

# any time: print caps, graphs, and live attempts for the fixture
make dev-status-live-dogfood
```

Inside the TUI, select the live dogfood graph, press `Enter` to hydrate detail,
press uppercase `L`, enter `1`, and press `Enter` to launch one bounded live
attempt. Wait for the linked provider session to reach `Completed`, then run
`make dev-commit-live-dogfood` to validate and commit the output.

These targets only enable `recursive_dag_live_scheduler_control_enabled`. They
do not enable `recursive_dag_live_executor_enabled`, do not enable
`recursive_dag_background_loop_enabled`, do not make live execution reachable by
default, and do not add topology-linked live delegation. The lower-level
`make recursive-dag-live-dogfood-*` targets expose the same steps individually
(`setup`, `env`, `daemon`, `gate`, `tui`, `claude-login`, `status`, `commit`,
`clean`); run `make help` for the full list. Removing the fixture requires
`CONFIRM=1 make recursive-dag-live-dogfood-clean`.

## Quick Start With Fixture Data

Build the daemon, TUI, RPC helper, and fixture generator:

```bash
# Disk-backed scratch target — never /tmp (tmpfs with usrquota; big cargo
# target trees there blow the quota and fail all writes with EDQUOT).
export CARGO_TARGET_DIR="$HOME/.rsi/tmp/cargo-targets/rsi-target"
export TARGET_DIR="$CARGO_TARGET_DIR"

cargo build -p rsid -p rsi
cargo build -p rsi-common --bin rsi-rpc
cargo build -p rsid --features dev-fixtures --bin rsi-recursive-dag-smoke-fixture
```

Create a fresh fixture home:

```bash
RSI_FIXTURE_HOME="$(mktemp -d /tmp/rsi-recursive-dag-fixture.XXXXXX)"

"$TARGET_DIR/debug/rsi-recursive-dag-smoke-fixture" \
  --output-home "$RSI_FIXTURE_HOME"
```

For intentional ordinary live dogfood, add the explicit live graph seed flag.
This creates one ordinary `live_session` graph in the fixture DB, but it does
not enable live scheduler control, the standalone live executor, or the
background loop:

```bash
"$TARGET_DIR/debug/rsi-recursive-dag-smoke-fixture" \
  --output-home "$RSI_FIXTURE_HOME" \
  --include-live-dogfood-graph
```

Create a copied fixture home from the generated DB. This exercises the same
copied-DB path used for smoke testing:

```bash
RSI_EXISTING_HOME="$(mktemp -d /tmp/rsi-recursive-dag-existing.XXXXXX)"

"$TARGET_DIR/debug/rsi-recursive-dag-smoke-fixture" \
  --source-db "$RSI_FIXTURE_HOME/.rsi/rsi.db" \
  --output-home "$RSI_EXISTING_HOME" \
  --include-live-dogfood-graph
```

The fixture generator writes a summary file:

```text
$RSI_EXISTING_HOME/recursive-dag-smoke-fixture.json
```

That JSON contains the generated graph, run, artifact, validation, topology,
cancellation, and recovery IDs needed for smoke testing. When the live dogfood
flag is used, the ordinary live graph is recorded as
`.ids.live_dogfood_graph_id`.

## Starting The Daemon

Start `rsid` against the copied fixture home:

```bash
RSI_EXISTING_SOCKET="$RSI_EXISTING_HOME/.rsi/daemon.sock"

HOME="$RSI_EXISTING_HOME" \
RSI_DAEMON_SOCKET_PATH="$RSI_EXISTING_SOCKET" \
RSI_SMOKE_SUPPRESS_RETRY_RESTORE=true \
RSI_RECURSIVE_DAG_STARTUP_RECOVERY_MAX_GRAPHS=1 \
"$TARGET_DIR/debug/rsid"
```

The recovery graph budget is intentionally low in smoke runs so deferred
recovery rows remain visible for manual recovery continuation tests.

Check daemon health:

```bash
"$TARGET_DIR/debug/rsi-rpc" \
  --socket "$RSI_EXISTING_SOCKET" \
  GetHealthStatus
```

## Enabling Fake/Manual Controls

The TUI renders controls only when the daemon advertises the corresponding
capability and runtime config allows the operation. Enable fake scheduler,
cancellation, and recovery controls in the fixture daemon:

```bash
"$TARGET_DIR/debug/rsi-rpc" --socket "$RSI_EXISTING_SOCKET" \
  UpdateDaemonConfig \
  --params '{"field":"recursive_dag_scheduler_controls_enabled","value":true}'

"$TARGET_DIR/debug/rsi-rpc" --socket "$RSI_EXISTING_SOCKET" \
  UpdateDaemonConfig \
  --params '{"field":"recursive_dag_cancellation_controls_enabled","value":true}'

"$TARGET_DIR/debug/rsi-rpc" --socket "$RSI_EXISTING_SOCKET" \
  UpdateDaemonConfig \
  --params '{"field":"recursive_dag_recovery_controls_enabled","value":true}'
```

Enable the ordinary live scheduler only when you intend to launch a bounded live
session from an ordinary recursive DAG graph. Each request is still
one-live-launch-at-a-time: after launching a provider session, the scheduler
stops at the live launch boundary and output validation/commit remains a
separate step.

```bash
"$TARGET_DIR/debug/rsi-rpc" --socket "$RSI_EXISTING_SOCKET" \
  UpdateDaemonConfig \
  --params '{"field":"recursive_dag_live_scheduler_control_enabled","value":true}'
```

Standalone live executor and background loop attempts should still fail:

```bash

"$TARGET_DIR/debug/rsi-rpc" --socket "$RSI_EXISTING_SOCKET" \
  UpdateDaemonConfig \
  --params '{"field":"recursive_dag_live_executor_enabled","value":true}'

"$TARGET_DIR/debug/rsi-rpc" --socket "$RSI_EXISTING_SOCKET" \
  UpdateDaemonConfig \
  --params '{"field":"recursive_dag_background_loop_enabled","value":true}'
```

## Read-Only Recursive Graph Rendering In `gv`

The `gv` graph overlay can render a recursive task graph's **structure**
read-only by bridging it to a workflow definition. This surface is gated on the
`gv_render_recursive_origin` daemon capability (default **false**). Enable it
durably (the setting is persisted to SQLite and survives daemon restart):

```bash
"$TARGET_DIR/debug/rsi-rpc" --socket "$RSI_EXISTING_SOCKET" \
  UpdateDaemonConfig \
  --params '{"field":"gv_render_recursive_origin","value":true}'
```

Once enabled, `GetDaemonCapabilities` returns `gv_render_recursive_origin:
true`, and the new `GetRecursiveGraphAsWorkflow` RPC (params `{"graph_id": …}`)
returns the bridged definition. In the TUI, open `gv`, press `R` to open the
"Recursive graphs" picker, and select a graph. The graph renders on the
unchanged Sugiyama canvas with a `recursive (read-only)` title badge; the node
detail panel shows `[read-only]`, and every mutating key (`d`/`x`/`i`/`I`/`u`,
and `r`) is inert — the bridged draft is held in memory only and is never
persisted as a workflow row, nor executed. This is a structure-only view; live
execution/attempt state is not reflected. The inspection capability is **not**
required for this surface; reachability depends on `gv_render_recursive_origin`
alone. With the capability off, the `R` key is a no-op and the picker source is
absent.

## Opening The TUI

Launch `rsi` against the fixture daemon:

```bash
HOME="$RSI_EXISTING_HOME" \
RSI_DAEMON_SOCKET_PATH="$RSI_EXISTING_SOCKET" \
RSI_TUI_NO_AUTO_START_DAEMON=true \
TERM=xterm-256color \
"$TARGET_DIR/debug/rsi"
```

Open the recursive DAG browser:

```text
:dag
```

There are no global recursive DAG keybindings yet. Use `:dag` to enter the
browser.

## TUI Browser Basics

Inside `:dag`:

| Key                      | Action                                                                   |
| ------------------------ | ------------------------------------------------------------------------ |
| `Tab` / `l` / Right      | Next panel                                                               |
| `Shift+Tab` / `h` / Left | Previous panel                                                           |
| `j` / Down               | Next row                                                                 |
| `k` / Up                 | Previous row                                                             |
| `g`                      | First row                                                                |
| `G`                      | Last row                                                                 |
| `Enter`                  | Load selected graph, open selected inspector row, or load more artifacts |
| `r`                      | Refresh selected graph                                                   |
| `!`                      | Show control gate details                                                |
| `Esc` / `q`              | Close inspector or browser                                               |

See `docs/keybindings.md` for the complete keybinding table.

## Running The Fake Scheduler From TUI

1. Open `:dag`.
2. Select the fixture manual fake scheduler graph.
3. Press `R`.
4. Enter a positive digit-only `max_steps` value, for example `1`.
5. Press `Enter`.

Expected result:

```text
FAKE scheduler completed: run=<uuid> status=Completed steps=1/1
```

The TUI requires explicit `max_steps`. Empty, zero, non-digit, and invalid input
is rejected before the RPC call. The TUI calls `RunRecursiveFakeScheduler`, not
the live scheduler.

## Running The Ordinary Live Scheduler

Use the ordinary live scheduler only on an isolated DB or intentional dogfood
state after enabling `recursive_dag_live_scheduler_control_enabled`. The
operator must supply an explicit positive `max_steps`; the TUI `L` prompt and
the `RunRecursiveLiveScheduler` RPC both reject unbounded runs.

The current live scheduler is request-scoped. It launches at most one provider
session for a graph per scheduler request, then stops with the launch-boundary
warning. After the linked provider session completes, commit its final
assistant output through `CommitRecursiveLiveAttemptOutput` before running the
scheduler again for a dependent task.

The launched provider session is told that it is already the live session for
the selected `recursive_live_attempt_id`. It must not create or inspect a
separate fixture DB, and its final assistant message must be only one valid
`RecursiveLiveOutputEnvelope` JSON object. For success, the envelope must copy
each task acceptance criterion exactly into the acceptance checks and include a
non-empty `tests` array. The `tests` array must carry either real test evidence
or one explicit not-run record, for example
`{"status":"not_run","required":false,"reason":"<why no test was run>"}`. A
success envelope with an empty `tests` array fails validation with
`missing_required_field` at `/tests`, which fails the live attempt, task, and
graph. The live launch prompt now states this requirement explicitly.

Fixture-backed operator flow:

1. Generate or upgrade the isolated fixture with
   `--include-live-dogfood-graph`.
2. Read `.ids.live_dogfood_graph_id` from
   `$RSI_EXISTING_HOME/recursive-dag-smoke-fixture.json`.
3. Start the daemon against that isolated home and enable only
   `recursive_dag_live_scheduler_control_enabled=true`.
4. Confirm `recursive_dag_live_scheduler_control=true` and
   `recursive_dag_live_execution=true` in `GetDaemonCapabilities`, while
   `recursive_dag_background_loop=false` in capabilities and
   `recursive_dag_live_executor_enabled=false` in `GetDaemonConfig`.
5. Open `:dag`, select the graph whose ID matches
   `.ids.live_dogfood_graph_id`, and press `Enter` to hydrate detail.
6. Press uppercase `L`, enter `1`, and press `Enter`.
7. Wait for the linked provider session to reach `Completed`, then commit the
   live output:

   ```bash
   "$TARGET_DIR/debug/rsi-rpc" \
     --socket "$RSI_EXISTING_SOCKET" \
     CommitRecursiveLiveAttemptOutput \
     --params '{"live_attempt_id":"LIVE_ATTEMPT_UUID"}'
   ```
8. Refresh `:dag` with `r`.

Expected first-boundary result: `RunRecursiveLiveScheduler` is called with
`max_steps=1`, reaches at most one provider-session launch for that ordinary
`live_session` graph, and then stops at the request-scoped launch boundary.
Expected completion result after `CommitRecursiveLiveAttemptOutput`: one live
validation is recorded, `live_active=0`, the task is `Succeeded`, and the graph
is terminal under the existing recursive DAG state model. Invalid JSON is
recorded as a validation failure and maps through the existing store state
transition rules instead of launching or repairing another session.

Automated dogfood coverage includes:

- one-task live smoke: launch, capture assistant output, validate/commit, and
  prove restart recovery does not duplicate validation
- two-task ordinary graph smoke: root task plus dependent task edge, two
  sequential live launches, validation/commit for both tasks, dependency
  readback, terminal no-duplicate rerun, and topology row/link no-mutation

Topology-linked graphs remain rejected by `RunRecursiveLiveScheduler`; topology
live delegation/T6 is not implemented.

## Cancelling Graphs And Runs

Graph cancellation:

1. Select a cancellable graph.
2. Press uppercase `C`.
3. Press `g`.
4. Enter a nonempty reason, 240 characters or fewer.
5. Press `Enter`.

Run cancellation:

1. Select a cancellable scheduler run.
2. Press uppercase `C`.
3. Press `r`.
4. Enter a nonempty reason, 240 characters or fewer.
5. Press `Enter`.

`C t` is reserved and disabled. Task-scoped runtime cancellation is not
executable yet.

The TUI sends `requested_by = "rsi-tui"` and refreshes readbacks after the
control RPC returns.

## Continuing Recovery

Manual recovery continuation is lower-case `c`.

1. Open `:dag`.
2. Confirm deferred recovery work is visible.
3. Press `c`.
4. Enter positive `max_graphs`.
5. Optionally enter digit-only `time_budget_ms`; `0` is allowed.
6. Press `Enter`.

The TUI calls `ContinueRecursiveRecovery` only when deferred recovery is
visible.

## Inspecting Artifacts And Validation

The fixture contains enough artifacts for pagination and preview checks.

Inside `:dag`:

- Use the artifact panel to select artifact rows.
- Press `Enter` on a normal artifact row to open its inspector.
- Press `Enter` on the load-more row to request the next artifact summary page.
- Press `p` inside an artifact inspector to call bounded artifact preview.
- Press `m` for metadata.
- Press `l` for links.
- Press `t` to show typed test detail unavailable state.
- Press `d` to show typed diff detail unavailable state.
- Use `]a` and `[a` to move between artifact inspector rows.

Preview behavior:

- Inline/database-owned artifacts can render bounded text.
- URI, file, path, external, and unsupported artifacts render unavailable
  metadata and do not read local content.
- Preview responses are scoped by graph and artifact ID. Stale responses are
  dropped.

Validation rows and issues are available through the validation inspector
readbacks. Typed test, diff, and scheduler-report inspector details are still
capability-disabled until future typed RPC handlers are implemented.

## Topology Fake Workflow

The fixture includes a topology-linked recursive fake graph.

Use RPC readbacks to inspect topology-recursive state:

```bash
"$TARGET_DIR/debug/rsi-rpc" --socket "$RSI_EXISTING_SOCKET" \
  ListRecursiveGraphsForTopology \
  --params '{"topology_id":"TOPOLOGY_UUID"}'

"$TARGET_DIR/debug/rsi-rpc" --socket "$RSI_EXISTING_SOCKET" \
  GetTopologyRecursiveStatus \
  --params '{"topology_id":"TOPOLOGY_UUID"}'
```

Run the topology fake scheduler wrapper with explicit fake mode only:

```bash
"$TARGET_DIR/debug/rsi-rpc" --socket "$RSI_EXISTING_SOCKET" \
  RunRecursiveTopologyNodeFakeScheduler \
  --params '{"graph_id":"GRAPH_UUID","max_steps":1,"operator":"smoke","execution_mode":"fake"}'
```

`execution_mode = "live_session"` must be rejected.

## Proving Live Scheduler Gate Behavior

By default these capability values should remain false:

- `recursive_dag_live_scheduler_control`
- `recursive_dag_live_execution`
- `recursive_dag_background_loop`

After setting `recursive_dag_live_scheduler_control_enabled=true`,
`recursive_dag_live_scheduler_control` and `recursive_dag_live_execution` should
report true while `recursive_dag_background_loop` remains false.

These config values should remain false, except
`recursive_dag_live_scheduler_control_enabled` when intentionally enabled and
`recursive_dag_fake_executor_only` which should remain true:

- `recursive_dag_live_executor_enabled=false`
- `recursive_dag_background_loop_enabled=false`
- `recursive_dag_fake_executor_only=true`

`RunRecursiveLiveScheduler` should return a disabled-gate error until
`recursive_dag_live_scheduler_control_enabled=true`.
`CommitRecursiveLiveAttemptOutput` should return the same disabled-gate error
until that gate is enabled, and should return a not-Completed error when the
linked provider session has not finished. Fake scheduler RPCs must still reject
`execution_mode = "live_session"`.

## Fixture Generator Safety

The fixture generator:

- is available only with `--features dev-fixtures`
- refuses default/production DB paths
- writes to generated or copied output homes
- copies source DBs read-only and verifies checksums
- uses Store APIs and fake scheduler paths
- does not launch providers, models, or sessions
- writes a summary JSON with generated IDs

Do not use ad hoc SQL to create recursive DAG fixture rows. SQL is acceptable
for read-only diagnostics and backup verification.

## Smoke Checklist

The full smoke checklist lives at:

```text
thoughts/shared/verification/2026-05-27-recursive-dag-operator-smoke-checklist.md
```

Run it before declaring a new recursive DAG dogfood build ready. For live
dogfood readiness, also run the focused ordinary live scheduler smokes in the
automated verification section of the current slice ledger. The ordered
`make dev-*-live-dogfood` targets in
[Ordered Make Workflow For Live Dogfood](#ordered-make-workflow-for-live-dogfood)
drive the manual bounded live dogfood loop end to end.

## Troubleshooting

### The TUI says the daemon is disconnected

Confirm `RSI_DAEMON_SOCKET_PATH` points at the daemon socket and that the TUI
was launched with the same `HOME` as the fixture daemon.

### The fake scheduler control is unavailable

Check daemon capabilities and runtime config. The fake scheduler needs
recursive DAG inspection plus scheduler control enabled. Enable:

```bash
"$TARGET_DIR/debug/rsi-rpc" --socket "$RSI_EXISTING_SOCKET" \
  UpdateDaemonConfig \
  --params '{"field":"recursive_dag_scheduler_controls_enabled","value":true}'
```

### Cancellation or recovery controls are unavailable

Enable the corresponding gates:

```bash
"$TARGET_DIR/debug/rsi-rpc" --socket "$RSI_EXISTING_SOCKET" \
  UpdateDaemonConfig \
  --params '{"field":"recursive_dag_cancellation_controls_enabled","value":true}'

"$TARGET_DIR/debug/rsi-rpc" --socket "$RSI_EXISTING_SOCKET" \
  UpdateDaemonConfig \
  --params '{"field":"recursive_dag_recovery_controls_enabled","value":true}'
```

### Artifact preview is unavailable

Preview is available only for inline/database-owned content. URI, file, path,
external, unsupported, and malformed artifacts intentionally do not expose local
content.

### Copied DB startup launches old sessions

Use:

```bash
RSI_SMOKE_SUPPRESS_RETRY_RESTORE=true
```

This is for smoke/copy runs. Default production retry behavior is unchanged.

### The TUI auto-starts a daemon during disconnected checks

Use:

```bash
RSI_TUI_NO_AUTO_START_DAEMON=true
```

Normal TUI startup behavior is unchanged when this variable is absent.

## Next Work

The main remaining work is broader live recursive execution beyond the bounded
ordinary smoke path. That should continue through gated slices:

1. richer operator live UX and ergonomics
2. cancellation, interrupt, heartbeat, and budget enforcement in live runs
3. background scheduling/recovery policy
4. provider/model-backed recursive master-implement loops
5. only then topology live delegation

Until those gates are complete, keep live runs bounded, explicit, ordinary-graph
only, and separate from topology live delegation.
