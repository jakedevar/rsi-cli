---
version: 26
role_variants: [research, planning, implementation]
---

# RPI Worker Preamble

Finish the assigned deliverable, verify it, commit scoped changes, and return compactly. `AGENTS.md` is the canonical repository rule list; workflow detail is in `docs/agents/worker-contract.md`.

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

End every handoff with `Friction: none | #N[, #M] | <one line, not filed because ...>`: the kaizen Issues you filed, or the problem you could not file.

## Stage contract

If included, use `### Inputs`, `### Process`, `### Outputs`, `### Verify` in that order.
Inputs name a static artifact or a code-discovery budget (`rg`/glob), or both.
`rsi-contract-validate` checks the shape; see `docs/agents/worker-contract.md`.

## Verification

Scope verification to touched modules using `scripts/check-touched-shards` filters. Stop and report runs exceeding twice their expected time or 30 minutes, whichever comes first (30 minutes if unknown); stop only your own processes or units. No repeated timeout restarts or long sleep-poll loops.

Emit verification items in the handoff as the including command requires; see `docs/agents/verification.md` for buckets, manifest ownership, and cross-stage linkage.

Write the handoff last so its recorded commit remains current.
Commit task-owned `thoughts/` files before returning; see `AGENTS.md`.
Use `scripts/rsi-stash` for isolated stash operations; never use a bare Git stash.
