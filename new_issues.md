# RSI issues to pick up on personal resources

## 1. Review RSI issue #1684: target/ reclaim

Please review RSI issue #1684 and determine whether it still needs a fix.

Context from the current rolling branch:

- Commit d9c0686aa (“Reclaim landed terminal workers' target/ inside the idle TTL and accept cross-target triple dirs (#1684)”) added reclamation of a completed worker’s `target/` cache when its commit is already on `origin/rolling`, even within the idle TTL and without disk pressure. It also accepts cross-target triple directories.
- Commit 27c749237 (“Silence git stderr in the landed-on-rolling probe (#1684)”) silenced stderr for the Git ancestry check.
- Relevant code is in `crates/rsid-store/src/sandbox/custody.rs`, `crates/rsid-store/src/sandbox/target_reclaim.rs`, and `crates/rsid/src/session/launch.rs`.

Please check the current behavior and the original issue details. If a gap remains, convert it into a focused RSI issue with reproduction steps and acceptance criteria, then fix it through the normal workflow. If the issue is already fully addressed, report that with the relevant evidence instead of opening a duplicate.

After you’ve converted this note into an issue or finished the review, delete this temporary document.

---

## 2. Stop committing thoughts/ in every project (this host's RSI #1699, parked)

Parked on 2026-10-20 because RSI isn't developed on the work laptop with company tokens. Pick it up on personal resources.

Operator request: untrack thoughts/ from every git project. It adds noise to the repositories and triggers unnecessary CodeRabbit reviews and PRs.

Root cause: `crates/rsid/src/session/preamble.rs` (`project_has_thoughts_dir`, about line 769) injects "Thoughts artifact commit policy (HARD)" into every session whose project has a thoughts/ dir. AGENTS.md, docs/agents/worker-contract.md and the command preambles repeat it. So agents force-add thoughts files even in repos that ignore thoughts/ (gong_ml ignores it, yet tracks 64 files added with `git add -f`).

Tracked thoughts files on 2026-10-20: rsi 4974, gong_ml 64, gong-mcp-pilot 16, assetwatch 1, aw-mysql-mcp-rs 1, AW-Marketplace / admin-os / people_ops_os 0.

Plan:
1. A durable per-project store outside git, e.g. `~/.rsi/thoughts/<project-slug>/`, kept as a local git repo the daemon snapshots, so sandbox reclaim loses nothing.
2. Every sandbox and main checkout gets `thoughts/` as a link to that store, so existing `thoughts/shared/...` paths keep working. Make it portable to macOS, Linux and Windows.
3. Replace the HARD commit policy everywhere with "write under thoughts/, never `git add` it; durable results go in the Issue handoff or docs/". Delete the contradicting text.
4. Then, per repo: copy the tracked thoughts/ into the store, `git rm -r --cached thoughts`, and add thoughts/ to .gitignore. Land each through that repo's normal flow, rsi last. Never rewrite history.

The full text is in this host's RSI Issue #1699. Note that Issue numbers differ between hosts: on this host #1684 is "rsi TUI keeps spinning at 100% CPU after its terminal closes", not the target-reclaim item in section 1.
