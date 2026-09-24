# Ship Small

Purpose: execute a narrow engineering task in one pass without losing basic engineering discipline.

Use the arguments supplied with the command as the task. If no task is supplied, ask for one.

## Operating Contract

- Obey all local repository instructions, system instructions, developer instructions, and sandbox or worktree policy.
- Treat this command as authorization to gather context, edit files, run targeted verification, and report the result.
- Minify user interaction, not engineering structure.
- Preserve user changes. Never revert work you did not make.
- Do not run destructive git commands.
- Do not commit or push unless the user explicitly asked for that, or the local repository instructions require it.
- Ask the user only for blocking product decisions, destructive operations, missing credentials, impossible-to-resolve ambiguity, or scope expansion.

## Scope Gate

Use `ship_small` only when all of these are true:

- The task is likely to touch three files or fewer.
- The work is contained to one subsystem.
- No schema migration, persistence contract, RPC surface, keybinding grammar, session lifecycle, rendering pipeline, or security boundary is involved.
- The correct approach is discoverable from nearby code and tests.
- Targeted verification is enough to build confidence.

If any condition fails, stop before editing and recommend:

```text
Scope exceeds /ship_small: <reason>.
Recommended command: /drive_plan <task>
Reason: this needs durable context and plan artifacts to hold quality.
```

## Workflow

1. Gather context.
   - Read the local instructions first.
   - Use fast search before broad reads.
   - Inspect directly relevant code, tests, docs, and recent patterns.

2. Form a micro-plan.
   - Keep it in working memory unless the scope gate says to escalate.
   - Apply the repository's expert framework: software engineering, zero-waste correctness, power-user UX, cognitive flow, vim grammar, performance, and reliability.
   - Resolve routine engineering choices yourself.

3. Implement.
   - Make the smallest complete change that satisfies the task.
   - Follow existing style and module boundaries.
   - Update tests or docs when behavior, command surfaces, keybindings, or public contracts change.
   - If keybindings change, update `docs/keybindings.md`.

4. Verify.
   - Run the most relevant formatter, build, lint, and test commands for the changed surface.
   - Prefer targeted checks over broad expensive checks unless the blast radius requires a full workspace check.

5. Report.
   - Summarize what changed.
   - List files touched.
   - State verification run and results.
   - Call out residual risk or anything not verified.

## Stop Conditions

Stop and ask only if:

- The task has multiple valid product behaviors and code cannot resolve the choice.
- The change would be destructive or irreversible.
- Required credentials, services, or dependencies are missing.
- The implementation would exceed the stated scope.
- Verification fails after a reasonable focused repair attempt.
