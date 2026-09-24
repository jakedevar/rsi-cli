# Rolling health baseline runbook

**Author:** Codex implementation agent, 2026-09-23<br>
**Intended reviewer model:** Claude Opus 5.5 (different model family)<br>
**Definition:** [rolling-health-baseline.json](topologies/rolling-health-baseline.json)

## Purpose

Run the daemon catalog's `rolling_baseline` checks against the execution base
(resolved from `origin/rolling`), gate on the typed overall exit code, then have
a short Codex session report each crate's check, clippy, and library-test
result on either outcome. The command node is daemon-owned and the definition
stays within the build-node concurrency cap.

## Inputs

No custom inputs are required. Execute from the repository whose `origin/rolling`
is the intended baseline. The definition names the catalog default crate set
explicitly: `rsi-common`, `rsid`, and `rsi` (3 crates, within the catalog's
8-crate limit). The catalog runs check, clippy, and `cargo test --lib` for each
crate, with its daemon-owned offline settings and `--test-threads=8`.

## Upsert and execute

Today, topology writes are operator RPCs; agent topology verbs are a T4
follow-up. Start from the repository root:

```bash
rpc CreateTopology --params "$(jq -c '{name,definition}' docs/topology-on-epic/topologies/rolling-health-baseline.json)"
rpc ExecuteTopology --params '{"topology_id":"<TOPOLOGY_UUID>","project_id":null,"inputs":null,"parent_id":"<EPIC_UUID>"}'
```

Record the returned topology and execution UUIDs with the operational result.
The first command validates and stores the definition; `ExecuteTopology` resolves
the rolling base and creates the durable execution. For an existing topology,
use `UpdateTopology` with its UUID and the file's `definition` rather than
creating a duplicate name.

## Read results and recover

Poll `GetWorkflowExecution` with the execution UUID. A pass or failure report
includes the catalog's typed `report` with a row for every check, the gate
result, and the report session's `PIPELINE HANDOFF`. The report session forks
from the command's result commit on success and its fork commit on routed
failure. If the execution is blocked with preserved work, inspect it
in the TUI using `:topology-resolve <execution_id> inspect`; use `accept`,
`retry`, or `discard` only after reviewing the preserved commit and the
resolution rules in §3.4 of the design plan.

```bash
rpc GetWorkflowExecution --params '{"execution_id":"<EXECUTION_UUID>"}'
```

The `completed` routes from the command send both successful and routed
terminal-failure outputs to the gate and report. The gate can read the recorded
command `exit_code` on either outcome; the report receives the typed command
fields and, for a routed failure, `failure_class` and `error` as well. A zero
exit code makes the gate true; a nonzero exit code makes it false while still
allowing the report session to describe the failed checks.

## Expected cost and duration

There is no model call for the command. The report is one gpt-6-luna session.
The baseline runs up to nine serial Cargo invocations; allow roughly 15–60
minutes on a cold cache, with shorter runs when the sandbox has a warm build
cache. Actual duration depends on host load and dependency state. Daemon command
concurrency is bounded by `topology_max_concurrent_build_nodes` (default 2).

## Failure modes

- A nonzero check fails the command node; its failure is routed to the gate
  and report with the recorded catalog output. Read the failed crate/check in
  the report and the retained typed output.
- Invalid or unavailable crate names fail catalog validation or execution.
- Offline dependency gaps or a command timeout fail the command node.
- A catalog command that changes tracked repository state is detected after the
  fact and blocks the execution as preserved work.
- Provider launch policy or unavailable gpt-6-luna access can prevent the final
  report session from completing.

## What v1 does not enforce

Catalog operations run repository code (`build.rs`, tests) as the daemon user;
there is no jail, network isolation, or host-socket isolation. Writes inside the
sandbox outside `target/` and `.rsi-tmp/` are detected after the fact, while
writes outside the sandbox are neither prevented nor detected. Replay safety
covers sandbox state only. Session nodes keep the launcher's credentials, and
the executor does not refuse `git push`. These limits are described in plan §V
and tracked by #645.

## Stage contract

### Inputs

- Static: this runbook, the stored definition, plan §V, §3, §3.2, §4, and §7 T5b.
- Discovery budget: inspect the target repository's `origin/rolling` and
  `GetWorkflowExecution` result.

### Process

- Upsert the JSON definition, execute it under the target Epic, and poll the
  durable execution to settlement.

### Outputs

- One typed per-crate command report and a concise `PIPELINE HANDOFF` report.
- Record the live execution UUID and observed completion in the run record.

### Verify

- Confirm the gate reflects the overall `rolling_baseline.exit_code` and the
  report accounts for all three crates and all three checks per crate on both
  success and routed terminal failure.
- No live execution UUID is recorded here: this author session has no
  `ExecuteTopology` control verb. Record the first operator-run execution UUID
  and settlement here after live acceptance.
- This fixture's static acceptance is covered by
  `stored_topology_automation_definitions_validate`.
