# Pinned benchmarks (`eval/bench/`)

Pinned external benchmark definitions, separate from the replay corpus in `eval/corpus`.

## `eval/bench/tb2/`: Terminal-Bench 2.0 (#3175, program #3135)

One committed, drift-checked definition of Terminal-Bench 2.0: the 89 tasks of
`harbor-framework/terminal-bench-2` at commit `2fd12b88aafdd04a52c298e3940bcb189f9766d6`,
executed by the RSI Rust runner with Harbor 0.24.0 as the parity reference.
Definition tooling is native Rust in `rsi-eval`, with no network except where noted.

| File | Purpose |
| --- | --- |
| `dataset.toml` | Repo, commit, Harbor version. Per task: difficulty, category, cpus, memory_mb, agent and verifier timeouts, image tag, linux/amd64 manifest digest, compressed image bytes, `task.toml` sha256. Also the shard plan. |
| `rsi-eval bench tb2 split`, `split.json` | Seeded split (seed 3135, algorithm `tb2-split-v1`, plan appendix A): `search` 30 (1 easy, 19 medium, 10 hard), `held_out` 59, `smoke` 5 and `calibration` 10 (both subsets of `search`). Checks by default (also `--check`); exits 1 if `split.json` differs from the regenerated split. `--write` regenerates it. |
| `rsi-eval bench tb2 pin-check` | `pin-check CHECKOUT [--images] [--require-pulled] [--task NAME]`: exits 1 and prints one `DRIFT` line per changed task directory, `task.toml`, checkout commit or (with `--images`, on a docker host) pulled image whose RepoDigests lack the pinned digest. |
| `rsi-eval bench tb2 leak-audit` | `leak-audit [--checkout CHECKOUT] (--diff FILE\|- \| --files PATH...) [--terms held_out\|search\|all]`: checks held-out task names, unique test file names and expected-output literals in added diff lines or files. Findings contain file, line and kind, never the term or task. Paths are normalised to repo-relative; only `dataset.toml` and `split.json` are excluded and `candidates/` is always audited. A candidate directory audits its overlay, instructions and `notes.md`. `--terms search` reports search-task names and search-only test file names as `GENERALITY` lines. Exits 1 on a hit, 0 when clean, 2 on input errors. M6 candidate verification calls this audit in-process. |
| `rsi-eval bench tb2 refresh-dataset` | Re-pin only: `refresh-dataset CHECKOUT [--manifests JSON] [--out PATH]` rebuilds `dataset.toml` from a checkout and `docker manifest inspect -v` (needs network unless captured manifests are supplied). |

Tuning (M6) uses only the `search` split; `held_out` validates a chosen candidate.
Images are 49.0 GB compressed, so `dataset.toml` carries a shard plan: shards hold at most
12 GB of compressed images, never mix `search` and `held_out`, and each image of 4 GB or more
(`mteb-leaderboard`, `mteb-retrieve`, `hf-model-inference`, `pytorch-model-recovery`) is a shard
of its own. The runner pulls shard images by digest and preserves shared images and caches;
cleanup removes only its owned trial resources.

