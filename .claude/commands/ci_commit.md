---
description: Commit this session's changes without asking, in scoped commits
model: haiku
capability_class: lookup_fast
---

# ci_commit

Non-interactive. Commit the work this session produced. Do not ask the
operator anything; decide and commit.

## Steps

1. Inventory: `git status --short`, then `git diff` and `git diff --staged`.
   Match each changed path to work done in this session. Paths you did not
   touch belong to someone else: leave them unstaged.
2. Group: one commit per coherent unit (a feature slice, a fix, its tests, a
   doc update). Split unrelated changes; keep a change and its tests together.
3. Message: follow the style of recent `git log` subjects. Imperative subject
   of at most 72 characters; the body says why, not which files.
4. Stage and commit each group: `git add -- <explicit paths>`, then
   `git commit`. Never `git add -A` or `git add .`.

## Rules

- Include task-owned `thoughts/` artifacts in the commit that produced them.
- Never commit scratch files, stray debug scripts, secrets, or build output.
- No attribution footers unless the repository's own instructions ask for them.
- If a pre-commit hook fails, fix the cause and retry; never `--no-verify`.
- Do not push.
- Finish with `git status --short` and list each commit as `<sha> <subject>`.
