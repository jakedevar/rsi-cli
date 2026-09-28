---
name: orchestration-router
description: Risk and capability router for agent work
---

# Orchestration Router

Choose the smallest useful work unit and lowest capable model. Risk tiers and review details: `docs/agents/routing.md`.

| Class | Claude | Codex | Use |
| --- | --- | --- | --- |
| `architect` | `opus`, `xhigh` | `gpt-6-astra`, `xhigh` | Ambiguous architecture or independent semantic review |
| `implementer` | `sonnet`, `high` | `gpt-6-astra`, `high` | Scoped implementation or investigation |
| `lookup_fast` | `haiku` | `gpt-6-astra`, `low` | Lookup, formatting, build, test |

- Use the lowest capable tier and one owner for each shared effect.
- Parallelize only disjoint work that shortens the critical path.
- Tier-2 gets one independent review round and one finding-focused delta round; unresolved blockers still block acceptance.
- Reuse exact-source evidence; do not re-verify what is already established.
- Escalate the model only on concrete evidence of capability failure.
