---
description: Implement an approved plan from thoughts/shared/plans, phase by phase, with verification
---

# Implement

Input: a plan under `thoughts/shared/plans/`. Each phase lists its changes and
its success criteria. Your job is to land the phases in order and prove each
one before moving on.

**Worker preamble (binding):** This command and any sub-agents it spawns MUST load and obey `/home/jakedevar/rsi/.claude/commands/_shared/worker_preamble.md` with `role=implementation` before acting. That file defines the read budget, return budget, forbidden-content rules, and failure-mode contract. The rules below COMPOSE ON TOP and may tighten (never loosen) any limit declared there.

## Five-Expert Verification Framework

This application is built for a single user (IQ 150, ADHD, vim devotee). Quality over speed. Before committing any code, verify it through all five expert lenses:

1. **SWE** (Clean architecture): Is the code clean? No duplication? Correct abstraction boundaries? Single responsibility maintained?
2. **Tech Wizard** (Zero-waste correctness): Am I building on the right foundation? Will any of this code be thrown away when a later phase lands? If so, stop and reconsider the approach.
3. **UI/UX Power User** (Information density): Is the UI information-dense? Every action keyboard-reachable in 1-2 keystrokes? No unnecessary confirmations, hand-holding, or "are you sure?" dialogs?
4. **Cognitive Flow Engineer** (ADHD-aware design): Does this code protect flow state? Is latency acceptable — no polling where subscribe works, no spinners where instant feedback is possible? Is test coverage maintained during transitions — never create a gap that forces debugging by feel?
5. **Vim Language Designer** (Compositional grammar): Do all keybindings compose with vim grammar? `i` means insert mode, not "continue session." Actions follow verb-noun composability. Custom operators wait for motions. Command-mode (`:continue`, `:approve`) over normal-mode overloads.

**If any expert objects to the code being written, stop and resolve before proceeding.** The plan was designed with these experts in mind — if the code violates a lens, either the code or the plan needs adjustment.

Canonical reference: `five-experts.md`

## Scope Routing — MANDATORY

Before creating the worktree (next section), open the plan file and assess scope. If ANY of the following are true, automatically route to `/team_implement`:

- Plan has >3 phases
- Plan touches >2 crates (e.g., TUI + daemon + common)
- Mixes >2 of {schema migration, RPC surface, UI, persistence, daemon process management}
- Estimated >60 min of focused implementation work

Announce the routing decision once:
```
Scope is team-sized (reason: <1 sentence>); routing to `/team_implement`.
```

Continue immediately in the current invocation using the Team Implement
workflow in `team_implement.md`. Do not ask for confirmation and do not require
the user to re-invoke another command. If the active harness cannot dispatch
team workers, apply the same phase decomposition and verification discipline in
the current session and note the degraded execution mode; do not turn missing
team tooling into a user gate. An explicit user instruction to keep the work
solo overrides this automatic route.

### Preflight: deferred tools

`EnterWorktree` is a deferred tool — its schema is not loaded by default. Before calling it, invoke `ToolSearch` with `select:EnterWorktree,ExitWorktree` to load the schemas. Same applies to any other deferred tool listed in the environment reminder.

## Worktree Setup — MANDATORY FIRST STEP

Before doing ANY implementation work, you MUST create an isolated worktree:

1. Use the `EnterWorktree` tool to create a new worktree (use the plan name or ticket ID as the worktree name if available)
2. All implementation work happens inside this worktree — never modify the main working tree
3. The `thoughts/` directory is synced between the main repo and worktrees, so plan paths like `thoughts/shared/plans/...` work as-is

Do NOT skip this step. Do NOT implement directly in the main repo.

## Start

No plan path given: ask for one and stop.

With a plan path:

1. Create the worktree (above).
2. Read the whole plan. Items already ticked (`- [x]`) are done.
3. Read the ticket and every file the plan names, within the preamble's read
   budget (`_shared/worker_preamble.md` §Read budget).
4. Map how the pieces connect before editing anything.
5. Open a todo list: one item per phase, plus any setup the plan implies.
6. Begin with the first unticked item.

## Working the Plan

- The plan states intent; the code is the ground truth. Adapt details to what
  you find, but land each phase completely before starting the next.
- Tick plan checkboxes (edit the plan file) as items land, and keep the todo
  list current.
- Check that each change fits the surrounding code, not just the plan's letter.

A plan that contradicts itself is a mismatch too. If a constraint and a design
decision cannot both hold, treat it exactly like the mismatch case below: stop,
surface both statements, and get the conflict resolved. Do not quietly satisfy
one and violate the other -- that ships the violation as intended behavior.

When you write the test for such a change, assert the positive end state. Never
assert that a user-visible name, title, label, or identifier is ABSENT; that
pins the defect as a requirement and blocks whoever is sent to fix it.

### When the plan and the code disagree

Stop; do not improvise around it. Report:

  ```
  Mismatch in Phase [N]
  Plan says: [what the plan expects]
  Code shows: [what actually exists, with file:line]
  Impact: [why this blocks or changes the phase]

  Which way should I go?
  ```

## Verifying Each Phase

After a phase lands:

1. Run its automated success criteria (the commands the plan lists; often
   `make check test`).
2. Fix what fails before moving on.
3. Tick the phase's automated items in the plan and update the todos.
4. Commit the phase (see Commit and Push).

After the last phase, stop for the human's manual checks and list them by phase:

  ```
  Implementation complete: ready for manual verification

  Automated checks passed:
  - [each automated check that passed]

  Manual steps from the plan, by phase:
  - Phase 1: [...]
  - Phase 2: [...]

  Tell me when these pass, or what failed.
  ```

Leave manual-verification boxes unticked until the user confirms them.

## Stop condition

If automated checks (build / test / clippy) fail 3 times on the same phase, STOP. Report the failing diff, the last error output, and the attempted fixes. Do not loop — hand control back to the user.

## Stuck

- Re-read the code the phase touches until you can explain it.
- Check whether the codebase moved since the plan was written (`git log` on
  the touched paths).
- Still blocked: report it in the mismatch format and ask.

Spawn sub-tasks only for targeted debugging or unfamiliar territory.

## Resuming a Partly Done Plan

Ticked items are done: trust them and start from the first unticked item.
Re-verify earlier work only when something contradicts it. The goal is working
software, not ticked boxes.

## Commit and Push — MANDATORY

Before responding, commit all task-owned changes (code changes, directly related `thoughts/` files, plan checkbox updates, etc.) using explicit paths to the **feature branch** in the worktree. Never stage the entire `thoughts/` directory or unrelated edits. Leave the tree clean. Push only if the user explicitly asked; then push the current branch as a safe fast-forward, never `main`.

**Do NOT merge into main.** The branch stays as-is for review. No merge, no rebase onto main, no PR merge.

## Final Response Format

When every phase has landed, report the branch name and worktree path so the
user can review and merge.

Commit after each phase without asking, and commit only work you touched.
