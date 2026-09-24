# Read-only fan-out audit runbook

**Author:** Codex implementation agent, 2026-09-23<br>
**Intended reviewer model:** Claude Opus 5.5 (different model family)<br>
**Definition:** [readonly-fanout-audit.json](topologies/readonly-fanout-audit.json)

## Purpose

Run four parallel read-only audits of four caller-supplied repository areas
using OpenRouter, then synthesize the four handoffs in a Codex gpt-6-luna
session. The summarizer uses `custody.from` to select a deterministic fork
source, receives the auditors' full handoff content, and writes no files.

## Inputs

Supply four distinct, bounded areas in the topology execution input object:

```json
{
  "area_1": "authentication token refresh in crates/rsid/src/auth",
  "area_2": "session launch validation in crates/rsid/src/session",
  "area_3": "topology custody and retry behavior",
  "area_4": "related regression tests and error handling"
}
```

Each auditor receives this input map and is instructed to inspect only its
corresponding key. The two configured OpenRouter models are
`deepseek/deepseek-v4.1-flash` and `z-ai/glm-5.3-flashx`; all model and effort
fields are explicit. The four parallel sessions are within the design's
OpenRouter fan-out policy.

## Upsert and execute

Today, topology writes are operator RPCs; agent topology verbs are a T4
follow-up. Start from the repository root:

```bash
rpc CreateTopology --params "$(jq -c '{name,definition}' docs/topology-on-epic/topologies/readonly-fanout-audit.json)"
rpc ExecuteTopology --params '{"topology_id":"<TOPOLOGY_UUID>","project_id":null,"inputs":{"area_1":"...","area_2":"...","area_3":"...","area_4":"..."},"parent_id":"<EPIC_UUID>"}'
```

Replace the sample values with actual areas. Record the returned topology and
execution UUIDs with the result. For an existing topology, use `UpdateTopology`
with its UUID and the file's `definition` rather than creating a duplicate name.

## Read results and recover

Poll `GetWorkflowExecution` with the execution UUID. The four audit sessions
must settle with `PIPELINE HANDOFF` status `complete` before the summary node
runs. The summary handoff carries the combined report. If a session is blocked
with preserved work, use `:topology-resolve <execution_id> inspect` and follow
the §3.4 resolution rules before retrying or accepting work.

```bash
rpc GetWorkflowExecution --params '{"execution_id":"<EXECUTION_UUID>"}'
```

## Expected cost and duration

Expect five model calls: four OpenRouter audits and one Codex summary. Budget
roughly 3–8 minutes per auditor and 2–5 minutes for synthesis for bounded areas;
the four audits run in parallel, subject to provider availability and launch
policy. Cost is token-dependent and billed at the configured OpenRouter and
Codex rates; bound each area and its requested evidence to keep the total small.

## Failure modes

- Missing or vague input areas produce weak or mismatched findings; provide four
  distinct paths or questions with clear boundaries.
- OpenRouter model access, rate limits, or unsupported effort settings can fail
  an auditor launch.
- A non-complete or malformed handoff blocks that audit and therefore the
  summarizer.
- Auditor findings are model output and require human interpretation before
  acting on them.
- The model-family classifier cannot classify the DeepSeek and Z.ai model IDs
  behind OpenRouter today. The definition explicitly selects a Codex/OpenAI
  summarizer, but the static validator cannot prove that its vendor family is
  different from those unclassified OpenRouter models.
- The operator `CreateTopology` flow has no T4 `allowed_launches` policy
  context; that model-triple policy is enforced only on the future agent upsert
  and execute surfaces.

## What v1 does not enforce

Read-only behavior is prompt policy only. v1 does not sandbox session tools into
a read-only mode and does not prevent an auditor from editing or committing;
the executor's clean-tree and `expects_commit: false` checks detect many such
changes only after a session ends. Session nodes keep the launcher's
credentials, and the executor does not refuse `git push`. See plan §V, #645.

## Stage contract

### Inputs

- Static: this runbook, the stored definition, plan §V, §3, §4, §5.3, and §7 T5c.
- Discovery budget: inspect the four supplied areas and their directly related
  tests.

### Process

- Upsert the JSON definition, execute it with four area values, and poll until
  the audits and summary settle.

### Outputs

- Four read-only audit handoffs and one synthesized `PIPELINE HANDOFF`.
- Record the live execution UUID and observed completion in the run record.

### Verify

- Confirm four audit nodes received their matching input keys and the summary
  includes evidence for each area.
- This fixture's static acceptance is covered by
  `stored_topology_automation_definitions_validate`.
- No live execution UUID is recorded here: this author session has no
  `ExecuteTopology` control verb. Record the first operator-run execution UUID
  and settlement here after live acceptance.
