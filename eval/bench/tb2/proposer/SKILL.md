---
name: tb2-harness-proposer
description: One Meta-Harness proposer session for the RSI native harness on the Terminal-Bench 2.0 search split. Reads the committed candidate store and trace digests, writes exactly one new candidate overlay, never runs a bench job.
---

# TB2 harness proposer (#3197, program #3135 M6)

Method: Meta-Harness (arXiv 2603.28052). A proposer reads what earlier harness candidates did, forms one
hypothesis about what the model gets wrong, and commits one candidate. The lead evaluates it. Plan:
`thoughts/shared/plans/2026-10-19-issue-3135-m6-meta-harness-loop.md` (D1, D2, D9, D10).

## Role

You write **exactly one candidate** per session: one hypothesis, one change.

- Prefer **additive context** (an extra instruction, a fact the model lacks) over rewriting the prompt or
  changing the completion flow. In the paper's TB2 search six prompt or flow edits regressed and one additive
  environment snapshot won. A prompt-only environment survey is a good early candidate.
- Base the hypothesis on **evidence in the digests** (a repeated failing call, an assumed tool, a wrong path,
  an early finish, an iteration cap). Quote the digest file name in `notes.md`.
- Do not repeat a candidate the index already lists with a decision. `verify` refuses an overlay whose digest
  matches another candidate's.
- You never run a bench job, call a paid model, push, or edit another candidate.

## Read set (and nothing else)

1. `eval/bench/tb2/candidates/index.json`: ids, parents, status, gain, cost, decision.
2. Every `eval/bench/tb2/candidates/*/`: `overlay.toml`, `instructions.md`, `notes.md`, `digest/`
   (`summary.json` plus one `<task>.k<attempt>.md` per trial: outcome, tool histogram, first error with context,
   last 30 events, the trace path).
3. The default bench prompt body: `crates/rsid/assets/harness/bench_instructions.md`.
4. The overlay schema: `eval/bench/README.md`, sections "Run overlays" and "Candidate store".
5. The full trace a digest file names (`- trace: <path>`), for that **search** trial only. Grep it; do not cat it.

## Forbidden

- `eval/bench/tb2/records/` (committed run records hold held-out outcomes).
- Any `~/.rsi/bench/` path that no digest names; never list that directory.
- Held-out task names, instructions, tests or sources. Do not open `split.json` or `dataset.toml` (they list
  every task); the search tasks you may talk about are the ones in the digests.
- Terminal-Bench or Harbor sources, online or in a checkout.
- A bench job, a paid model call, or any model other than your own session.

The lead exports your tool calls (`AgentReadSessionEvents`) and runs `verify --proposer-events`. A call whose
text touches `records/`, an unlisted `~/.rsi/bench/` run, a bench directory listing or a held-out task makes
the candidate **ineligible**, and it cannot be undone. So name directories explicitly, and never run a recursive
`grep`, `find` or `git grep` over `eval/bench/tb2/` or `eval/bench/`.

## Write set

One new directory `eval/bench/tb2/candidates/<id>/`, made with the tool. Check `--help` rather than guessing flags.

```
rsi-eval bench candidate new --parent <id>        # copies the parent's overlay, instructions and notes template
# edit overlay.toml / instructions.md, then:
rsi-eval bench candidate rename <old-id>          # the id is c<NN>-<first 12 hex of the overlay digest>
# fill notes.md, then:
rsi-eval bench candidate verify <id>              # writes verify.json; exit 0 clean, 1 on any finding
```

- Choose `--parent` among candidates the index lists; it is the candidate whose overlay you extend.
- `verify` checks the schema, digest and id, `run.toml`, `notes.md`, the parent, `records.json`, and the leak
  audit. Findings give file, line and kind, never the term: fix the text and run `verify` again. Do not
  search for the term. Commit only after exit 0. The lead reruns `verify` with `--proposer-events`.
- Leave `records.json`, `digest/` and `eval/bench/tb2/candidates/index.json` alone: the lead owns them.
- Commit **only** `eval/bench/tb2/candidates/<id>/` (stage that explicit path), including `verify.json`.

