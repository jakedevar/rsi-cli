# Two ways to develop with RSI

## (i) Supervised flow

You stay in the loop. Open the project (`:projects`), start a session
(`:blank <objective>`, `Ctrl-N` or `<Space>N`), answer approval prompts as they
appear (`<Space>gd` lists manager decisions awaiting you), and merge the
results yourself. Nothing is appointed and no agent lands anything. Good for a
first hour, for sensitive repos, and for learning the keys (`?` anywhere,
[keybindings.md](keybindings.md)).

## (ii) Fully managed agent branch

Agents work in sandboxes, a manager integrates, and the lander publishes to an
agent-owned integration branch (`rolling` in this repo; `agent-dev` is the same
idea under another name) without a permission prompt per action. `main` stays
operator-only: the pre-push hook refuses any agent push to `main`
(`tools/git-hooks/pre-push`, the `refs/heads/main` case), and a push to the
integration branch is only accepted from the lander (`RSI_ROLLING_LANDER=1`
marker, same file). The hook is a guard rail, not a security boundary.

### Set up on a new repo

1. **Branch.** From the default branch: `git branch rolling main && git push origin rolling`.
2. **Hooks.** Copy `tools/git-hooks/` and `tools/install-hooks.sh` from this repo,
   then run `./tools/install-hooks.sh` once (it sets `core.hooksPath` for every
   worktree). The shipped `pre-push` also runs RSI-repo-specific checks
   (test-shard inventory, operator-identifier scan); trim those for a foreign repo.
3. **Lander.** `make release-install` installs `rsi-rolling-land` next to `rsid`.
   Publish with `rsi-rolling-land --repo <sandbox> --remote origin --accepted <SHA>`
   (add `--test-filter PACKAGE=FILTER` to scope the gate). Managers can instead
   use the daemon merge queue (`AgentEnqueueLandingSource`,
   [agent-control.md](agent-control.md)); the operator switches it on and off
   with the **Rolling merge queue** setting (`<Space>,`).
4. **Manager policy preset.** Seat a manager with `:manager appoint`, then
   `:manager policy`, choose **Execute**, press `s`
   ([harness-manager.md](harness-manager.md)). For several repos use
   `:manager global appoint <projects>`, which defaults the managers it seats to
   the Execute preset.

### One prompt that does steps 1 and 4's seating

After `:projects` and `:blank`, run `:manager appoint`, then send:

> Set this repo up for managed agent development: create a `rolling` branch from
> the default branch, install the git hooks with `tools/install-hooks.sh`, and
> confirm `rsi-rolling-land` is on PATH. Never touch `main`. Report each step.

Seating (`:manager appoint`, `:manager policy`) and enabling the queue setting
stay operator actions; no single command does all four steps yet.
