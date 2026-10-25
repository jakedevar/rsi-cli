# Worker contract reference

The launch preamble stays brief because every leaf worker pays its prompt cost. `AGENTS.md` holds canonical repository rules. The including command supplies role, task, return schema, and any tighter limits. A parentless session directly addressed by the human may answer plainly; do not infer that status from a title or missing schema.

Work on the assigned Issue. If it omits intent or acceptance criteria, state
what you build to in the handoff. Before the live context threshold is reached,
commit a checkpoint and hand off a precise partial result; `AgentGetAuthorityCatalog` serves `worker_context_cap_tokens`.

## Source ancestry

Start from your allocated source on the assigned sandbox branch and preserve
its ancestry. Merge `origin/rolling` when needed; never rebase a source stack.
The queue's lander integrates `rolling`. Custody and pending verification wakes
require the allocated source to remain an ancestor of your head.
`git commit --amend` is refused in sandboxes, even for unpublished commits; make a new descendant commit instead (for example, a separate formatting commit).

If another queued source claims your migration number first, the merge queue renumbers a declared provisional migration to the next free head with `scripts/repo-guard rolling-migration-renumber`; never renumber by hand.
A `git merge-tree` add/add conflict on `vNNN.rs` is this expected collision; an undeclared migration is refused as `queue_migration_out_of_order`.
A slice based on an unlanded sibling migration lists the skipped numbers in its declaration's `pending_predecessors`.
See `crates/rsid-store/AGENTS.md`, "New migrations", for the migration rules.

## Verification bounds

Settings registry rows have two desktop steps: after adding or changing a row
in `crates/rsi/src/settings_registry.rs`, run `node desktop/scripts/settings.mjs`
from the repository root and commit `desktop/src/settings.generated.ts`. Map new
rows in `widgetFor` / `OVERLAY_ROW_IDS` in `desktop/src/settings-model.ts` and
register overlays in their owning desktop view. Intentional TUI-only preferences
also need `TUI_ONLY` in `desktop/tests/settings-state.test.mjs` and `tuiOnlyRows`
in `desktop/parity-audit.json`. The desktop lane checks regeneration and mapping.

Desktop workers, including satellite workers on macOS and Windows, run
`cargo test --locked --manifest-path desktop/src-tauri/Cargo.toml`; an unlocked
test can silently repair a stale lock and hide a merge-queue failure. Dependency
changes in the desktop's path dependencies (including rsi-common and rsi-graph)
must refresh and commit `desktop/src-tauri/Cargo.lock` in the same source as the
root lock. The shared desktop lane refuses a stale lock as `desktop_lock_stale`
and names the repair: `cargo update --workspace --manifest-path desktop/src-tauri/Cargo.toml`.