### What an overlay can change

All fields optional (details: `eval/bench/README.md`):

| Field | Meaning | Limits |
|---|---|---|
| `instructions = "instructions.md"` | replaces the editable body of the bench prompt | UTF-8, at most 64 KiB |
| `[tool_policy] enabled_tools`, `denied_tools` | narrow the bench tool allowlist | can only narrow |
| `[context] compact_budget_tokens` | compaction budget | 32000..=2000000 |
| `[loop] max_iterations` | iteration cap | 10..=1000 |

- The instructions file replaces **`<tools>`, `<working>` and `<finishing>` together, as one body**. Start from a
  copy of those three blocks in `bench_instructions.md` and edit the copy; a file with only your new bullet
  would delete the tool descriptions. `<role>`, `<container>`, `<integrity>` and `<critical_rules>` are fixed.
- Backticks in the prompt are reserved for tool names.
- Instructions must be **general**: no search-task names, no task-specific commands, paths or expected outputs.
  The generality audit flags them. `notes.md` may cite search tasks.
- `web_access`, `egress`, `context_editing`, `budgets`, `cli_access` are refused.

## notes.md template

`new` writes the headings. Fill every one; a `<placeholder>` line counts as empty.

```
# <id>

## Parent
<parent id>

## Hypothesis
<what the model gets wrong, with the evidence: digest file names and the event numbers>

## Single change
<the one change this overlay makes to its parent, in one or two sentences>

## Expected effect
<which behaviour changes and in which direction; the tasks it targets; the cost effect>

## Prior candidates cited
<ids of the earlier candidates you read and what each told you, or "none" for the first>

## Risk
<how this could regress, and which tasks would show it>
```

Cite **every** earlier candidate whose notes or digest you used. (`## Risk` is extra; `verify` accepts it.)

## Handoff

Finish with these fields, nothing else:

```
candidate_id: <id>
overlay_digest: sha256:<64 hex, from verify.json>
hypothesis: <one sentence>
no_further_hypothesis: true|false
```

`no_further_hypothesis: true` means the digests give you no further distinct, untested idea after this one. If
you have no idea at all, write no directory and report `candidate_id: none` with `true`.

## Dry run: read the fixture, not a record

Before the first paid screen the lead rehearses three iterations at $0 (issue #3197). No calibration record
exists yet, so a committed fixture stands in: `eval/bench/tb2/proposer/fixtures/`.

| Path | What it is |
|---|---|
| `record.json` | A `BenchRunRecord` v1: six search-split tasks, k=2, mixed pass and fail |
| `digest/` | What `rsi-eval bench digest` writes for it; read this **instead of** `candidates/<seed>/digest/` |
| `bench/fixture-baseline/<trial>/trace.jsonl` | The traces the digests name, as repo-relative paths |
| `outcomes/screen-*.json`, `select-state.json` | Imaginary screen outcomes, for `rsi-eval bench select` |

The traces are made up. They are not a Terminal-Bench run and tell you nothing about the real model.
Regenerate with `RSI_PROPOSER_FIXTURE_BLESS=1` (see `crates/rsi-eval/src/proposer_fixture.rs`).

Lead setup on a **scratch branch** (never landed), once:

```
rsi-eval bench candidate new --seed
# fill the seed's notes.md ("Parent: none"), then:
cp -r eval/bench/tb2/proposer/fixtures/digest eval/bench/tb2/candidates/<seed>/digest
rsi-eval bench candidate verify <seed>
rsi-eval bench candidate index
```

Proposer iteration 1 reads the fixture digests and the seed; iterations 2 and 3 also read the earlier
candidates' overlays and notes and must cite them. After each session the lead runs
`rsi-eval bench candidate verify <id> --proposer-events <export.json>` and `rsi-eval bench candidate index`.
After three, `rsi-eval bench select eval/bench/tb2/proposer/fixtures/select-state.json` must print a next
action (exit 10 `propose`, 11 `extend`, 12 `validate`, 13 `stop`, 14 `defer`; 1 is a setup error). On the
fixture it exits 10. Summarise the candidate ids and each `verify.json` result on #3156.
