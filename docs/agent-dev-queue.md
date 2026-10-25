# Retired agent-dev / batched-QA pilot

The opt-in, manually driven repository pilot is retired by #717 following the
#477 research and PM decision. Its CLI, `scripts/agent-dev-queue.py`, and all 52
pilot-specific cases in `scripts/tests/test_agent_dev_queue.py` are removed
together. Those cases retire with the feature; they are not ported or replaced
by daemon QA coverage. The prior implementation and manual remain in Git history
at `350eb12ac06e8aa1e997a5fa7141cce185c9da36`.

Preserve all existing pilot state: journal and source/batch retention refs, Git
objects, workspace records, source and QA worktrees, caches, configuration, job
and terminal records, receipts and logs. Repository retirement performs no
inspection, migration, cleanup, process termination or custody settlement of that
state. Actual operator use, external automation and in-flight work remain
unknown; removal does not establish runtime inactivity. Any recovery or custody
decision belongs to the authorized owner, with the saved evidence preserved.

The removed CLI's initialization, intake, batching, run, status, eligibility,
recovery and accounting commands are no longer available. A delayed self-worker
launch that resolves the deleted driver path can fail. Saved receipts bind the
original driver, interpreter, configured executables, environment and workspace
inputs; another driver cannot be assumed compatible. This retirement provides
no recovery command, compatibility wrapper or automatic import of old state.

Use the independent supported [daemon merge queue](agents/landing-and-gates.md)
for new work through its existing admission and landing gates. It does not adopt
the pilot's saved state or make its receipts valid for daemon queue admission.
