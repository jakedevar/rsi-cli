# Configuration

`rsid` loads daemon-scoped secrets from `~/.rsi/.env` during startup, before
config and provider availability checks read environment variables.

Use the committed `.env.example` as a template, but keep real API keys only in
`~/.rsi/.env`:

```bash
mkdir -p ~/.rsi
cp .env.example ~/.rsi/.env
$EDITOR ~/.rsi/.env
```

Restart `rsid` after editing `~/.rsi/.env`.

Process environment variables take precedence over `~/.rsi/.env`. Values set by
your shell, service manager, or launch wrapper are not overwritten by dotenv
values. Missing `~/.rsi/.env` is normal. Malformed dotenv syntax is logged as a
warning and the daemon continues without those file values.

Repo-root `.env` is ignored by git and is not the production config path. Use it
only for ad hoc local tooling that explicitly reads repo-root dotenv files; the
daemon reads `~/.rsi/.env`.

## Retry Configuration

Retry defaults are kind-scoped. `Story` and `Standard` sessions default to no
automatic retry because replaying them can duplicate an interactive or
orchestrator prompt. Worker kinds (`TaskRabbit`, `Task`, `Bug`, `Feature`,
`Refactor`, `Research`) use `RSI_RETRY_MAX_DEFAULT`, which defaults to `3`.
An explicit `LaunchSessionParams.max_retries` value overrides the kind default
for that one launch.

| Variable | Default | Effect |
|---|---:|---|
| `RSI_RETRY_MAX_DEFAULT` | `3` | Default retry budget for worker-kind sessions. Set `0` to make new sessions default to no retry. |
| `RSI_RETRY_MAX_BACKOFF_MS` | `120000` | Maximum retry backoff in milliseconds. |
| `RSI_RETRY_ON_STALL` | `true` | Enables the bus-driven stall retry handler at daemon startup. |

The legacy `MOTHERSHIP_*` and `FLYWHEEL_*` names are still accepted for these
variables.

Runtime config updates through `UpdateDaemonConfig` are live for
`retry_enabled`, `retry_max_default`, and `retry_max_backoff_ms`: disabling
`retry_enabled` stops new retry arming and restart re-queueing immediately.
Already-armed timers are not drained automatically; use `CancelRetry` for a
specific pending retry. To re-enable worker retries after using the kill-switch,
set `retry_max_default=N` (`N>0`) rather than `retry_enabled=true` alone — the
disable zeroes `retry_max_default`, and setting the default back also re-enables
`retry_enabled`.

## Model Control

### Codex CLI models and reasoning effort

The Codex provider discovers its model picker entries dynamically from the
installed CLI with `codex debug models --bundled`. If that command is
unavailable, fails, or returns no visible models, RSI uses this offline fallback
order: GPT-5.6-Sol, GPT-5.6-Terra, GPT-5.6-Luna, GPT-5.5, GPT-5.2.

`None` leaves reasoning effort at the selected model's CLI default. The known
fallback models use these defaults and ordered ladders:

| Model | Default | Supported effort ladder |
|---|---|---|
| GPT-5.6-Sol | `low` | `low` → `medium` → `high` → `xhigh` → `max` → `ultra` |
| GPT-5.6-Terra | `medium` | `low` → `medium` → `high` → `xhigh` → `max` → `ultra` |
| GPT-5.6-Luna | `medium` | `low` → `medium` → `high` → `xhigh` → `max` |
| GPT-5.5 / GPT-5.2 | `medium` | `low` → `medium` → `high` → `xhigh` |

RSI rejects raw effort values outside this set and rejects known
model-and-effort combinations outside the applicable ladder. Explicit
unknown/custom model identifiers remain compatible pass-throughs: RSI forwards
their valid raw effort to the Codex CLI, which remains authoritative for their
capabilities.

Operator-facing model control is a separate control plane from `UpdateDaemonConfig`.
Use the operator RPCs:

- `GetModelControlStatus`
- `UpdateModelControlPolicy`
- `ListModelInvocations`
- `CancelModelInvocation`

The live modes are:

| Mode | Effect |
|---|---|
| `normal` | Default policy. Interactive paid work is allowed subject to registry policy and budget. |
| `pause_background` | Denies new background model work. Foreground work remains eligible. |
| `deny_paid` | Denies new paid-capable model work. Local-only flows remain eligible. |
| `local_only` | Denies any new non-local model work. |
| `stop_all` | Denies new paid work and, when `interrupt_active=true`, interrupts/cancels eligible active work. |

These mode changes are live immediately and are advertised on the daemon bus as
`model_control_mode_changed` and `model_control_circuit_changed`. The TUI
surfaces them in `Settings -> Daemon Features` and `Settings -> Stats`.

Conservative defaults:

- Worker retry default is `0` unless a kind-specific policy or launch override
  enables it.
