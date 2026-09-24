# Drive Plan

Purpose: collapse context gathering, planning, implementation, and verification into one user command while preserving the quality benefits of durable research and plan structure.

Use the arguments supplied with the command as the task. If no task is supplied, ask for one.

## Operating Contract

- Obey all local repository instructions, system instructions, developer instructions, and sandbox or worktree policy.
- Treat this command as authorization to complete the task end to end.
- Work in phases, but do not stop between phases unless a blocking decision is required.
- Minify user interaction, not engineering structure.
- Preserve user changes. Never revert work you did not make.
- Do not run destructive git commands.
- Do not commit or push unless the user explicitly asked for that, or the local repository instructions require it.
- If presenting options to the user, state your recommendation and the reason in one or two sentences.

## Artifact Contract

Create or update a plan document under:

```text
thoughts/shared/plans/YYYY-MM-DD-<short-slug>.md
```

The plan document must include:

- Original Request
- Context Brief
- Relevant Files and Systems
- Current Behavior
- Constraints and Local Instructions
- Expert Review
- Design Decisions
- Implementation Plan
- Verification Plan
- Risks
- Completion Log

Create a separate research document under `thoughts/shared/research/` only when it adds real value, such as:

- The task spans more than two subsystems.
- Historical context from `thoughts/` materially affects the design.
- External documentation was required.
- The context brief would otherwise become too large for the plan.

When a research document is created, link it from the plan and keep the implementation plan self-contained enough for a fresh agent to continue.

## Scope Gate

Before editing code, classify the task:

- Small: use the `ship_small` workflow instead if the task clearly satisfies that command's scope gate.
- Plan-driven: continue with this command when the task benefits from durable context and planning.
- Team-sized: stop and recommend the appropriate team or orchestration command when the task likely needs more than three phases, touches more than two crates or major subsystems, mixes several high-risk domains, or requires more than one focused implementation session.

If team-sized, report:

```text
Scope appears team-sized: <reason>.
Recommended command: <team command or orchestration command>
Reason: this should be decomposed instead of forced through one foreground agent.
```

## Workflow

1. Preflight.
   - Read local repository instructions.
   - Check git status and identify unrelated user changes without modifying them.
   - Identify whether local policy requires a worktree or sandbox before edits.

2. Gather context.
   - Read directly mentioned files first.
   - Use fast search before broad reads.
   - Inspect relevant code, tests, docs, and prior `thoughts/` artifacts.
   - Use parallel sub-agents only when the active CLI supports them and the task is decomposable without duplicating work.

3. Write the plan artifact.
   - Capture concrete context, constraints, decisions, file references, implementation steps, and verification commands.
   - Resolve routine engineering decisions yourself.
   - Do not leave open questions in the plan unless they are explicit blockers.

4. Implement from the plan.
   - Make the smallest complete set of changes that satisfies the plan.
   - Follow existing architecture and style.
   - Update tests, docs, command files, and generated artifacts when the changed surface requires it.
   - If keybindings change, update `docs/keybindings.md`.

5. Verify.
   - Run the verification commands named in the plan when feasible.
   - Add or adjust targeted tests when the risk justifies it.
   - If full verification is too expensive or unavailable, run the closest targeted checks and record the gap.

6. Update the artifact.
   - Add completion notes to the plan.
   - Record deviations from the original plan and why they were correct.
   - Record verification results.

7. Report.
   - Summarize the result.
   - List files touched.
   - Name the plan and research artifacts.
   - State verification run and results.
   - Call out residual risk or follow-up work.

## Stop Conditions

Stop and ask only if:

- The task has multiple valid product behaviors and code cannot resolve the choice.
- There are conflicting instructions.
- The change would be destructive or irreversible.
- Required credentials, services, or dependencies are missing.
- The implementation would exceed the requested scope.
- Verification repeatedly fails for the same reason after focused repair.