Run the commands from the repository root; `--dataset` and `--split` override the pin paths.
Tests live in `crates/rsi-eval/src/tb2/tests.rs` and `crates/rsi-eval/tests/tb2_cli.rs`,
selected by the required scoped-test job. They need no interpreter, Docker host or network.
The runner's preflight calls #3620's pin-check library directly. R7 (#3685) removes the former
Python transport, Harbor adapter package and tests, and the transitional Python pin checker and
library. `crates/rsi-eval/tests/tb2_retirement.rs` scans runtime, packaging and gate sources for
dependencies on those retired inputs. The daemon's required scoped-test and landing gates remain.
Unrelated gate tooling remains under the repository's transitional daemon-runner policy.

## Running the oracle with the Rust runner (#3681 R3b, program #3135 option B)

The `bench` job (`AgentSubmitJob {kind:"bench", params:{config, budget_usd}}`, manager or Epic lead)
launches the stamped `rsi-eval` binary installed beside `rsid` (`make release-install` builds and
installs it; a deploy carries it) as an owned systemd unit: `rsi-eval bench tb2 run --plan
<run>/plan.json`. No Python, Harbor, uv or venv is involved, and no model is launched by an oracle config.

- **Before the first run** the hub needs the pinned checkout at `~/.rsi/bench/tb2/<commit>` (a clone of
  `harbor-framework/terminal-bench-2` checked out at the commit `dataset.toml` pins), and the laptop needs
  Docker with Compose v2 reachable as `ssh://<bench.laptop_host>`. The four operator settings
  (`bench.enabled`, `bench.docker_hosts`, `bench.laptop_host`, `bench.max_concurrency`) are unchanged.
- **Admission** refuses with the existing typed codes. The runner build is checked first
  (`bench_preflight_failed: runner_unavailable` when `rsi-eval` is missing or its stamp is not
  self-consistent), then `rsi-eval bench tb2 preflight` (`bench_pin_check_failed` for drift,
  `bench_preflight_failed: <kind>: <detail>` for host, Docker, platform or capacity).
- **The result** is `result.bench` plus `~/.rsi/bench/<run>/record.json`, a `BenchRunRecord` v1 whose
  `run.harbor_version` is `rsi-tb2/1;parity=harbor-0.24.0` and whose `run.adapter_tree_hash` is the runner's
  stamped evaluator digest (`rsi-eval bench tb2 stamp`), so a Harbor record and a Rust record are never
  compared. An oracle trial has zero usage and cost and no session or trace hash. Per-attempt outcomes are in
  `~/.rsi/bench/<run>/trials/<task>-s<slot>-r<retry>/outcome.json`.
- **Smoke** (`eval/bench/tb2/runs/smoke-oracle.toml`): the five `smoke` tasks; expect five valid reward
  passes, `spent_micro_usd` 0, five recorded trials and nothing left running on the laptop.
- **Harness** (`kind = "rsi"`) uses the Rust client on the run-local socket (R4–R6), with the
  daemon retaining grant, spend-cap, route and session authority. This retirement authorizes no
  model launch or paid run.
- **R7 search receipt: pending manager execution.** After deploy and normal bench admission, the
  lead runs all 30 search tasks at k=1, records every valid/invalid outcome and unresolved failure,
  and attaches zero-session/zero-spend evidence. Code retirement alone establishes no oracle pass.
- **Harbor reference comparison: UNRUN.** No pinned historic Harbor receipts were supplied for
  comparison. Rust fixtures and oracle plumbing do not establish a head-to-head Harbor result.
  Do not execute Harbor to create receipts under the Python ban.

The hub command executor permits exactly `docker` and `ssh`; Rust regression tests assert that
the recorded synthetic oracle invocations cover that approved set. Python in a pinned official
task container is benchmark content and is unaffected by RSI runtime retirement.

## `BenchRunRecord` v1 and `rsi-eval bench` (#3179, program #3135)

Every published benchmark number comes from one versioned record,
`rsi_common::bench_record::BenchRunRecord` (alphabetical JSON keys, `schema_version` 1):
per-trial rows (reward, validity, failure cause, cost, tokens, image digest, trace hash,
suspected solution lookup) plus the run part (dataset pin, Harbor version, adapter tree hash,
harness build digest, model, effort, route, k, totals and the `observation_fingerprint`).

- Invalid trials (`reward_valid` false, `invalid_route`, `infra_interrupted`) are excluded from
  pass rates and counted separately.
- Pass rate = mean of per-task pass fractions; 95% Wilson interval for k=1, seeded task-clustered
  percentile bootstrap (10,000 resamples) for k>1. pass@k is the unbiased `1 - C(n-c,k)/C(n,k)`
  and is refused when a task has fewer than k valid trials.
- `observation_fingerprint` = sha256 of the canonical JSON of dataset repo and commit, tasks and
  split, image digests, Harbor version, adapter tree hash, harness build digest, harness source
  digest, the effective-input digests (prompt, tool spec, policy, loop knobs, overlay), model,
  effort, route, sampling, timeouts and multiplier, and k. A stored record is reused only on an
  identical fingerprint (`rsi-eval bench lookup <dir> <fingerprint>`).
- The rsid build SHA is provenance (`run.build_sha`, `trials[].build_sha`), not a fingerprint
  input (#3192): a hub redeploy that leaves the harness alone keeps an earlier baseline usable.
  `harness_source_digest` is a sha256 over the git tree or blob ids, at the build commit, of the
  paths in `crates/rsi-common/data/harness_source_paths.txt` (one reason per path); a change
  under a listed path changes it, a change elsewhere does not. `compare` refuses a pair that
  differs in anything but the build SHA (dataset pin, Harbor, adapter, model, effort, route,
  source digest, effective-input digests, per-task `harness_version_hash`) with a typed
  `NotComparable` error naming the first differing field. A deploy that changes the source digest
  mid-window needs a new baseline; ask the PM for a hub deploy freeze per benchmark window.
- `scan_solution_lookup` flags a trace with network access to `terminal-bench`, `tbench.ai`,
  `harbor-framework`, `laude-institute` or the task's upstream repository.

```
rsi-eval bench report <record.json>                      # markdown report
rsi-eval bench compare <a.json> <b.json> [--fail-on-regression]   # paired compare, exit 2 on regression
```

## `rsi-eval bench digest` (#3195, M6)

```
rsi-eval bench digest --record <record.json> --out eval/bench/tb2/candidates/<id>/digest/ \
    [--split eval/bench/tb2/split.json] [--bench-dir ~/.rsi/bench] [--run <run id>] [--trajectories <run or legacy job dir>]
```

Bounded, mechanical per-trial excerpts of a search-split run for the Meta-Harness proposer: one
`<task>.k<attempt>.md` per trial plus `summary.json`. A trial file holds the outcome fields, the
tool-call histogram, compaction events, the first error with 5 events before and after it, the last
30 events and the absolute trace path (for a deeper grep of that search trial only). Fields are cut at
2,000 characters; files at 16 KiB (failed), 4 KiB (passed) and 512 KiB per candidate; every cut is
marked in place. It reads `trace.jsonl` (found by the row's `trace_sha256`), else the ATIF
`trajectory.json`. Discovery supports Rust daemon traces at `<run>/<task>-s<slot>-r<retry>/trace.jsonl`
and collected runner artifacts at `<run>/trials/<task>-s<slot>-r<retry>/logs/agent/`, matching retries
by trace hash or trajectory session id. Legacy Harbor artifact directories and v1 records remain
readable; the legacy `harbor_version` and `adapter_tree_hash` field names now carry the Rust runtime
discriminator and evaluator digest described above. A record with any trial outside `split.json` `search` is refused (exit 4) before
anything is written, so held-out content never enters the candidate store. Deterministic; no network,
no model calls.

## Run overlays: Meta-Harness candidates (#3191, program #3135 M6)

A candidate is one committed overlay under `eval/bench/tb2/candidates/<id>/`: `overlay.toml`, an
optional instructions file and the run config `run.toml` that names it with
`overlay = "eval/bench/tb2/candidates/<id>/overlay.toml"`. The `bench` job reads both from its own
committed, clean HEAD and applies the overlay to every trial session of that run and to nothing
else: no rebuild, no daemon deploy and no landing on `rolling` per candidate, and no daemon setting
or stored session policy is written. A config at `candidates/<id>/run.toml` is accepted by the job
as well as `runs/*.toml`. The overlay agent kind must be `rsi` (an oracle run calls no model).

```toml
# eval/bench/tb2/candidates/c03-1a2b3c4d5e6f/overlay.toml: every field is optional
instructions = "instructions.md"      # replaces the editable body of the bench prompt (UTF-8, <= 64 KiB)
[tool_policy]
enabled_tools = ["shell", "exec_command", "write_stdin", "read_output", "compact"]
denied_tools = ["update_plan"]        # both can only narrow the bench allowlist
[context]
compact_budget_tokens = 96000         # 32000..=2000000
[loop]
max_iterations = 200                  # 10..=1000
[compaction]                           # each optional; unset keeps today's value (#3199)
message_trigger = 180                  # 40..=1000 messages
keep_recent = 20                       # 4..=100 messages kept verbatim, below message_trigger
window_percent = 75                    # 25..=95 share of the model window
summary_tokens = 4000                  # 500..=16000, default derived from the threshold
summary_instruction = "..."            # up to 4 KiB; {max_tokens} names the summary length
```

- The prompt's fixed header is always kept and cannot be overridden: `<role>`, `<container>`,
  `<integrity>` (no Terminal-Bench or Harbor sources, no solution lookup) and `<critical_rules>`.
  The instructions replace `<tools>`, `<working>` and `<finishing>`, in that place.
- `web_access`, `egress`, `context_editing`, `budgets` and `cli_access` are refused
  (`overlay_field_not_allowed`); the reserved table `reasoning_replay` is refused
  `overlay_field_not_supported` until it is wired. An unknown field, an out-of-range value
  (never clamped), a tool the bench allowlist does not hold and an instructions path that leaves
  the candidate directory are refused with a typed code (`bench_overlay_invalid: <code>: ...`).
- Overlay values are layered per launch on the run config's `[loop]`; the overlay wins.
- Identity: `harness_build_digest` and the `overlay` effective-input digest (hence
  `observation_fingerprint`) fold in the canonical overlay digest, the sha256 of each file it
  references and the effective rendered prompt digest. A run with no overlay and a run whose overlay
  sets nothing are the same run, and the default prompt is byte-identical to the pre-overlay one, so
  an M5 baseline stays comparable.
- Admission runs the native leak audit over the overlay and its files (held-out terms on every candidate
  file, search-split terms on the files a model reads, with the checkout's terms when the hub holds
  the pinned checkout) and refuses the run on any hit or failure to run
  (`bench_overlay_leak_audit_failed`).
- Dry run, no model call and nothing opened: `rsi-bench-resolve eval/bench/tb2/candidates/<id>/run.toml`
  (run from the worktree, or `--repo <dir>`) prints the effective prompt digest, tool list and knobs.

## Candidate store and `rsi-eval bench candidate` (#3194, program #3135 M6)

`eval/bench/tb2/candidates/<id>/` (`id` = `c<NN>-<first 12 hex of the overlay digest>`) holds `overlay.toml`,
an optional `instructions.md`, `run.toml` (search split, k=1, the M6 model and effort), `notes.md`
(headings `Parent`, `Hypothesis`, `Single change`, `Expected effect`, `Prior candidates cited`),
`records.json` (pointers to `eval/bench/tb2/records/<run>.json`), `verify.json` and `digest/`.
`c00-*` is the seed: an empty overlay. Offline, no model call:

```
rsi-eval bench candidate new --seed [--record eval/bench/tb2/records/<run>.json]
rsi-eval bench candidate new --parent <id> [--id-ordinal N]   # copies the parent's overlay; edit it
rsi-eval bench candidate rename <id>                          # move to the id the edited overlay names
rsi-eval bench candidate verify <id> [--proposer-events FILE] [--checkout DIR]   # writes verify.json, exit 1 on failure
rsi-eval bench candidate index                                # rebuild index.json (stable JSON, no timestamps)
```

`verify` checks the schema (the daemon's overlay type), the digest and id, `run.toml`, `notes.md`, the
parent, the `records.json` shape, the leak audit (held-out terms on every file, search terms on the
overlay and instructions only) and, with an `AgentReadSessionEvents` export, the read audit: tool
calls that touch `eval/bench/tb2/records/`, a `~/.rsi/bench/` run no `records.json` or digest names, or a
held-out task. Findings give file, line and kind, never the held-out term.

## Proposer skill and dry-run fixture (#3197, program #3135 M6)

`eval/bench/tb2/proposer/SKILL.md` is the committed skill of the Meta-Harness proposer: one candidate per
session, with the read set, the forbidden set, the write set, the `notes.md` template and the handoff
fields. `eval/bench/tb2/proposer/fixtures/` is a small made-up search-split record, its digests, the
traces they name (repository-relative paths) and imaginary screen outcomes with a `select` state, so the
$0 three-iteration dry run needs no calibration record. `crates/rsi-eval/src/proposer_fixture.rs` generates
and checks the fixture (`RSI_PROPOSER_FIXTURE_BLESS=1` rewrites it).

## Oracle findings (#3685, search split)

The $0 oracle run over the 30 search tasks (job `2aa4e94a`, 2026-10-23, config
`tb2/runs/oracle-search30.toml`) passed 27/30 with 30/30 valid trials, 0 model sessions and a
clean runner (30/30 collected and cleaned, no leftover `rsi-tb2` objects). The 3 failures are
task or laptop-environment limits, not runner bugs. They are oracle-unsolvable/environment-limited
and are left out of paid runs (`tb2/runs/search27-flash.toml` lists the other 27 explicitly;
`dataset.toml` and `split.json` stay unedited because they are generated and pin-checked):

| Task | Oracle outcome | Reason |
| --- | --- | --- |
| `caffe-cifar-10` | `agent_timeout` | the reference solution (a slow Caffe build on the laptop) ran past the agent deadline (1224 s wall) |
| `install-windows-3.11` | verifier 3/4 | the oracle ran; `test_qemu_running_with_correct_params` (QEMU/KVM environment check) fails on the laptop |
| `rstan-to-pystan` | verifier 1/6 | the reference solution crashes: `ModuleNotFoundError: pkg_resources` in `/opt/py310` (setuptools missing in the image) |

`sqlite-with-gcov` passed in both oracle runs (S10 job `ef0be3ba` and `2aa4e94a`); #3895 (its
verifier needing `releases.astral.sh`) was not reproduced and the task stays in.