- Dream consolidation starts disabled by default. Set `RSI_DREAM_ENABLED=true`
  to opt in, or enable it from the TUI memory settings.

Local/OpenAI-compatible session routing is classified from the resolved
transport, not from the provider name alone. Every `/chat/completions` attempt
uses model-control admission and settlement, including tool-loop follow-up
turns.

Native `ollama_client` generation is local-only. `RSI_OLLAMA_URL` must resolve
to `localhost` or a loopback IP address; the daemon rejects any other host
before network I/O. Configure remote Ollama or another compatible service
through the admitted OpenAI-compatible provider route instead.

## Issue Tracker

The daemon runs at most one issue-tracker backend, selected at startup by
`RSI_ISSUE_TRACKER_KIND`. Selection is exclusive: `local` ignores any Linear
credentials that happen to be set, and `linear` (or unset) behaves exactly as
before the selector existed.

| Variable | Default | Effect |
|---|---:|---|
| `RSI_ISSUE_TRACKER_KIND` | `linear` | Backend selector: `linear` or `local`. Any other value disables the tracker (with a warning). |
| `RSI_ISSUE_TRACKER_WORKING_DIR` | — | Required for both kinds; working directory for sessions dispatched from issues. |
| `RSI_LINEAR_API_KEY` / `RSI_LINEAR_TEAM_ID` | — | Required (non-empty) only for `kind=linear`. |

- `kind=linear` (or unset): the tracker starts only when `RSI_LINEAR_API_KEY`,
  `RSI_LINEAR_TEAM_ID`, and `RSI_ISSUE_TRACKER_WORKING_DIR` are all set —
  unchanged legacy behavior.
- `kind=local`: only `RSI_ISSUE_TRACKER_WORKING_DIR` is required. Candidates
  come from the daemon's own `issues` store (ready = `Open` with no open
  blocker), identifiers look like `LOCAL-<n>`, and the `issue_tracker`
  daemon capability reports `true` (`GetIssueTrackerStatus` shows
  `tracker: "local"`).
- The generic knobs (`RSI_ISSUE_TRACKER_POLL_INTERVAL_MS`,
  `RSI_ISSUE_TRACKER_ACTIVE_STATES`, `RSI_ISSUE_TRACKER_MAX_CONCURRENT`,
  `RSI_ISSUE_TRACKER_PROVIDER`, `RSI_ISSUE_TRACKER_MODEL`,
  `RSI_ISSUE_TRACKER_COMPLETION_STATE`) apply to both kinds.

Local-mode note: see [the local issue tracker guide](local-issue-tracker.md).
Without `RSI_ISSUE_TRACKER_COMPLETION_STATE`, completion neither closes the
local issue nor releases its running slot; set it to `completed`/`closed` or use
`UpdateIssueStatus`. Local candidates normalize to `unstarted`, so excluding
that state from `RSI_ISSUE_TRACKER_ACTIVE_STATES` silently yields zero work.

### Amazon Bedrock through Codex CLI

Select **Bedrock** in provider picker. RSI sends Codex CLI Responses requests to
`https://bedrock-runtime.{region}.amazonaws.com/openai/v1`. Set
`AWS_REGION` or `AWS_DEFAULT_REGION`; RSI also reads region from
`aws configure get region` when environment has none. To use renewable
short-term keys, install AWS's token generator into
`~/.rsi/bedrock-token-venv` with `python3 -m venv ~/.rsi/bedrock-token-venv`
and `~/.rsi/bedrock-token-venv/bin/python -m pip install aws-bedrock-token-generator`.
With AWS credentials available to the daemon, RSI generates a new
region-specific bearer key for each Codex CLI launch and passes it only through
the child environment. Set `AWS_BEARER_TOKEN_BEDROCK` in the daemon environment
(for example in `~/.rsi/.env`) only to use an existing key instead. Restart the
daemon after changing its environment or installing the generator. Keep
`~/.rsi/.env` private (`chmod 600 ~/.rsi/.env`). [AWS documents short-term key
expiry](https://docs.aws.amazon.com/bedrock/latest/userguide/api-keys-generate.html).
AWS credentials enable token generation; they do not authenticate Codex CLI
inference by themselves. Keep bearer keys out of command
arguments and logs.

Model picker calls the Bedrock control-plane `ListInferenceProfiles` operation
with the same bearer key used for launch (no AWS CLI profile involved) to show
active OpenAI GPT inference profiles for selected region. These are catalog entries: account model
access is checked by Bedrock when inference starts. Default model is
`global.openai.gpt-5.6-sol`; choose another profile when region or account
requires one (for example, organization SCPs that deny cross-geography routing
reject every `global.*` profile while allowing `us.*`). Bedrock runtime does not implement `GET /models` for Responses.
