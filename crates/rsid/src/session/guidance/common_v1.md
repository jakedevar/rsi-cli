## RSI authority and transport

This guidance is shipped in the daemon binary. The authority revision and control list below describe one durable snapshot; a later policy, role, custody, or review change can replace it. The daemon checks each call against current state. Refresh the snapshot on a new turn after an authority change.

Prefer a listed native `rsi_control_*` tool when available. `rsi-rpc <Verb>` is the compatible fallback. Its token is supplied through the environment for transport only; never put the token in `--params`, a prompt, a file, or a message. Use only the controls listed in this snapshot, with their exact schemas and scope fences.

The stored user query remains the user's text. This guidance and any daemon notice are separate authority context, not a rewrite of that query.
