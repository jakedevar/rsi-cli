//! Durable driving of catalog command nodes (#635, plan §2.4, §3.2).
//!
//! A command attempt allocates a fresh sandbox at its fork commit, records
//! the sandbox and its `pre_head` before the op starts, then runs the
//! daemon-built op in its own process group under the daemon-wide
//! `topology_max_concurrent_build_nodes` cap. After every op, and for any run
//! an earlier incarnation left behind, the postcondition decides:
//! - HEAD == `pre_head` and a clean tree (ignoring `target/`, `.rsi-tmp/`):
//!   the exit code settles the attempt; a stale run settles `interrupted`
//!   and re-runs only as a **new** `attempt_no`.
//! - anything else: `failed(sandbox_mutated)` recorded as a preserved-work
//!   block. The op is never re-run in place.

use rsi_common::types::{CatalogOp, TopologyStep};

use crate::error::Result;
use crate::topology::catalog::{self, CommandOutcome, CommandPoll, SCRATCH_DIRS};
use crate::topology::custody::{self, TopologyForkSource, node_pin_ref};
use crate::topology::executor::{Executor, NodeEffects, failed, node_data};
use crate::topology::steps::WorkflowSteps;
use crate::topology::store::{
    self as rows, AttemptRow, AttemptStatus, ExecutionRow, ExecutionStatus, Settlement, failure,
};

fn command_op(execution: &ExecutionRow, node: &str) -> Result<CatalogOp> {
    let steps = WorkflowSteps::from_workflow(&execution.definition)
        .map_err(crate::error::DaemonError::InvalidParam)?;
    match steps.step(node) {
        Some(TopologyStep::Command { op }) => Ok(op.clone()),
        _ => Err(crate::error::DaemonError::InvalidParam(format!(
            "node {node} is not a command node"
        ))),
    }
}

impl<E: NodeEffects> Executor<E> {
    pub(crate) async fn drive_command(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
    ) -> Result<bool> {
        match attempt.status {
            // A recorded sandbox means the op may have started: never start
            // it again in place; settle it from the postcondition.
            AttemptStatus::Launching if attempt.sandbox_root.is_some() => {
                self.settle_stale_command(execution, attempt).await
            }
            AttemptStatus::Reserved | AttemptStatus::Launching => {
                if execution.status == ExecutionStatus::Cancelling {
                    self.settle(
                        execution,
                        attempt,
                        AttemptStatus::Cancelled,
                        Settlement {
                            failure_class: Some(failure::CANCELLED),
                            ..Settlement::default()
                        },
                    )
                    .await?;
                    return Ok(true);
                }
                self.launch_command(execution, attempt).await
            }
            AttemptStatus::Running => self.observe_command(execution, attempt).await,
            _ => Ok(false),
        }
    }

    async fn launch_command(&self, execution: &ExecutionRow, attempt: &AttemptRow) -> Result<bool> {
        let cap = self.effects.build_node_cap().max(1);
        {
            let store = self.store.lock().await;
            if !rows::claim_command_slot(&store, attempt.id, self.boot_id, cap)? {
                // At the daemon-wide cap: wait for a slot (tick or wake).
                return Ok(false);
            }
        }
        let Some((op, sandbox, pre_head)) = self.prepare_command(execution, attempt).await? else {
            return Ok(true);
        };
        {
            let store = self.store.lock().await;
            rows::record_command_sandbox(&store, attempt.id, &sandbox, &pre_head)?;
        }
        match self
            .effects
            .start_command(attempt.id, sandbox.clone(), op)
            .await
        {
            Ok(pgid) => {
                let update = {
                    let store = self.store.lock().await;
                    if let Some(pgid) = pgid {
                        rows::record_process_group(&store, attempt.id, pgid)?;
                    }
                    rows::mark_running(&store, execution.id, attempt, None)?
                };
                self.publish([update]);
            }
            Err(error) => {
                self.settle(
                    execution,
                    attempt,
                    AttemptStatus::Failed,
                    failed(failure::LAUNCH_REFUSED, &error.to_string()),
                )
                .await?;
                catalog::reclaim_cache(&sandbox);
            }
        }
        Ok(true)
    }

    /// Resolve the op and allocate a fresh sandbox exactly at the fork
    /// commit; any refusal is settled here (`None`).
    async fn prepare_command(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
    ) -> Result<Option<(CatalogOp, std::path::PathBuf, String)>> {
        let refusal = match self.try_prepare_command(execution, attempt).await {
            Ok(prepared) => return Ok(Some(prepared)),
            Err(refusal) => refusal,
        };
        self.settle(
            execution,
            attempt,
            AttemptStatus::Failed,
            failed(failure::CUSTODY_REFUSED, &refusal),
        )
        .await?;
        Ok(None)
    }

    async fn try_prepare_command(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
    ) -> std::result::Result<(CatalogOp, std::path::PathBuf, String), String> {
        let op = command_op(execution, &attempt.node_id).map_err(|e| e.to_string())?;
        let fork = TopologyForkSource::verified(&execution.repo_root, &attempt.base_commit)
            .map_err(|e| e.to_string())?;
        let sandbox = self
            .effects
            .allocate_command_sandbox(attempt.session_id, fork)
            .await
            .map_err(|e| e.to_string())?;
        let observed = custody::observe_sandbox_excluding(&sandbox, SCRATCH_DIRS)
            .map_err(|e| e.to_string())?;
        if observed.dirty || observed.head != attempt.base_commit {
            return Err("fresh command sandbox does not match its fork commit".into());
        }
        Ok((op, sandbox, observed.head))
    }

