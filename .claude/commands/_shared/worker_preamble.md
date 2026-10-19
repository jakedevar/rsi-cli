---
version: 30
role_variants: [research, planning, implementation]
---

# RPI Worker Preamble

Finish the assigned deliverable, verify it, commit scoped changes, and return compactly. `AGENTS.md` is the canonical repository rule list; workflow detail is in `docs/agents/worker-contract.md`.

Preserve the ancestry of your allocated source on the assigned sandbox branch. Merge `origin/rolling` when needed, never rebase a source stack; let the queue's lander integrate `rolling`. Custody and pending job wakes require that source to remain an ancestor.

For durable follow-ups outside this deliverable, use `AgentCreateIssue` or its native equivalent; the full authority and payload contract is in `.agents/skills/rsi-agent-control/SKILL.md`.

Kaizen: improve the line, never stop it. When you hit a structural or process problem (a wrong-seeming refusal, stale guidance, a missing tool, a repeated step, waste, a flaky gate, operator friction), file one `kaizen` Issue and keep working; what to file and how to dedupe is "Improve the line" in your `AgentGetAuthorityCatalog` guidance.

An operator-direct, parentless session may answer plainly. If either condition is unknown, use the worker return contract; an explicit operator return format still applies.

## Evidence

Tag load-bearing claims `[observed]` for this pass's result, `[source]` for cited authoritative material, or `[inferred]` for unverified reasoning. Cite the artifact; do not claim inference as acceptance evidence. Details: `docs/agents/worker-contract.md`.

## Return

The first nonblank final line to a master is `PIPELINE HANDOFF — <STAGE>:` (em dash, uppercase stage, trailing colon); `rsi-contract-validate` parses it. Use the including command's stage and body schema; Issue implementation workers use `IMPLEMENTATION`, reviewers use `REVIEW`. Put `RESULT` or `REVIEW` fields on the next line when required by the worker contract.

Return budget: research ≤250 tokens; planning ≤400; implementation ≤300. Use the lowest cap when the role is unclear.

Echo the including command's `capability_class` (`architect`, `implementer`, or `lookup_fast`) in one return field; omit it if undeclared.

Keep returns to results, evidence paths, tests, commit, and blockers; omit code and repeated instructions. See `docs/agents/worker-contract.md`.

End every handoff with `Friction: none | #N[, #M] | <one line, not filed because ...>`: the kaizen Issues you filed, or the problem you could not file. Put it only on the final handoff, not on status messages while a job runs. A problem named anywhere in the handoff body ("possible kaizen", "flaky", "never found the cause") is filed or cited with `#N` on that line; `Friction: none` means the handoff names none.

## Stage contract

If included, use `### Inputs`, `### Process`, `### Outputs`, `### Verify` in that order.
Inputs name a static artifact or a code-discovery budget (`rg`/glob), or both.
`rsi-contract-validate` checks the shape; see `docs/agents/worker-contract.md`.

## Verification

Verification scope and limits stated in your launch brief (a policy such as `scoped_only`, a forbidden build, a CPU, timeout or wake limit, a deadline) win over everything below. Acceptance is a green `scoped_test` receipt whose `receipt_head` equals the exact final committed HEAD. Run one `scoped_test` per final head (`AgentSubmitJob` `scoped_test` with `base:"origin/rolling"`, one `AgentScheduleWake {"mode":"when","message":"<what to do on resume>","when":{"jobs_terminal":["<job id>"]}}`; `message` is required); it derives its own scope. After a red run, fix the cause, commit, push, and run exactly ONE more `scoped_test`. Never loop or retry the same head; if the second run is still red, hand off with the reds named instead of starting a third. A prose-only, docs-only or source-only change needs no compile job. Optional compile feedback, only when the brief does not forbid it: one `build` job `check` for the touched `package` (`--keep-going`); for a shared type or signature change across crates, `{"command":"check","workspace":true,"all_targets":true}` instead reports every crate's errors in one pass. Optional early feedback for a new runtime or schema assertion you are unsure of: before the `scoped_test`, at most one exact package `test` job per touched crate (`package`, `lib_only`, `exact`, full-path `filters`). It runs the working tree, not committed bytes, so it is never acceptance; a zero-test match fails `job_filter_matched_no_tests`; acceptance still requires the green receipt at the final committed HEAD. To verify shard features (`--no-default-features --features test-shard-X`) never self-launch cargo: submit one `build` job `{"command":"test_no_run","package":"rsid","shards":["store-01","other-02"]}`; it builds the shards in `rsi-build.slice`, cleans the package between them, and its receipt names each shard's exit code and warnings. Do not derive landing filters by hand: the merge queue derives them. A `scoped_test` receipt with `merges_cleanly_onto_rolling: false` means your commit no longer merges onto `origin/rolling`: merge it (no rebase), resolve `conflict_paths`, commit and push; verify the new head within the two-run limit above, or hand off the remaining blocker. Allow healthy `scoped_test` progress inside its admitted budget (default 60 minutes); passing 20 or 30 minutes alone is not runaway. Inspect stalled, over-broad or unbounded work and CPU pressure; cancel your own runaway job with `AgentCancelJob` and report elapsed time and evidence. Other runs stop at twice their expected time or the brief's deadline, whichever comes first (30 minutes if neither is known). An explicit brief deadline still applies to `scoped_test`; stop only your own processes or units. `AgentHalt` stops a provider session, not a detached job. No repeated timeout restarts or long sleep-poll loops.

Python suites run under stdlib `python3 -m unittest`; never require `pytest` (a function-style tools suite may use it only when the host has it). A runner the host lacks is reported as `runner_unavailable` with zero executed tests, never as green and never replaced by another runner (`docs/agents/worker-contract.md`).

Emit verification items in the handoff as the including command requires; see `docs/agents/verification.md` for buckets, manifest ownership, and cross-stage linkage.

Write the handoff last so its recorded commit remains current.
Commit before you park: end a turn to wait on a job or wake only with your work committed, since an uncommitted file at park time is at risk if the session is cut off (#2625).
Commit task-owned `thoughts/` files before returning; see `AGENTS.md`.
Use `scripts/rsi-stash` for isolated stash operations; never use a bare Git stash.
