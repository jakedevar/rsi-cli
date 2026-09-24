# Rolling landing CLI

`rsi-rolling-land` prepares and publishes accepted commits to the `rolling`
branch. It requires one or more accepted source/base pairs and always runs the
repository's `scripts/rolling-landing-guard.py` on the prepared candidate.

```sh
rsi-rolling-land --repo /path/to/rsi --remote origin \
  --accepted <base-commit-id>:<source-commit-id> \
  --test-filter rsid=integration::
```

Repeat `--accepted BASE:SOURCE` for additional accepted commits. The command
accepts repeatable `--test-filter PACKAGE=FILTER` arguments and forwards each
one to the guard. Use these for an explicit focused test gate when a crate's
full suite has known baseline failures. The guard rejects a filter whose
package is not affected by the candidate.

The command requires `CARGO_TARGET_DIR` to name an existing target directory
outside the system temporary directory. It preserves that caller-provided
setting when affected-crate tests run, so candidate builds use the current
session's sandbox target and never create an untracked `target` directory in
the candidate worktree. It does not override `CARGO_TARGET_DIR`.

The command fetches `refs/heads/rolling` immediately before preparation in a
mode-0700 private shared clone, so the caller's local `rolling` branch is not
changed. The clone keeps no primary checkout; it contains the committed guard
script, object store, refs, and temporary candidate worktrees.
The fetch is bounded to 30 seconds; a stalled Git process group is killed and
reaped. Automatic and detached Git maintenance are disabled in the private
clone, including for later Git subprocesses.
The integration engine creates a fast-forward candidate or a two-parent merge
candidate while preserving source ancestry. The Python guard checks each
accepted pair for lost hunks and reports affected crates. An insertion only
counts as preserved when it maps with adjacent source context; an identical
attribute added elsewhere cannot hide a lost accepted line. The CLI turns that
plan into an ordered guard spec: `git diff --check`, each affected crate's
library tests, then `cargo check --all-targets` for every transitive workspace
dependent of those crates. Test filters apply only to directly affected crates.
The spec runs in the clean candidate worktree with bounded
process groups, command timeouts, output tails, and the caller's
`CARGO_TARGET_DIR`. If every source is already an ancestor of rolling, the
lost-hunk check still runs in plan-only mode. The command then exits without
publishing a candidate. A suspected lost hunk still runs the affected-crate
test phase before the CLI refuses publication; the hunk evidence remains a
review gate.

The CLI re-reads the remote again after guard work and before preparation. An
ancestor-only fast-forward discovered there is a stale target, not an approved
merge source: preparation fails closed before push, and the newer rolling tip is
integrated in a new candidate on retry (2026-09-23 J content guard;
`ancestor_remote_advance_during_guard_fails_closed_before_push`).

Publication sends the full candidate object ID with an ordinary Git push. A
non-fast-forward update fails; if the remote changed since fetch, the command
re-reads the remote immediately before push and reports a stale target if it
differs from the fetched tip. It also rechecks the configured push URL before
publication and after the canary. After a successful push it reads the remote ref
again and succeeds only when that exact tip equals the candidate. It then runs
the same guard spec as a canary on the verified published tip and checks the
remote once more before reporting success. If another fast-forward advances
rolling from the green candidate during the canary, it reports the distinct
`published_descendant_unverified` outcome with the observed tip; the combined
tip needs its own verification. The candidate worktree is kept
through this verification; the CLI never tears down a source worktree.
Successful output includes `candidate_id`, `candidate_kind`,
`fetched_target_id`, and `published_target_id`.

If the published-tip canary fails, the CLI creates a new child commit of the
published candidate whose tree matches the pre-landing rolling tip. It
first tries to publish this forward revert when the remote still equals the
candidate. On an observed descendant advance, it makes at most two fresh
attempts: apply the inverse landing delta to the latest tip in a private
worktree, run the affected-crate guard, and push the new child by ordinary
fast-forward. It rechecks the configured push URL after each retry guard,
immediately before the push. A conflict, red guard, unrelated tip, or exhausted
retries leaves rolling untouched by the revert and reports the observed tip. The accepted
source remains in Git ancestry. An uncertain publication or unverified forward
revert retains the private clone and reports `recovery_path` for reconciliation.
The invoking lead owns that path until the remote outcome is settled. Inspect
the remote before any retry; after the tip is verified, the lead removes the
retained recovery directory (2026-09-23 Slice 3; `recovery_path` is emitted only
for uncertain publication or an unverified forward revert).

Failure output starts with `publication_status`, `landing_outcome`, and
`exit_code`:

| Exit | `landing_outcome` | Action |
| --- | --- | --- |
| 1 | `not_published` | Candidate was not published; inspect the refusal before retry. |
| 2 | `published_cleanup_failed` | Green landing published; clean the retained private candidate. |
| 3 | `publication_unknown` or `canary_red_revert_unknown` | Reobserve the remote; retain recovery custody. |
| 4 | `canary_red_reverted` | Red landing was forward-reverted; do not record integration. |
| 5 | `canary_red_unreverted` | Rolling may remain red; retain recovery custody and repair it. |
| 6 | `published_unverified` | Candidate is published but the final verification failed. |
| 7 | `published_descendant_unverified` | Candidate is an ancestor of the newer tip; verify that combined tip. |

Failures after candidate creation include `candidate_id` and
`fetched_target_id`. A verified published failure also includes
`published_target_id`; an ambiguous remote response includes
`observed_target_id` when available. A forward revert includes
`forward_revert_id` and its `forward_revert_status`. At most two descendant
retry attempts run after the first stale-tip refusal (`retry_forward_revert`).
Check the remote tip before retrying an `unknown` outcome or removing recovery
custody.
