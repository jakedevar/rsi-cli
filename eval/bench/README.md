# Pinned benchmarks (`eval/bench/`)

Pinned external benchmark definitions, separate from the replay corpus in `eval/corpus`.

## `eval/bench/tb2/`: Terminal-Bench 2.0 (#3175, program #3135)

One committed, drift-checked definition of Terminal-Bench 2.0: the 89 tasks of
`harbor-framework/terminal-bench-2` at commit `2fd12b88aafdd04a52c298e3940bcb189f9766d6`,
run by Harbor 0.24.0 (`harbor run -p <checkout> -i <task>...`). Python 3.11+, stdlib only,
no network except where noted.

| File | Purpose |
| --- | --- |
| `dataset.toml` | Repo, commit, Harbor version. Per task: difficulty, category, cpus, memory_mb, agent and verifier timeouts, image tag, linux/amd64 manifest digest, compressed image bytes, `task.toml` sha256. Also the shard plan. |
| `split.py`, `split.json` | Seeded split (seed 3135, algorithm `tb2-split-v1`, plan appendix A): `search` 30 (1 easy, 19 medium, 10 hard), `held_out` 59, `smoke` 5 and `calibration` 10 (both subsets of `search`). `python3 split.py` exits 1 if `split.json` differs from the regenerated split. |
| `pin_check.py` | `pin_check.py CHECKOUT [--images] [--task NAME]`: exits 1 and prints one `DRIFT` line per changed task directory, `task.toml`, checkout commit or (with `--images`, on a docker host) pulled image whose RepoDigests lack the pinned digest. The runner (S5) calls it before every run. |
| `leak_audit.py` | `leak_audit.py --checkout CHECKOUT (--diff FILE\|- \| --files PATH...) [--terms held_out\|search\|all]`: reports held-out task names, test file names and expected-output literals in added diff lines or files. Paths are normalised to repo-relative; only `dataset.toml` and `split.json` are excluded and `candidates/` is always audited. A candidate directory audits its overlay, instructions and `notes.md`. `--terms search` reports search-task names and search-only test file names as `GENERALITY` lines (overfit to the search split). Exits 1 on a hit. M6 runs it on every candidate. |
| `refresh_dataset.py` | Re-pin only: rebuilds `dataset.toml` from a checkout and `docker manifest inspect -v` (needs network). |

Tuning (M6) uses only the `search` split; `held_out` validates a chosen candidate.
Images are 49.0 GB compressed, so `dataset.toml` carries a shard plan: shards hold at most
12 GB of compressed images, never mix `search` and `held_out`, and each image of 4 GB or more
(`mteb-leaderboard`, `mteb-retrieve`, `hf-model-inference`, `pytorch-model-recovery`) is a shard
of its own. Pull a shard, run it, then prune its images before the next.

Tests: `python3 -m unittest discover -s eval/bench/tb2/tests` (also run by
`scripts/tests/test_eval_bench_tb2.py` under `scripts/scoped-test`).

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
  split, image digests, Harbor version, adapter tree hash, harness build digest, model, effort,
  route, sampling, timeouts and multiplier, and k. A stored record is reused only on an identical
  fingerprint (`rsi-eval bench lookup <dir> <fingerprint>`).
- `scan_solution_lookup` flags a trace with network access to `terminal-bench`, `tbench.ai`,
  `harbor-framework`, `laude-institute` or the task's upstream repository.

```
rsi-eval bench report <record.json>                      # markdown report
rsi-eval bench compare <a.json> <b.json> [--fail-on-regression]   # paired compare, exit 2 on regression
```