    async fn observe_command(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
    ) -> Result<bool> {
        match self.effects.poll_command(attempt.id) {
            // Started by an earlier incarnation (or its run was lost).
            None => self.settle_stale_command(execution, attempt).await,
            Some(CommandPoll::Running { pgid }) => {
                if let Some(pgid) = pgid
                    && attempt.process_group_id != Some(pgid)
                {
                    let store = self.store.lock().await;
                    rows::record_process_group(&store, attempt.id, pgid)?;
                }
                if execution.status == ExecutionStatus::Cancelling {
                    self.effects.cancel_command(attempt.id);
                }
                Ok(false)
            }
            Some(CommandPoll::Exited(outcome)) => {
                self.settle_command_exit(execution, attempt, &outcome)
                    .await?;
                Ok(true)
            }
        }
    }

    /// The postcondition, shared by a finished op and a stale run: `None`
    /// when it holds, otherwise the settlement of the violation.
    fn violation(
        execution: &ExecutionRow,
        attempt: &AttemptRow,
    ) -> Option<(AttemptStatus, Settlement)> {
        let (Some(sandbox), Some(pre_head)) = (&attempt.sandbox_root, &attempt.pre_head) else {
            return None;
        };
        match catalog::sandbox_unchanged(sandbox, pre_head) {
            Ok(true) => None,
            Ok(false) => {
                let (status, mut settlement) = Self::preserve(execution, attempt, sandbox);
                if status == AttemptStatus::Blocked {
                    settlement.failure_class = Some(failure::SANDBOX_MUTATED);
                    settlement.error = Some(
                        "catalog op changed its sandbox; preserved as work to resolve, never re-run"
                            .into(),
                    );
                }
                Some((status, settlement))
            }
            Err(error) => Some((
                AttemptStatus::Failed,
                failed(failure::CUSTODY_REFUSED, &error.to_string()),
            )),
        }
    }

    async fn settle_command_exit(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
        outcome: &CommandOutcome,
    ) -> Result<()> {
        let (status, settlement) = match Self::violation(execution, attempt) {
            Some(outcome) => outcome,
            None if execution.status == ExecutionStatus::Cancelling => (
                AttemptStatus::Cancelled,
                Settlement {
                    failure_class: Some(failure::CANCELLED),
                    ..Settlement::default()
                },
            ),
            None => {
                let op = command_op(execution, &attempt.node_id)?;
                let output = serde_json::to_value(node_data(&outcome.output(&op)))?;
                if outcome.timed_out {
                    (
                        AttemptStatus::Failed,
                        Settlement {
                            output: Some(output),
                            ..failed(failure::TIMEOUT, "catalog op exceeded its wall time")
                        },
                    )
                } else if outcome.exit_code != 0 {
                    (
                        AttemptStatus::Failed,
                        Settlement {
                            output: Some(output),
                            ..failed(
                                failure::EXIT_NONZERO,
                                &format!("{} exited with {}", op.name(), outcome.exit_code),
                            )
                        },
                    )
                } else {
                    Self::command_success(execution, attempt, output)
                }
            }
        };
        self.settle(execution, attempt, status, settlement).await?;
        self.effects.forget_command(attempt.id);
        if let Some(sandbox) = &attempt.sandbox_root {
            catalog::reclaim_cache(sandbox);
        }
        Ok(())
    }

    /// A clean op passes its fork commit through, pinned for downstream.
    fn command_success(
        execution: &ExecutionRow,
        attempt: &AttemptRow,
        output: serde_json::Value,
    ) -> (AttemptStatus, Settlement) {
        let head = attempt
            .pre_head
            .clone()
            .unwrap_or_else(|| attempt.base_commit.clone());
        let pin = node_pin_ref(execution.id, &attempt.node_id, attempt.iteration);
        let pinned = attempt
            .sandbox_root
            .as_ref()
            .map_or(Ok(()), |sandbox| custody::pin_commit(sandbox, &pin, &head));
        if let Err(error) = pinned {
            return (
                AttemptStatus::Failed,
                failed(failure::CUSTODY_REFUSED, &error.to_string()),
            );
        }
        (
            AttemptStatus::Succeeded,
            Settlement {
                result_commit: Some(head),
                pin_ref: Some(pin),
                output: Some(output),
                ..Settlement::default()
            },
        )
    }

    /// Plan §2.4 row for a catalog op of an earlier incarnation: kill the
    /// stale group, then an unchanged sandbox is `interrupted` (re-run as a
    /// new attempt) and a changed one is preserved work, never re-run.
    async fn settle_stale_command(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
    ) -> Result<bool> {
        if let Some(pgid) = attempt.process_group_id {
            self.effects.kill_stale_group(pgid);
        }
        let (status, settlement) = match Self::violation(execution, attempt) {
            Some(outcome) => outcome,
            None if execution.status == ExecutionStatus::Cancelling => (
                AttemptStatus::Cancelled,
                Settlement {
                    failure_class: Some(failure::CANCELLED),
                    ..Settlement::default()
                },
            ),
            None => (
                AttemptStatus::Interrupted,
                failed(
                    failure::INTERRUPTED,
                    "catalog op interrupted with an unchanged sandbox; re-run as a new attempt",
                ),
            ),
        };
        self.settle(execution, attempt, status, settlement).await?;
        if let Some(sandbox) = &attempt.sandbox_root {
            catalog::reclaim_cache(sandbox);
        }
        Ok(true)
    }
}
