# Agent Commands

Project-local top-level `.claude/commands/*.md` files are the source of truth
for shared agent command text. Supporting files under `.claude/commands/_shared/`
are local overlays or preambles; they are not top-level slash commands.

## Commands

- `ship_small`: narrow one-pass implementation for small tasks. It gathers only the needed context, makes the change, verifies it, and reports. It should escalate to `drive_plan` when durable planning would materially improve quality.
- `drive_plan`: one user command for context gathering, planning, implementation, and verification. It keeps the quality value of research and plan artifacts by writing durable docs before implementation.

## Distribution

Project-local adapters are installed in:

- `.claude/commands/`
- `.codex/prompts/`
- `.gemini/commands/`
- `.agents/skills/`

For the explicit cross-harness canonical set, Claude commands are the source of
truth. Codex prompts are provider adapters: they remove only `model` and
`capability_class` from Claude's first YAML block and retain the three committed
Codex `argument-hint` adapters. Gemini commands are TOML wrappers whose `prompt`
value contains the complete Claude command. Active `.agents` skills are derived
from Codex prompts by `.codex/migrate-prompts-to-skills.sh`; do not edit any of
the three derived representations directly.

`master_orchestrate` is authored as a portable top-level command. Backend policy
that must follow RSI-managed sessions lives in the daemon-injected preamble, and
repo-local tightening for this project lives in
`.claude/commands/_shared/master_orchestrate_rsi_overlay.md`. Keep that split:
portable lifecycle in the mirrored command, RSI-owned control policy in daemon
framing, and `rsi` validators/smoke/docs rules in the overlay.

The overlay uses progressive disclosure. Explicit program mode loads
`master_orchestrate_rsi_program.md`; Closure-tagged review loads
`master_orchestrate_rsi_closure.md`. Ordinary slices must not load either
reference. Keep conditional mechanics out of the portable entrypoint and base
overlay so prompt cost tracks the active work.

Codex CLI currently discovers custom prompts from `$CODEX_HOME/prompts` rather than project-local `.codex/prompts` directories, so sync the repo prompts into your home prompt directory before expecting them to appear in Codex.

After changing a top-level canonical file under `.claude/commands/`, run:

```bash
scripts/sync-agent-commands.sh
```

To verify all four representations still match their provider-aware rendering:

```bash
scripts/sync-agent-commands.sh --check
tools/check-command-mirrors.sh
```

`scripts/sync-codex-prompts.sh` installs repository Codex prompts into the home
prompt directory; it is not a command-mirror renderer.

To verify the home Codex prompts match the repo prompts:

```bash
scripts/sync-codex-prompts.sh --check
```

If a CLI directory is mounted read-only, the sync script skips that destination and returns non-zero. The adapter source remains under `agent-commands/adapters/` for installation when that directory is writable.
