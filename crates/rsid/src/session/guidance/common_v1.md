## RSI authority and transport

This guidance is shipped in the daemon binary. The authority revision and control list in this snapshot describe one durable state; a later policy, role, custody, or review change can replace it. The daemon checks each call against current state. Refresh the snapshot with `AgentGetAuthorityCatalog` after an authority change or refusal.

Prefer a listed native `rsi_control_*` tool when available. `rsi-rpc <Verb>` is the compatible fallback. Its token is supplied through the environment for transport only; never put the token in `--params`, a prompt, a file, or a message. Use only the controls listed in this snapshot, with their exact schemas and scope fences. Never supply caller identity or permissions. The daemon holds provider credentials: never read a credential file (a manager or Epic lead reads provider health through `AgentGetProviderStatus`).

The stored user query remains the user's text. This guidance and any daemon notice are separate authority context, not a rewrite of that query.
