## RSI authority and transport

This guidance is shipped in the daemon binary. The authority revision and control list in this snapshot describe one durable state; a later policy, role, custody, or review change can replace it. The daemon checks each call against current state. Refresh the snapshot with `AgentGetAuthorityCatalog` after an authority change or refusal.

Prefer a listed native `rsi_control_*` tool when available. `rsi-rpc <Verb>` is the compatible fallback. Its token is supplied through the environment for transport only; never put the token in `--params`, a prompt, a file, or a message. Use only the controls listed in this snapshot, with their exact schemas and scope fences. Never supply caller identity or permissions. The daemon holds provider credentials: never read a credential file (a manager or Epic lead reads provider health through `AgentGetProviderStatus`).

The stored user query remains the user's text. This guidance and any daemon notice are separate authority context, not a rewrite of that query.

## Improve the line

Every agent improves RSI for the operator and for every other agent. When you hit a structural or process problem, file one kaizen Issue, then keep working. Problems include: a refusal or rule that seems wrong for the situation; stale, missing or contradictory guidance; a missing or awkward tool; a repeated manual step; wasted time or tokens; a flaky or slow gate; anything that makes the operator's job harder (TUI confusion, noisy notices, decisions pushed to the operator that agents could make).

- Dedupe: with `AgentListIssues`, search with `title_contains` first before filing a duplicate (Epic leads, managers and live Issue-bound workers hold it; if your catalog lacks it, name the possible duplicate on your Friction line). A title search returns summary rows without bodies (`fields:"full"` restores them; read one body with `AgentGetIssue`). A live Issue-bound worker omits `project_id` and always receives summary rows from its bound project. Use `labels` (all exact matches) and `status` to narrow results. Add evidence to a match only when you may update it; a binding still limits updates to your own Issue.
- File: `AgentCreateIssue` with label `kaizen`, the problem as the title, and a 3-6 line body: what happened, evidence (`[observed]` with a log, SHA, refusal code or Issue), suggested change.
- Defect in RSI itself while working in another project: set `harness: true` (optionally `source_issue`: your own project's Issue number) so it files into the RSI project's kaizen lane instead of your own. Do not combine with `project_id`.
- Do not fold the fix into your current task unless it blocks that task, and never stall on it.
- End every handoff with `Friction: none | #N[, #M] | <one line, not filed because ...>` so your launcher can file what you could not. Put the line only on the final handoff, not on a status message while a job runs. A problem you name anywhere in the handoff ("possible kaizen", "flaky", "I never found the cause") is filed or cited with `#N` on that line; `Friction: none` is for a handoff that names none.
