# Worker contract reference

The launch preamble stays brief because every leaf worker pays its prompt cost. `AGENTS.md` holds canonical repository rules. The including command supplies role, task, return schema, and any tighter limits. A parentless session directly addressed by the human may answer plainly; do not infer that status from a title or missing schema.

## Evidence and return

`[observed]` means executed or inspected in this pass; cite the actual result. `[source]` means extracted from an authoritative source with an exact location or reproducible extraction. `[inferred]` means reasoned but unverified. Mechanically extract counts, schemas, and field sets. Check the current tool surface before claiming an exact helper or payload shape. Do not use inference as acceptance or success evidence, or call an inference-bearing plan implementation ready without resolving it.

A worker's final answer to a master starts `PIPELINE HANDOFF — ` on the first nonblank line. Follow the including command's exact return schema and per-field caps. Return the outcome, evidence paths, checks, commit, and blocker if any. Prefer file and line references over excerpts; leave code and detailed artifacts on disk. If incomplete, report a precise partial result and observed blocker evidence. Budget or review exhaustion calls for continuation, not a fabricated human gate.

If a `## Stage contract` appears, `rsi-contract-validate` checks `### Inputs`, `### Process`, `### Outputs`, `### Verify` in order. Inputs must name a static artifact or explicit `rg`/glob discovery budget. The block is optional for historical handoffs; an incomplete block fails validation. The including command may require it.

Write a handoff document after other edits and commits. If more files change afterward, refresh its recorded `git_commit:` before return. Commit task-owned `thoughts/` artifacts. Use the worktree-isolated `scripts/rsi-stash` interface for stash operations. For issue control authority and payloads, read `.agents/skills/rsi-agent-control/SKILL.md` on demand. For disk pressure during builds, check `df -h /tmp .` before diagnosing SQLite write errors as product defects.

Reconcile a plan's conflicting requirements before implementation. Assert positive user-visible identity in tests. The canonical form of these repository rules is `AGENTS.md`.
