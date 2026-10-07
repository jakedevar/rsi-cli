# Worker contract reference

The launch preamble stays brief because every leaf worker pays its prompt cost. `AGENTS.md` holds canonical repository rules. The including command supplies role, task, return schema, and any tighter limits. A parentless session directly addressed by the human may answer plainly; do not infer that status from a title or missing schema.

## Verification bounds

Scope verification to touched modules using filters from `scripts/check-touched-shards --base origin/rolling`. Set an expected duration before running; stop and report the command, elapsed time and log when a run exceeds twice its expected time or 30 minutes, whichever comes first (30 minutes if unknown). Stop only processes or units you started. Do not repeatedly restart timed-out runs or use long sleep-poll loops. The suggested filters are bounded and exact (named tests, not module prefixes); a receipt with `broad: true` names a filter that may not finish in a worker's budget, so run it in parts or say so in the handoff (usage text in the script).

## Evidence and return

`[observed]` means executed or inspected in this pass; cite the actual result. `[source]` means extracted from an authoritative source with an exact location or reproducible extraction. `[inferred]` means reasoned but unverified. Mechanically extract counts, schemas, and field sets. Check the current tool surface before claiming an exact helper or payload shape. Do not use inference as acceptance or success evidence, or call an inference-bearing plan implementation ready without resolving it.

A worker's final answer to a master has `PIPELINE HANDOFF — <STAGE>:` on the first nonblank line (em dash, uppercase stage, trailing colon). Use the including command's stage, body schema and per-field caps. Issue implementation workers use `PIPELINE HANDOFF — IMPLEMENTATION:`; reviewers use `PIPELINE HANDOFF — REVIEW:`. Put `RESULT` or `REVIEW` fields on the next line when required by `thoughts/shared/manager/worker-contract.md`. Return the outcome, evidence paths, checks, commit, and blocker if any. Prefer file and line references over excerpts; leave code and detailed artifacts on disk. If incomplete, report a precise partial result and observed blocker evidence. Budget or review exhaustion calls for continuation, not a fabricated human gate. The handoff's last line is `Friction: none | #N[, #M] | <one line, not filed because ...>` (#1332); `rsi-contract-validate` accepts it and does not yet require it.

If a `## Stage contract` appears, `rsi-contract-validate` checks `### Inputs`, `### Process`, `### Outputs`, `### Verify` in order. Inputs must name a static artifact or explicit `rg`/glob discovery budget. The block is optional for historical handoffs; an incomplete block fails validation. The including command may require it.

Write a handoff document after other edits and commits. If more files change afterward, refresh its recorded `git_commit:` before return. Commit task-owned `thoughts/` artifacts. Use the worktree-isolated `scripts/rsi-stash` interface for stash operations. For issue control authority and payloads, read `.agents/skills/rsi-agent-control/SKILL.md` on demand. For disk pressure during builds, check `df -h /tmp .` before diagnosing SQLite write errors as product defects.

Reconcile a plan's conflicting requirements before implementation. Assert positive user-visible identity in tests. The canonical form of these repository rules is `AGENTS.md`.