Acceptance is a green `scoped_test` receipt whose `receipt_head` equals the exact final committed HEAD. Run one `scoped_test` per final head (`AgentSubmitJob {"kind":"test","wake":"none","params":{"scoped_test":{"base":"origin/rolling"}}}`, then ONE `AgentScheduleWake {"mode":"when","message":"scoped_test finished: read the receipt","when":{"jobs_terminal":["<job id>"]}}`; `message` is required, a call without it is refused `invalid_input`); it derives the touched-module filters itself, and the merge queue derives the landing filters, so do not run `scripts/check-touched-shards` by hand. After a red run caused by your change, fix the cause, commit, push, and run exactly ONE more `scoped_test`. A receipt with `failure_class: "infrastructure_interrupted"` and `ok: false` means the compile/test phase failed only from SIGINT, SIGKILL or SIGTERM, with no compiler diagnostic or failing test. Such a run does not count toward the two-run limit: rerun once at the same committed head. If that rerun is interrupted too, hand off the infrastructure blocker. A job that settles `job_admission_timed_out` carries `failure_class: "infrastructure_admission_timeout"` (result and receipt): it never got a build slot, disk or memory floor or lock, so nothing compiled or ran. It does not count toward the two-run limit either; the receipt `admission.blocked_by` and the result detail name the slot holders (lease, pid, unit, seconds held) or the floor that held it. Resubmit once at the same committed head after that clears; if it times out again, hand off the named blocker instead of looping. A mixed signal plus compiler error or test failure is an ordinary red and counts toward the limit. Otherwise, never loop or retry the same head; if the second counted run is still red, hand off with the reds named instead of starting a third. For compile errors while you work, submit a `build` job with `command:"check"` and `package` (`AgentSubmitJob {"kind":"build","params":{"command":"check","package":"<crate>"}}`): it rides the governor's separate check lane (#2430) instead of queuing behind full builds and test runs, so use it before `scoped_test` rather than spending a scoped_test round on a type error. Compile feedback is optional and only when the launch brief permits it: a brief that sets a verification policy (`scoped_only`) or forbids a broad build wins, and a prose-only, docs-only or source-only change needs none. A `check` job passes `--keep-going`, so cargo does not stop at the first failing crate; when more than one crate may be affected (a shared type or signature change), you may submit one `{"kind":"build","params":{"command":"check","workspace":true,"all_targets":true}}` job instead of one single-crate job per crate: it reports every crate's errors, test targets included, in one round (#2516). To verify shard features (`--no-default-features --features test-shard-X`) never start your own cargo unit: submit one `{"kind":"build","params":{"command":"test_no_run","package":"rsid","shards":["store-01","other-02"]}}` job (#2692). It builds the shards in `rsi-build.slice`, cleans the package between them so the target stays small, and its receipt names each shard's exit code and warning counts (judged by the #1897 gate). For a new runtime or schema assertion you are unsure of, you may first submit ONE exact package job per touched crate (`{"kind":"test","params":{"package":"<crate>","lib_only":true,"exact":true,"filters":["<module>::tests::<name>"]}}`, full module path): optional early feedback on the working tree, not committed bytes, so never acceptance. When `filters` is non-empty, it exits 0 and its whole log proves libtest ran no test (0 passed and 0 failed in every test binary), it settles `failed` `job_filter_matched_no_tests` with exit_code 0 kept; a custom harness or unreadable log keeps the ordinary result. Do not use it for `rsid-store` migration suites (disk-backed scratch, slow); the final committed `scoped_test` still derives the acceptance scope. A `scoped_test` has a bounded 60-minute default (or a higher operator test default, capped at 180); allow healthy progress inside its admitted budget rather than stopping it at 20 or 30 minutes. Inspect stalled, over-broad or unbounded work and host CPU pressure; cancel your own runaway job with `AgentCancelJob` and report its command, elapsed time and log. Other runs stop at twice their expected time or the brief's deadline, whichever comes first (30 minutes if neither is known). An explicit brief deadline still applies to `scoped_test`. Stop only processes or units you started; `AgentHalt` stops a provider session, not a detached job. Do not repeatedly restart timed-out runs or use long sleep-poll loops. The suggested filters are bounded and exact (named tests, not module prefixes); a receipt with `broad: true` names a filter that may not finish in a worker's budget, so run it in parts or say so in the handoff (usage text in the script).

For a baseline red you did not cause, first query `AgentQueryFailureSignatures` with the failing test name and title-search open Issues with `AgentListIssues` (`title_contains` set to that name). If a fix exists or is queued, leave the test file untouched and name its Issue as `fixes_issue: <N>` in the handoff. If none exists, file one Issue naming the failing test and keep its repair in a separate small, one-file source the manager can land first; never fold it into the feature branch. Name that Issue and the baseline red in the handoff.

When a baseline red has no signature record, the admitted project snapshot can name an open, non-archived `qa` Issue by exact test identity (the QA `Test:`/`Failing tests:` fields or a QA title's trailing test name). The receipt and output label this advisory, not a signature match; signature records take precedence and advisory context never changes classification or the run's verdict.

A receipt with a non-zero `exit_code` is still verified when its `new_failures` is an empty list and every red is listed in `baseline_reds` (or named by `AgentQueryFailureSignatures`) (#3770). The landing gate is no new failures relative to `rolling`, and the daemon's `require_verified` check and `AgentCheckLandingCandidate` (manager-only; a worker is refused `candidate_not_authorized`, so read the receipt instead) apply the same rule. Hand off complete with the receipt's job id and exit code, and the baseline reds with their Issues named. Do not end `blocked` and do not ask the manager: a `technical_impasse` parked only on baseline reds costs the manager a turn and a parked execution for a ruling that is already written here. Block only when `new_failures` is non-empty or `null` (the run failed before the baseline comparison), or the run was interrupted, timed out or failed to build. The starter node prompts (`issue-pipeline`, `issue-pipeline-reviewed`, `issue-pipeline-post-review`: implement, repair and pre-land repair nodes) carry the same rule.

Read the receipt's `merges_cleanly_onto_rolling` (#348): `scripts/scoped-test` runs a fetch-free `git merge-tree --write-tree origin/rolling HEAD` at the end and records `merges_cleanly_onto_rolling` (`true`, `false`, or `null` with `merge_check_reason` when git could not say) and, when false, `conflict_paths` (the first 32, `conflict_paths_total` the full count). It is informational and never changes the run's verdict, so a green run can still name a commit that no longer merges. When your receipt shows `false`, merge `origin/rolling` into your branch (no rebase), resolve the named paths, commit and push; verify the new head within the two-run limit above, or hand off the remaining blocker. `AgentCheckLandingCandidate` reports the same fact (and a fresh check against the current `origin/rolling`) as `candidate_conflicts_with_rolling`, and the worker roster's job summary carries it.
For the whitespace check of your source as merged onto rolling before a handoff, run `scripts/check-merge-whitespace` (default `origin/rolling HEAD`): it names conflicting paths in plain words (exit 2) instead of the `fatal: ambiguous argument` that `git diff --check origin/rolling $(git merge-tree --write-tree origin/rolling HEAD)` prints on a conflict, and runs `git diff --check` only on a clean merge (#96).

New agent verbs follow `docs/agent-control.md` ("Adding an agent verb").
Settings text and key-table changes also need the `manual` job and regenerated
docs. New agent verbs or native tools need RPC and harness-tool tests; name
missing coverage in the handoff if the derived plan omits them. New rsid or
rsid-store library tests use their source file's shard gate immediately before
`#[test]`; `scripts/check-rsid-test-shards.py --require-gates` audits them.

## Scratch and output

Fixtures that allocate sandboxes or require real disk paths use
`crate::test_support::disk_backed_tempdir(label)` rather than `TempDir::new()`.
Scoped tests already run tmpfs parity when available; report unavailable parity
as such, without adding a second verification run. For manually allocated test
scratch, use `scripts/rsi-scratch-mkdir worker <name>` (or `var <name>`) so reclaim
can track it, and remove it after use; do not adopt unrecorded scratch yourself.

`/tmp` is tmpfs on the hub and counts against RAM and swap (hub #3629 saw 15 GB
there, 13 GB of it a copied `~/.rsi`, while swap was full). Never put large
scratch on it: database copies, `target` directories, fixtures, clones or logs
over a few hundred MB. Use a disk-backed path (`scripts/rsi-scratch-mkdir`, or a
subdirectory of your sandbox outside tracked paths) and remove it before your
handoff. The governor holds heavy builds on projected memory headroom, and the
`hub_memory_high` health condition wakes the manager when memory runs low.

Keep large build/test output out of the prompt with `scripts/rsi-spill -- <command>`
or `--shell '<command>'`; inspect only needed ranges with `scripts/rsi-spill show`.
The wrapper preserves exit status. Claude's hooks apply this automatically;
other CLI workers call it explicitly.

## QA companion continuation

Only an explicit launch authorization enables the fallback for named companion
lanes at a fixed source pin. Follow `docs/agents/w-l52-qa-companion-template.md`:
prefer durable jobs; if they cannot represent the lane, use one owned
`scripts/systemd-test-fallback` unit and one named, one-shot `resume` wake while
it runs. Cancel that wake on settlement. This grants no extra source custody,
child, deployment or landing authority. Ordinary workers use completion wakes
for their own verification jobs and do not spawn children or enter program mode.

## Language policy and verification

The operator forbids Python across every project, including scripts, tooling
and tests. Prefer Rust; Go is permitted when needed. Never write, extend or run
Python yourself: no `python3` in a shell, no new `.py` file, no Python check by
hand. This supersedes older briefs and runner instructions. The daemon's
`scoped_test` job, `scripts/cargo-slot`, the lander and the installed hooks are
still Python. They stay in service only as a transitional bridge, reached
through `AgentSubmitJob` and the automatic hooks, until their Rust ports land
(AGENTS.md "Build and test", #3482). A port keeps the gate's coverage, bounds,
custody and receipt semantics. If no allowed runner exists for a check, report
a tooling blocker and preserve the committed work. Never omit a required check,
and never describe an unexecuted or zero-test run as green. The bridge's
remaining Python suites in `scripts/tests` and `tools/tests` are stdlib
`unittest` suites that only the daemon's jobs and the
lander run, never you. A planned runner that is missing is the typed
`runner_unavailable`: the receipt's `python` entry is red with
`executed_tests: 0`. Report it as the tooling blocker it is.

### Build restrictions and hook publication

Distinguish three scopes in a source-only brief (#676):

- **No hand-run builds, automatic hooks expressly allowed:** ordinary commits
  may run the existing hooks, including guard-only Cargo compilation under the
  transitional bridge. This grants no manual guard/build/test/job permission.
- **Literal no builds:** use `RSI_REPO_GUARD_NO_BUILD=1` on each Git command that
  can run guard hooks (for example `RSI_REPO_GUARD_NO_BUILD=1 git commit -m 'fix: ...'`).
  Every hook guard resolution inherits it. The wrapper runs the actual checks
  only with a content-matching, capable prebuilt in the stamped caller target;
  otherwise it refuses nonzero without Cargo. Cold targets or changed compiled
  guard inputs require a separately authorized, platform-compatible prebuild.
  External integration-test-only changes preserve the existing source stamp.
- **No script/guard execution:** no-build mode still executes guards, so a
  worker under this restriction cannot commit through executing hooks. Preserve
  the changes and report the blocker; never suppress hooks or change their
  persistent configuration.

No-build mode is fail-closed resolution, not deferred-check publication or a
full cold-worker solution. Missing/stale/incompatible/incapable guards and guard
failures yield no success receipt. The build-enabled default, mandatory staged
checks, protected refs, exact-head hub acceptance ownership and merge-queue gates
remain unchanged. Keep unexecuted verification **UNRUN**; neither source-branch
publication nor another session's receipt waives acceptance. See the guard
[wrapper contract](../../crates/rsi-repo-guards/README.md#the-wrapper).

## Script suites (Python is banned)

Python is forbidden in this repository (#358): never run `python`, `python3` or `pip`, and never write a `.py` file. The tests of the repository's scripts are bash TAP suites, `scripts/tests/test_<stem>.sh` and `tools/tests/test_<stem>.sh`, each declaring `# suite-gate: tap` and sourcing `scripts/tests/lib/tap.sh` (a case is `run_case "name" function`; an assertion that fails prints why and leaves that case; the file ends with `tap_done`). The scoped runner's residual lane selects a suite the way the retired Python gate did (a changed suite selects itself; `test_<stem>` matches a changed script's stem; a suite naming a changed file's basename observes it; `# suite-gate: scans <dir>` selects it for any change under that directory) and runs `bash <suite>` in the candidate checkout under the run's PID isolation with a 300 s ceiling per suite. The verdict is the runner's: red on a non-zero exit, on any `not ok` (numbered or not), on malformed or incomplete TAP (no `1..N` plan, a plan that disagrees with the case lines, a case out of sequence, `Bail out!`), on a timeout, and on a suite that executes no case (`no_tests_executed`: a case reported `# SKIP` is recorded in `skipped_tests` and is not an executed test, so an all-skipped suite is red, not green; keep one case that runs on every host). A host without bash reports the typed `runner_unavailable` (the runner, `executed_tests: 0`, a red job) and no other runner is substituted; report it as the prerequisite it is, not a test result. A suite still written in Python is a red `incomplete_contract` entry owned by #366 until it is ported and its `.py` deleted in the same change. Run a suite by hand with `bash scripts/tests/test_<stem>.sh`; `TAP_SELECT=<text>` runs only the cases whose name contains the text (a debugging aid, never acceptance). Exit-status shell tests without the declaration (`scripts/tests/test_rsid_*.sh`, `check-corpus-pins.test.sh`) run through `scripts/tests/test_shell_tests.sh`.


## Fixture Git

A fixture or reproduction repository is never reached by `cd <dir> && git ...` or by a bare `git` whose working directory is implied. If the `mkdir`, `mktemp` or `cd` fails, the shell carries on and Git acts on the worktree you are standing in, which in a sandbox is your assigned branch (hard rule 2; #2291 made two junk commits and a `reset --mixed` that way). Instead:

- Create the directory, check that it exists and is outside your sandbox root, then run every Git command as `git -C <absolute dir> ...` (or with explicit `GIT_DIR` and `GIT_WORK_TREE`). Never reset, check out, switch or branch-change the assigned worktree to repair a mistake; stop and report.
- Rust or Go fixture helpers must require an absolute, existing directory whose resolved path is outside every protected checkout and any `GIT_DIR`/`GIT_WORK_TREE`. Run Git with the repository-location environment removed and a ceiling above the scratch directory, failing before Git starts on an unsafe path. A sandbox's own `target/.rsi-tmp` is inside the sandbox and is rejected. Allocate repository fixtures in a disk-backed host temporary directory outside every protected root without changing process-wide temporary-directory state. Tests of a fake assigned repository build it there; none exercises a denied mutation in the live sandbox. In this repository Rust fixtures and tools use `rsi_repo_guards::scratch_git::Policy` (`Policy::from_process(checkout)`, then `tempdir(prefix)` and `command(dir, args)`; the native environment is never lossily converted); `scripts/scratch_git.py` is reference text for the same rules until its remaining Python callers are deleted.
- The custody scan (`rsi_common::custody_scan`, shared by the CLI hook and the Harness tools, #2571) refuses a `cd`/`pushd` to a scratch or temporary location (a `/tmp`-style path, `$TMPDIR`, `mktemp`, a variable holding one) followed in the same command line by a repository-changing Git command (`commit`, `add`, `reset`, `update-ref`, `branch <name>`, `tag <name>`, `config` writes, ...) that is not pinned by a literal absolute path: `-C /abs`, or both `--git-dir /abs` and `--work-tree /abs` (or `GIT_DIR=/abs GIT_WORK_TREE=/abs` written in front of that same command). A variable, `$(...)`, `~` or relative path is not a pin because it may be empty, an earlier `export GIT_DIR=` pins nothing later, and a later `cd` to a sandbox path does not clear the risk. Reads, pinned commands, ordinary assigned-branch `commit`/`fetch`/`merge`/`push` and scratch words inside quoted messages are not refused. Limits: it reads one command line, not earlier tool calls; it does not expand `${VAR}` (brace splitting), follow functions, or know what a variable holds, so a pin must be a literal path (`git -C "$dir"` after a scratch `cd` is refused). The guard is a cooperative guard rail, not a security boundary.
- A scratch clone outside every sandbox (#331) is not this sandbox, so a brief may say "clone to `/tmp/<dir>`, then split or check out there": `git clone --no-local <sandbox> /tmp/<dir>/<repo>`, then name the clone by a literal absolute path, either `git -C /tmp/<dir>/<repo> checkout --detach <sha>` or `cd /tmp/<dir>/<repo> && git checkout --detach <sha>` (a straight line: `&&`, `;` or newlines, no `||`, pipe or subshell before the git command). `checkout`/`switch` is refused only when that worktree is this sandbox, a worktree under `RSI_SANDBOX_BASE`, or cannot be resolved. A git command that is not a built-in (`git subtree split --prefix=desktop -b desktop-only <sha>`) runs there too when the line names the directory the same way, no alias of that name exists in that directory's configuration (the hook reads it with `git -C <dir> config`), and git ships an executable `git-<name>` in `git --exec-path`. The repository must resolve outside the assigned sandbox, every tree under `RSI_SANDBOX_BASE` and `~/rsi`, and share neither their common directory nor a worktree identity. Symlinked scratch parents are allowed after canonicalization; symlinks into managed trees are refused. Every `-C` component must be a literal absolute directory. Direct `git-NAME` executables, `PATH`/`GIT_EXEC_PATH` assignments, either `--exec-path` override spelling, `env -C`, `--git-dir`/`GIT_DIR` or `HOME`/`GIT_CONFIG_*` get the `may be an alias` refusal. Force push, `reset --hard`, bare `stash`, forced branch updates and aliases stay refused in the clone as everywhere. The fallback that always works without a scratch clone: run exactly `git subtree split --prefix=<dir> <sha>` (no additional options; it only writes objects) from the sandbox, then in the scratch repository `git -C /tmp/<dir>/<repo> fetch <sandbox path> <split sha>` and `git -C /tmp/<dir>/<repo> update-ref refs/heads/<name> <split sha>`.

## Evidence and return

**Branch publication before handoff.** Push the committed HEAD to your assigned
sandbox branch with `git push origin HEAD:refs/heads/<branch>`, never `main` or
`rolling`, and never force. If the push fails for any reason other than a
non-fast-forward rejection (including `fatal error in commit_refs`), retry the
same non-force push exactly once. A non-fast-forward rejection requires fetching,
inspecting and integrating the remote ancestry before pushing again; do not
blindly retry it.

After a successful initial push or retry, run
`git ls-remote origin refs/heads/<branch>` and require its returned SHA to equal
`git rev-parse HEAD` before reporting that SHA as published. If the retry fails,
the remote read fails, the branch is missing or the SHAs differ, report the
publication blocker and observed errors in the handoff; do not claim publication
succeeded.

`[observed]` means executed or inspected in this pass; cite the actual result. `[source]` means extracted from an authoritative source with an exact location or reproducible extraction. `[inferred]` means reasoned but unverified. Mechanically extract counts, schemas, and field sets. Check the current tool surface before claiming an exact helper or payload shape. Do not use inference as acceptance or success evidence, or call an inference-bearing plan implementation ready without resolving it.

**Model identity.** The daemon prepends an `RSI session identity` line (provider, model, effort) to every launch. Report an author or reviewer model only from that line, never from the model names in the router table, which name routing classes. Leads cite the session row, not a worker's self-report (#597).

**Replay against cited rows (#2221).** When the Issue cites `[observed]` database rows (counts, ids, a stuck population), a green test over rows you built yourself is not acceptance. The handoff's acceptance evidence includes a read-only replay of the new predicate (the SQL, or the store call, as a `SELECT` against the live DB; diagnostics SQL is allowed) over the cited rows, with the matched count before and after the change, for example `matched 0/26 -> 26/26`. Take the row shape from what the daemon writes (read a real row, or the production writer), never from a shape you invented. If the cited rows are unreachable from your sandbox, say so in the handoff and name the check that would settle it; do not claim the outcome. A reviewer of a custody, reclaim or lifecycle fix repeats the replay on the final commit.

A worker's final answer to a master has `PIPELINE HANDOFF — <STAGE>:` on the first nonblank line (em dash, uppercase stage, trailing colon). Use the including command's stage, body schema and per-field caps. Issue implementation workers use `PIPELINE HANDOFF — IMPLEMENTATION:`; reviewers use `PIPELINE HANDOFF — REVIEW:`. Unless the including command supplies another schema, put `RESULT <full sha> issue=#<n> tests="<exact test command>" status=<green|red: reason|partial: reason>` on the next line for implementation, or `REVIEW <sha> issue=#<n> verdict=<approve|changes>` for review. Return the outcome, evidence paths, checks, commit, and blocker if any. Claim a named test passed only from completed execution evidence (its `SCOPED_END`/`PASS` log line, typed per-test receipt data or exact test output); a `changed_modules` entry, filter, selected count, compile success or `gate_covers_rest` is plan intent, not a run. Report the executed count and label touched tests the bounded plan omitted `UNRUN` or `source-inspected`, naming who covers the residual (#250). Prefer file and line references over excerpts; leave code and detailed artifacts on disk. If incomplete, report a precise partial result and observed blocker evidence. Budget or review exhaustion calls for continuation, not a fabricated human gate. The handoff's last line is `Friction: none | #N[, #M] | <one line, not filed because ...>` (#1332); `rsi-contract-validate` accepts it and does not yet require it. The daemon's stranded check also accepts a near-miss header line (`**IMPLEMENTATION HANDOFF — ...**`: markdown emphasis, `<STAGE> HANDOFF` order, no colon) as a final report and annotates the terminal watch `handoff_header_normalized: <STAGE>` (#2525); `rsi-contract-validate` still requires the exact form. A Completed worker with no marker line in its final turn is labelled `no-result: stranded`; when it published a handoff to its bound Issue the watch says `no-marker: issue handoff published, read it (not a verified result)` instead (#187), so a hold, a partial and a finished result are told apart by reading the record, never from the label.

**Already satisfied (#2651).** An Issue-pipeline implement worker whose Issue's acceptance already holds on its base (a stale red, a duplicate, a sibling landing) makes no commit and hands off `status: already_satisfied` with `evidence_commit: <lowercase 40-hex commit>` and its verification lines (tests run, results), with a clean worktree, instead of `status: complete`. The executor accepts it only when the commit is an ancestor of the attempt's base, the worktree is clean and the attempt added no commit; it then settles the node `succeeded` without a result commit, treats land, review and follow-up as dead paths, settles the execution `succeeded` and closes the Issue with a note naming the evidence commit and the verification lines. A missing, malformed or non-ancestor `evidence_commit` fails the node `handoff_invalid`. The evidence commit must also be an ancestor of a freshly fetched `origin/rolling` (#3045): when it is only reachable from a supplied `base_commit` that is not on rolling, the work is not landed, so the executor treats the handoff as the #2732 `complete` handoff at that base (land runs, the Issue is not closed here); when that route does not apply (a repair, retry or non-first node) it fails `handoff_invalid`. `evidence_commit` is valid on no other status. The handoff must also carry proof (#3772): `summary:` as a block with one `AC<n>: <evidence>` line for each of the Issue's acceptance criteria (AC1, AC2, ... in the order the Issue lists them; the evidence is a path:line, commit, test name or measurement, never a bare word such as `ok`), and `tests:` (or `tests_added:`) as a block of `<command>: <result>` lines (`cargo test -p x --lib -- --exact a::b: ok`). Example: `summary:` then `- AC1: crates/x/src/lib.rs:10 returns None` and `- AC2: test a::b`, then `tests:` then `- cargo test -p x --lib -- --exact a::b: ok (1 passed)`. The daemon checks only that shape (#3808: one numbered line per criterion, the Issue's list items under its "Acceptance criteria" heading, or one line when it states none; `<command>: <result>` tests), not whether the evidence is true; a claim of any other shape (`summary: all criteria hold`, `tests: cargo test passed`) fails the node `handoff_invalid` with `already_satisfied_unproven` and the Issue stays open. An `operator-request` Issue is never auto-closed on this word: the node still settles `succeeded`, its `issue_close` is `held` with code `manager_acknowledgement_required`, and the manager (woken at once, since the Issue is not Closed) acknowledges by closing it or reopens the work.

**Verify supplied work (#2732).** Satellite or operator work that needs hub verification is run through the starter: start `issue-pipeline-post-review` with `base_commit` set to the supplied commit. When that commit is not an ancestor of a freshly fetched `origin/rolling` (git positively says so; a fetch outage counts as "cannot say" and keeps the rule below), the first implement node (whose base is the execution's `base_commit`) verifies the head unchanged and hands off `status: complete` with `commit` equal to that base and its own passing `scoped_test` at it, making no new commit. The executor accepts that as the node result (`result_commit` = the base, `changed: false`), then land and review run as for any commit. Not `already_satisfied`: that closes the Issue without landing. With the base already on `origin/rolling`, or for a repair or later node, the rule stays: `complete` without a new commit fails `handoff_invalid: node handoff requires a new commit`.

If a `## Stage contract` appears, `rsi-contract-validate` checks `### Inputs`, `### Process`, `### Outputs`, `### Verify` in order. Inputs must name a static artifact or explicit `rg`/glob discovery budget. The block is optional for historical handoffs; an incomplete block fails validation. The including command may require it.

Write a handoff document after other edits and commits. If more files change afterward, refresh its recorded `git_commit:` before return. Commit task-owned `thoughts/` artifacts. Use the worktree-isolated `scripts/rsi-stash` interface for stash operations. For issue control authority and payloads, read `.agents/skills/rsi-agent-control/SKILL.md` on demand. For disk pressure during builds, check `df -h /tmp .` before diagnosing SQLite write errors as product defects. A scratch write refused with `Disk quota exceeded` (EDQUOT) while `df` shows free space is a per-user quota on the mount (#565): `rsi-rpc scratch-quota [PATH]` prints the mount's quota options and your usage. Pass a control payload with `rsi-rpc <Verb> --params -` (stdin) instead of a `/tmp` file, and capture evidence through a pipeline only under `set -o pipefail`, so a failing producer or a refused write cannot record a red gate as a pass.

Reconcile a plan's conflicting requirements before implementation. Assert positive user-visible identity in tests. The canonical form of these repository rules is `AGENTS.md`.
