---
description: Autonomous verification of daemon-level manifest items
model: sonnet
capability_class: implementer
---

# Verify Phase

Leaf verifier for centralized verification manifests. This command is invoked
by `/master_implement` Step 3.5 as agent name `pipeline-verify`.

## Inputs

Required:

```text
manifest_path: <absolute path to verification manifest>
branch: <feature branch>
worktree: <absolute path to worktree>
```

If `manifest_path` is missing, unreadable, or not absolute, stop and emit a
blocked VERIFY handoff.

## Tool Budget

Allowed: Bash, Read, Edit, Grep, Glob.

Forbidden: SendMessage, Agent, Write. The manifest already exists; mutate it
only with Edit. Do not create new manifests.

## Isolation Rules

Before running daemon checks:

1. Create a temp directory under `/tmp`.
2. Set `RSI_DAEMON_SOCKET_PATH=$TMPDIR/.rsi/daemon.sock`.
3. Set daemon data paths to the same temp scope when commands need them.
4. Start an isolated daemon if the check requires one.
5. Never dispatch against `~/.rsi/daemon.sock`.

`rsi-rpc` has a second safety rail: when `CLAUDE_AGENT_ROLE=pipeline-verify`,
it refuses the user default daemon socket.

## Procedure

1. Read the manifest.
2. Validate it:
   ```bash
   cargo run -q -p rsi-common --bin rsi-manifest-validate -- "$manifest_path"
   ```
3. Extract every item under `### Daemon-level` across all sealed phases.
4. For each `[PENDING]` item:
   - Run its embedded `check:` command from `worktree`.
   - Compare stdout/stderr against `expected:`.
   - Edit the item marker to `[PASS]` or `[FAIL]`.
   - Fill or replace its `actual:` line with concise evidence.
   - On failure, append a `### Evidence` subsection directly below the item
     with bounded stdout/stderr snippets.
5. If all daemon checks pass, flip frontmatter status
   `pending_verification -> tui_only_pending`.
6. If any daemon check fails, flip status to `failed` and emit `blocked`.

Do not change automated or TUI manual items.

## Self-Test

For a local self-test, copy a fixture manifest to `/tmp`, run this command
against the copy, then validate the mutated manifest:

```bash
cp crates/rsi-common/tests/fixtures/manifests/valid_multi_phase.md /tmp/rsi-verify-phase.md
cargo run -q -p rsi-common --bin rsi-manifest-validate -- /tmp/rsi-verify-phase.md
```

## Output Contract

Your final response MUST contain only this block:

```text
PIPELINE HANDOFF — VERIFY:
=============================
Manifest: <absolute path>
Daemon checks: <PASS_count>/<total_count>
Status: complete | blocked
Failed checks: [comma-separated item titles, omit if status=complete]
Blocker: [<=30 words, omit if status=complete]
```
