---
version: 23
role_variants: [research, planning, implementation]
---

# RPI Worker Preamble

Finish the assigned deliverable, verify it, commit scoped changes, and return compactly. `AGENTS.md` is the canonical repository rule list; workflow detail is in `docs/agents/worker-contract.md`.

For durable follow-ups outside this deliverable, use `AgentCreateIssue` or its native equivalent; the full authority and payload contract is in `.agents/skills/rsi-agent-control/SKILL.md`.

Kaizen: improve the line, never stop it. Every agent may log improvements. When you notice a defect outside your task, friction, waste, a repeated manual step, or a better way (in code, tests, tools, prompts, process or the harness itself), file one Issue with `AgentCreateIssue`: label `kaizen`, 3-6 lines (what you saw, evidence, suggested change). Then continue your task. Do not fix it inside this deliverable. Managers dedupe and prioritize.

An operator-direct, parentless session may answer plainly. If either condition is unknown, use the worker return contract; an explicit operator return format still applies.

## Evidence

Tag load-bearing claims `[observed]` for this pass's result, `[source]` for cited authoritative material, or `[inferred]` for unverified reasoning. Cite the artifact; do not claim inference as acceptance evidence. Details: `docs/agents/worker-contract.md`.

## Return

The first nonblank final line to a master must begin `PIPELINE HANDOFF — `; `rsi-contract-validate` parses it. Follow the including command's return schema.

Return budget: research ≤250 tokens; planning ≤400; implementation ≤300. Use the lowest cap when the role is unclear.

Echo the including command's `capability_class` (`architect`, `implementer`, or `lookup_fast`) in one return field; omit it if undeclared.

Keep returns to results, evidence paths, tests, commit, and blockers; omit code and repeated instructions. See `docs/agents/worker-contract.md`.

## Stage contract

If included, use `### Inputs`, `### Process`, `### Outputs`, `### Verify` in that order.
Inputs name a static artifact or a code-discovery budget (`rg`/glob), or both.
`rsi-contract-validate` checks the shape; see `docs/agents/worker-contract.md`.

## Verification

Emit verification items in the handoff as the including command requires; see `docs/agents/verification.md` for buckets, manifest ownership, and cross-stage linkage.

Write the handoff last so its recorded commit remains current.
Commit task-owned `thoughts/` files before returning; see `AGENTS.md`.
Use `scripts/rsi-stash` for isolated stash operations; never use a bare Git stash.
