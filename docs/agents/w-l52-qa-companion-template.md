# QA companion launch template (w-l52, #133)

Canonical reusable successor to the satellite-local `w-l52-brief.md`, retaining
the newer `w-l54-brief.md` continuation authorization and TUI/E2E coverage.
Those local launch instances are not generated mirrors. Do not overwrite an
active sweep's brief or runner when updating this template.

The launching manager fills every placeholder below and names the bound Issue.
Only include the fallback authorization when that QA task is authorized to use
it. A brief cannot grant daemon capabilities. Read `AGENTS.md` and
`thoughts/shared/manager/worker-contract.md` from published `origin/rolling`;
section 3a defines this exception to section 5, so no conflicting precedence
rule is needed. These instructions launch future QA work, not a docs worker.

## Launch brief

You are the companion runner for QA sweep `<sweep>`, bound to Issue `<issue>`;
read only that Issue with `AgentGetIssue`. Fixed source pin: `<40-hex-sha>`.
The manager owns the rsid/rsid-store shard lanes, their source, jobs and state.
Launch only after the manager releases one build slot. Run your named companion
lanes sequentially, at most ONE heavy lane at a time; never touch manager lanes.

Use only `<authorized-companion-source>` at the exact pin, your own target under
your sandbox, and `<authorized-companion-output-paths>`. The manager supplies
the source allocation/cleanup instructions; this continuation exception grants
no additional worktree or sandbox custody. Do not change your sandbox branch,
merge a newer pin into the QA source, restart/deploy rsid, spawn children, enter
program mode or push `rolling`/`main`. Retain the established runner's
`CARGO_INCREMENTAL=0 CARGO_PROFILE_TEST_DEBUG=0` and cargo-slot discipline.

**Task-specific fallback authorization (include only when authorized):** You
may use worker-contract section 3a for the named companion lanes whose exact
target, source directory or environment the deployed job surface cannot
represent. Prefer caller-owned `AgentSubmitJob` jobs with `wake:"none"` and ONE
`mode:"when"` `when.jobs_terminal` completion wake with `timeout_seconds`.
Inspect the deployed catalog before deciding; a recipe must already be
declared in the allowed source and supported by the daemon. Worker `worktree`
selection is unavailable; `sandbox_session_id` cannot target test jobs.
Delegated `qa_lane` is known-shard-only and does not enable companion targets
or recipes. Do not treat its longer timeout as companion authorization.

For an unrepresentable lane, record the precise limitation and this launch
authorization, then use ONE owned detached `systemd-run --user --collect` unit
with `RuntimeMaxSec=1500`, durable log and exit record. While it is active, arm
at most ONE named, one-shot same-session `mode:"resume"` safety wake with
`in_seconds` from 1 to 600. Reuse the name; inspect `AgentListWakes` before
ending the turn. Each wake inspects the unit/evidence once, then advances or
replaces that wake and ends the turn. No `every_seconds`, fresh self-wake,
sleep/wait loop, or simultaneous completion and safety wake for the same lane.
Cancel any pending safety wake on settlement and before final handoff. Never
rearm without an active owned unit or just to wait for load/disk capacity;
report that capacity residual to the manager. Stop only your own unit. Split
capped lanes by exact filters; report unrun tests. If this authorization or a
durable backend is unavailable, report the unsupported lane instead of running
a different target or bypassing an authority refusal.

Execute `<approved-runner-and-exact-commands>` in this order, retaining its pin
assertions and source/target isolation:

1. Priority bin regressions: `<exact-priority-test-ids>`; report promptly to the
   manager through the launch-authorized reporting surface.
2. rsid bins; every rsid integration target; rsid/rsid-store doctests.
3. rsi library; rsi bins/integration targets excluding `e2e_tui`; then a separate
   `e2e_tui` run with `RSI_E2E=1`.
4. rsi-common; remaining workspace targets; remaining workspace doctests.

Before each lane, check load and disk: queue no new lane while one-minute load
exceeds 40 or free disk is below 20 GiB. Report blocked capacity to the manager.
Respect any stricter launch limits and worker-contract verification bounds.
Query `AgentQueryFailureSignatures` for each failed/interrupted test and run
ONE isolated exact-filter rerun within the same bounds. Classify NEW, FLAKE,
SEED or ENV with evidence. Use `<pin-specific-baseline-ids-and-owner-issues>`;
do not inherit baseline classifications from an older sweep.

Write `companions.md` (at most 100 lines) only in the authorized output scope:
lane run/pass/fail/skip/exit table, each failure's exact ID/signature/class,
rerun evidence, unsupported/unrun residuals and owned-unit/source cleanup.
Report the same results to the manager as authorized; it assembles final QA
artifacts. Append the handoff to your bound Issue, preserving its body prefix.
Commit task-owned artifacts before return; use worker-contract section 4's
`PIPELINE HANDOFF — IMPLEMENTATION:` and `RESULT <source-commit> issue=#<n>`
format, separately naming the tested pin and QA verdict GREEN/RED/INCOMPLETE.
End with `Friction: none | #N[, #M] | <one line, not filed because ...>`.
