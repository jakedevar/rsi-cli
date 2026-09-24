//! Chain driver — closes the `master_improve` convergence loop OUTSIDE the DAG.
//!
//! Subscribes to `DaemonEvent::GraphExecution` on the event bus. When a chain's
//! workflow execution finishes, parses the budget node's structured output,
//! either respawns a fresh iteration with the refined goal or marks the chain
//! halted with a terminal `HaltReason`.
//!
//! Path A architecture (see `thoughts/shared/research/2026-04-28-master-improve-convergence-loop.md` §1):
//! the driver reads ONLY the budget node's preview/artifact. The judge's
//! verdict flows transitively through `merge_upstream_data` into budget's
//! upstream context.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use regex::Regex;
use rsi_common::types::{
    ChainIteration, GraphExecutionUpdate, HaltReason, WorkflowExecutionLookup,
    WorkflowExecutionSnapshot, WorkflowExecutionStatus,
};
use rsi_graph::format::WorkflowDefinition;
use rsi_graph::generate::templates::build_starter_workflow;
use tokio::sync::Mutex;
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::bus::{DaemonEvent, EventBus};
use crate::session::SessionManager;
use crate::store::Store;

/// Compile-time default cap if RPC didn't override.
pub const MASTER_IMPROVE_DEFAULT_CAP: u32 = 10;

/// Spawn the chain driver background task. Returns the `JoinHandle`.
/// Wired in `crates/rsid/src/main.rs` after `stall_detector` spawn.
///
/// `store` is wrapped in a `tokio::sync::Mutex` because the underlying
/// `Store` (rusqlite Connection + statement cache) is `!Sync`. This matches
/// the `SessionManager`'s `Arc<Mutex<Store>>` ownership model so the same
/// handle can be shared with the driver task and held across await points.
pub fn spawn_chain_driver(
    session_manager: Arc<SessionManager>,
    store: Arc<Mutex<Store>>,
    event_bus: Arc<EventBus>,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Daemon-startup recovery: mark abandoned chains as Error("daemon-restart").
        {
            let store_guard = store.lock().await;
            if let Err(e) = recover_active_chains(&store_guard) {
                error!("chain_driver: recovery failed at startup: {e:#}");
            }
        }

        let mut rx = event_bus.subscribe();
        info!("chain_driver: subscribed to GraphExecution events");

        loop {
            tokio::select! {
                () = cancel.cancelled() => {
                    info!("chain_driver: shutdown requested, exiting loop");
                    event_bus.unsubscribe();
                    return;
                }
                ev = rx.recv() => {
                    match ev {
                        Ok(arc_ev) => {
                            if let DaemonEvent::GraphExecution { update } = arc_ev.as_ref() {
                                if !update.finished {
                                    continue;
                                }
                                if let Err(e) = process_execution_finished(
                                    update,
                                    &session_manager,
                                    &store,
                                ).await {
                                    error!(
                                        "chain_driver: process failed for execution {}: {e:#}",
                                        update.execution_id
                                    );
                                }
                            }
                        }
                        Err(RecvError::Lagged(n)) => {
                            warn!(
                                "chain_driver: broadcast lagged by {n} events; \
                                 re-querying active chains"
                            );
                            let store_guard = store.lock().await;
                            if let Err(e) = recover_active_chains(&store_guard) {
                                error!("chain_driver: lag-recovery failed: {e:#}");
                            }
                        }
                        Err(RecvError::Closed) => {
                            info!("chain_driver: event bus closed, exiting");
                            event_bus.unsubscribe();
                            return;
                        }
                    }
                }
            }
        }
    })
}

/// On each finished `GraphExecution`, decide if it belongs to a chain and act.
async fn process_execution_finished(
    update: &GraphExecutionUpdate,
    session_manager: &Arc<SessionManager>,
    store: &Arc<Mutex<Store>>,
) -> Result<()> {
    // Phase 1 — short, sync DB lookup under lock.
    let chain_lookup = {
        let s = store.lock().await;
        s.get_chain_for_execution(update.execution_id)
            .map_err(|e| anyhow!("get_chain_for_execution: {e}"))?
    };
    let Some((chain_id, iteration_index)) = chain_lookup else {
        // Not a chain execution — ignore.
        return Ok(());
    };

    debug!(
        "chain_driver: chain {} iter {} reached terminal status {:?}",
        chain_id, iteration_index, update.status
    );

    match update.status {
        WorkflowExecutionStatus::Failed => {
            let s = store.lock().await;
            s.update_chain_iteration_outcome(
                chain_id,
                iteration_index,
                &HaltReason::Error(format!(
                    "workflow-failed: {}",
                    update.error.as_deref().unwrap_or("(no error message)")
                )),
                None,
                None,
            )
            .map_err(|e| anyhow!("update outcome (failed): {e}"))?;
            drop(s);
            return Ok(());
        }
        WorkflowExecutionStatus::Interrupted => {
            let s = store.lock().await;
            s.update_chain_iteration_outcome(
                chain_id,
                iteration_index,
                &HaltReason::Error("workflow-interrupted".to_string()),
                None,
                None,
            )
            .map_err(|e| anyhow!("update outcome (interrupted): {e}"))?;
            drop(s);
            return Ok(());
        }
        WorkflowExecutionStatus::Succeeded => {
            // Continue below.
        }
        _ => {
            // Unexpected non-terminal status (Accepted/Running/future variants)
            // leaked through `finished == true`.
            warn!(
                "chain_driver: unexpected status {:?} on finished update",
                update.status
            );
            return Ok(());
        }
    }

    // Fetch the snapshot to read the budget node's preview.
    let lookup = session_manager
        .get_workflow_execution(update.execution_id)
        .await
        .map_err(|e| anyhow!("get_workflow_execution: {e}"))?;

    let snapshot = match lookup {
        WorkflowExecutionLookup::Found { execution } => execution,
        WorkflowExecutionLookup::Expired { execution_id, .. } => {
            return Err(anyhow!(
                "snapshot expired for execution {execution_id} before driver could read it"
            ));
        }
        WorkflowExecutionLookup::NotFound { execution_id } => {
            return Err(anyhow!("snapshot missing for execution {execution_id}"));
        }
        _ => {
            return Err(anyhow!(
                "unexpected WorkflowExecutionLookup variant for execution {}",
                update.execution_id
            ));
        }
    };

    let verdict = parse_budget_verdict(&snapshot, chain_id, iteration_index)?;
    apply_verdict(verdict, chain_id, iteration_index, session_manager, store).await
}

/// Driver-internal verdict, parsed from the budget node's output.
#[derive(Debug)]
enum BudgetVerdict {
    ProceedDone,
    ProceedContinue { refined_goal: String },
    Halt(HaltReason),
}

fn parse_budget_verdict(
    snapshot: &WorkflowExecutionSnapshot,
    chain_id: Uuid,
    iteration_index: u32,
) -> Result<BudgetVerdict> {
    // Find the budget node update (most recent one if multiple)
    let budget_update = snapshot
        .updates
        .iter()
        .rev()
        .find(|u| u.node_id.as_deref() == Some("budget"))
        .ok_or_else(|| anyhow!("no budget node update in snapshot"))?;

    let preview = budget_update.output_preview.as_deref().unwrap_or("");
    let trimmed = preview.trim();

    // Try short-form parsing first (preview may already contain full payload if short).
    if let Some(v) = parse_verdict_from_str(trimmed)? {
        return Ok(v);
    }

    // Fall back to the on-disk artifact (preview was truncated past 120 chars).
    let artifact_path = artifact_path(chain_id, iteration_index);
    let full = std::fs::read_to_string(&artifact_path)
        .with_context(|| format!("read budget artifact at {}", artifact_path.display()))?;
    let trimmed_full = full.trim();

    parse_verdict_from_str(trimmed_full)?
        .ok_or_else(|| anyhow!("budget output unparseable in artifact: {trimmed_full:?}"))
}

// Regex patterns and capture-group accesses are statically guaranteed valid
// by the regex literals below; the unit tests in this module exercise every
// branch. Allowing `unwrap_used` keeps the parser readable.
#[allow(clippy::unwrap_used)]
fn parse_verdict_from_str(s: &str) -> Result<Option<BudgetVerdict>> {
    // Order matters: try the more-specific HALT regexes before the generic CONTINUE-style.
    // (Note: budget grammar uses PROCEED:/HALT: prefixes only — no CONTINUE; CONTINUE is
    // judge-side and is transduced by budget into PROCEED:/HALT:.)

    if s == "PROCEED: judge=DONE" {
        return Ok(Some(BudgetVerdict::ProceedDone));
    }

    let re_continue = Regex::new(r"^PROCEED:\s+judge=CONTINUE\s+refined_goal=(.+)$").unwrap();
    if let Some(caps) = re_continue.captures(s) {
        let goal = caps.get(1).unwrap().as_str().to_string();
        // If the preview placeholder reads "<see artifact file path>", the caller falls
        // back to the artifact file. We treat that exact placeholder as "not parseable here."
        if goal == "<see artifact file path>" {
            return Ok(None);
        }
        return Ok(Some(BudgetVerdict::ProceedContinue { refined_goal: goal }));
    }

    let re_regression = Regex::new(r"^HALT:\s+regression\s+pre=(\d+)\s+post=(\d+)$").unwrap();
    if let Some(caps) = re_regression.captures(s) {
        let pre: u32 = caps.get(1).unwrap().as_str().parse()?;
        let post: u32 = caps.get(2).unwrap().as_str().parse()?;
        return Ok(Some(BudgetVerdict::Halt(HaltReason::Regression {
            pre,
            post,
        })));
    }

    if s == "HALT: stop-file" {
        return Ok(Some(BudgetVerdict::Halt(HaltReason::StopFile)));
    }
    if s == "HALT: judge-malformed" {
        return Ok(Some(BudgetVerdict::Halt(HaltReason::JudgeMalformed)));
    }

    let re_judge_blocked = Regex::new(r"^HALT:\s+judge-blocked\s+(.+)$").unwrap();
    if let Some(caps) = re_judge_blocked.captures(s) {
        return Ok(Some(BudgetVerdict::Halt(HaltReason::JudgeBlocked(
            caps.get(1).unwrap().as_str().to_string(),
        ))));
    }

    let re_error = Regex::new(r"^HALT:\s+error\s+(.+)$").unwrap();
    if let Some(caps) = re_error.captures(s) {
        return Ok(Some(BudgetVerdict::Halt(HaltReason::Error(
            caps.get(1).unwrap().as_str().to_string(),
        ))));
    }

    Ok(None)
}

async fn apply_verdict(
    verdict: BudgetVerdict,
    chain_id: Uuid,
    iteration_index: u32,
    session_manager: &Arc<SessionManager>,
    store: &Arc<Mutex<Store>>,
) -> Result<()> {
    match verdict {
        BudgetVerdict::ProceedDone => {
            let s = store.lock().await;
            s.update_chain_iteration_outcome(
                chain_id,
                iteration_index,
                &HaltReason::Done,
                None,
                None,
            )
            .map_err(|e| anyhow!("update outcome (done): {e}"))?;
            drop(s);
            info!(
                "chain_driver: chain {} converged at iter {}",
                chain_id, iteration_index
            );
        }
        BudgetVerdict::ProceedContinue { refined_goal } => {
            // Pre-respawn stop-file check (atomic — overrides judge=CONTINUE).
            if std::fs::metadata(stop_file_path()).is_ok() {
                let s = store.lock().await;
                s.update_chain_iteration_outcome(
                    chain_id,
                    iteration_index,
                    &HaltReason::StopFile,
                    None,
                    None,
                )
                .map_err(|e| anyhow!("update outcome (stop-file): {e}"))?;
                drop(s);
                info!(
                    "chain_driver: chain {} stopped at iter {} (stop-file)",
                    chain_id, iteration_index
                );
                return Ok(());
            }

            // Cap check + mark current iter as Done (one critical section).
            let s = store.lock().await;
            let iters = s
                .list_chain_iterations(chain_id)
                .map_err(|e| anyhow!("list_chain_iterations: {e}"))?;
            let cap = iters.first().map_or(MASTER_IMPROVE_DEFAULT_CAP, |i| i.cap);
            if iteration_index + 1 >= cap {
                s.update_chain_iteration_outcome(
                    chain_id,
                    iteration_index,
                    &HaltReason::Cap,
                    None,
                    None,
                )
                .map_err(|e| anyhow!("update outcome (cap): {e}"))?;
                drop(s);
                info!(
                    "chain_driver: chain {} hit cap {} at iter {}",
                    chain_id, cap, iteration_index
                );
                return Ok(());
            }

            // Mark this iteration's outcome as "Done" (it succeeded in advancing
            // the chain). Whether the chain ultimately converges is recorded on
            // the LAST row. post_failure_count is deferred to L3.
            s.update_chain_iteration_outcome(
                chain_id,
                iteration_index,
                &HaltReason::Done,
                None,
                None,
            )
            .map_err(|e| anyhow!("update outcome (proceed-done-marker): {e}"))?;
            drop(s);

            let new_execution_id = respawn_iteration(
                chain_id,
                iteration_index + 1,
                refined_goal,
                cap,
                session_manager,
                store,
            )
            .await?;
            info!(
                "chain_driver: chain {} respawned iter {} as execution {}",
                chain_id,
                iteration_index + 1,
                new_execution_id
            );
        }
        BudgetVerdict::Halt(reason) => {
            let s = store.lock().await;
            s.update_chain_iteration_outcome(chain_id, iteration_index, &reason, None, None)
                .map_err(|e| anyhow!("update outcome (halt): {e}"))?;
            drop(s);
            info!(
                "chain_driver: chain {} halted at iter {}: {:?}",
                chain_id, iteration_index, reason
            );
        }
    }
    Ok(())
}

/// Allocate a `chain_id`, insert iteration #0, mutate template entry instructions,
/// call `execute_workflow_live`. Returns `(chain_id, first_execution_id, started_at)`.
///
/// # Errors
///
/// Returns an error if the `master_improve` template is not registered, the
/// workflow execution fails to launch, or persisting the iteration row fails.
pub async fn register_chain(
    session_manager: Arc<SessionManager>,
    store: Arc<Mutex<Store>>,
    initial_goal: String,
    cap: u32,
    workflow_id: Uuid,
    project_id: Option<Uuid>,
    working_dir: Option<String>,
) -> Result<(Uuid, Uuid, chrono::DateTime<Utc>)> {
    let chain_id = Uuid::new_v4();

    let workflow_def = build_starter_workflow("master_improve")
        .ok_or_else(|| anyhow!("master_improve template not registered"))?;
    let workflow_def = stamp_iteration_context(workflow_def, chain_id, 0, cap, 0, &initial_goal)?;

    let started_at = Utc::now();
    let working_dir_pb = working_dir.map(PathBuf::from);

    // Call execute_workflow_live (associated function, takes Arc<SessionManager>).
    let response = SessionManager::execute_workflow_live(
        Arc::clone(&session_manager),
        workflow_id,
        workflow_def,
        None,  // input
        false, // dry_run
        project_id,
        working_dir_pb,
        None, // parent_id — chain driver spawns top-level workflow (P1.12)
    )
    .await
    .map_err(|e| anyhow!("execute_workflow_live: {e}"))?;

    let iter = ChainIteration {
        chain_id,
        iteration_index: 0,
        parent_execution_id: None,
        child_execution_id: response.execution_id,
        halt_reason: None,
        goal_text: initial_goal,
        refined_goal_text: None,
        token_count: None,
        pre_failure_count: Some(0), // iter 0 baseline
        post_failure_count: None,
        cap,
        started_at,
        ended_at: None,
    };
    {
        let s = store.lock().await;
        s.insert_chain_iteration(&iter)
            .map_err(|e| anyhow!("insert_chain_iteration: {e}"))?;
    }

    Ok((chain_id, response.execution_id, started_at))
}

/// Respawn iteration N (N > 0) of an existing chain.
async fn respawn_iteration(
    chain_id: Uuid,
    iteration_index: u32,
    refined_goal: String,
    cap: u32,
    session_manager: &Arc<SessionManager>,
    store: &Arc<Mutex<Store>>,
) -> Result<Uuid> {
    let workflow_def = build_starter_workflow("master_improve")
        .ok_or_else(|| anyhow!("master_improve template not registered"))?;

    // Read prior iteration metadata under lock.
    let s = store.lock().await;
    let prior_iters = s
        .list_chain_iterations(chain_id)
        .map_err(|e| anyhow!("list_chain_iterations: {e}"))?;
    drop(s);
    let prior = prior_iters
        .into_iter()
        .find(|i| i.iteration_index == iteration_index - 1);
    let pre_failure_count = prior
        .as_ref()
        .and_then(|i| i.post_failure_count)
        .unwrap_or(0);
    let parent_execution_id = prior.as_ref().map(|i| i.child_execution_id);

    let workflow_def = stamp_iteration_context(
        workflow_def,
        chain_id,
        iteration_index,
        cap,
        pre_failure_count,
        &refined_goal,
    )?;

    let workflow_id = Uuid::new_v4(); // each iteration is a fresh workflow execution
    let started_at = Utc::now();

    let response = SessionManager::execute_workflow_live(
        Arc::clone(session_manager),
        workflow_id,
        workflow_def,
        None,  // input
        false, // dry_run
        None,  // project_id (inherited via context, not re-supplied)
        None,  // working_dir
        None,  // parent_id — chain respawn keeps the same orphan-top-level mode (P1.12)
    )
    .await
    .map_err(|e| anyhow!("execute_workflow_live: {e}"))?;

    let iter = ChainIteration {
        chain_id,
        iteration_index,
        parent_execution_id,
        child_execution_id: response.execution_id,
        halt_reason: None,
        goal_text: refined_goal.clone(),
        refined_goal_text: Some(refined_goal),
        token_count: None,
        pre_failure_count: Some(pre_failure_count),
        post_failure_count: None,
        cap,
        started_at,
        ended_at: None,
    };
    {
        let s = store.lock().await;
        s.insert_chain_iteration(&iter)
            .map_err(|e| anyhow!("insert_chain_iteration: {e}"))?;
    }

    Ok(response.execution_id)
}

/// Mutate the entry node's instructions to inject ITERATION CONTEXT.
fn stamp_iteration_context(
    mut workflow_def: WorkflowDefinition,
    chain_id: Uuid,
    iteration_index: u32,
    cap: u32,
    pre_failure_count: u32,
    goal: &str,
) -> Result<WorkflowDefinition> {
    let entry = workflow_def
        .nodes
        .iter_mut()
        .find(|n| n.id == "entry")
        .ok_or_else(|| anyhow!("master_improve template has no entry node"))?;

    entry.instructions = format!(
        "ITERATION CONTEXT (driver-injected -- do not edit):\n  \
         chain_id: {chain_id}\n  \
         iteration_index: {iteration_index}\n  \
         cap: {cap}\n  \
         pre_failure_count: {pre_failure_count}\n  \
         refined_goal_from_previous_judge: {goal}\n\n\
         USER'S GOAL FOR THIS ITERATION:\n\
         {goal}\n\n\
         Reply with exactly `PIPELINE ENTRY READY`, then exit. Do not quote, \
         restate, or summarize the goal. Do not start work -- the downstream \
         nodes handle research, planning, and implementation."
    );

    Ok(workflow_def)
}

/// On daemon boot, mark any chain that was running at last shutdown as
/// `HaltReason::Error("daemon-restart")`.
///
/// # Errors
///
/// Returns an error if listing active chains or updating any iteration's
/// outcome fails (typically due to a `sqlite` I/O error or schema corruption).
pub fn recover_active_chains(store: &Store) -> Result<()> {
    let active = store
        .list_active_chains()
        .map_err(|e| anyhow!("list_active_chains: {e}"))?;
    for (chain_id, max_iter_idx) in active {
        warn!(
            "chain_driver: recovering abandoned chain {} (last iter {})",
            chain_id, max_iter_idx
        );
        store
            .update_chain_iteration_outcome(
                chain_id,
                max_iter_idx,
                &HaltReason::Error("daemon-restart".to_string()),
                None,
                None,
            )
            .map_err(|e| anyhow!("update outcome (daemon-restart): {e}"))?;
    }
    Ok(())
}

fn stop_file_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".rsi")
        .join("STOP")
}

fn artifact_path(chain_id: Uuid, iteration_index: u32) -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".rsi")
        .join("chain")
        .join(chain_id.to_string())
        .join(format!("iter-{iteration_index}-budget.txt"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // tests assert known-good inputs; unwrap is the canonical assertion form
mod tests {
    use super::*;

    #[test]
    fn iteration_entry_carries_goal_without_requesting_a_restatement() {
        let workflow = build_starter_workflow("master_improve").unwrap();
        let stamped =
            stamp_iteration_context(workflow, Uuid::nil(), 2, 5, 1, "Fix the timeout").unwrap();
        let entry = stamped
            .nodes
            .iter()
            .find(|node| node.id == "entry")
            .unwrap();

        assert!(entry.instructions.contains("Fix the timeout"));
        assert!(entry.instructions.contains("PIPELINE ENTRY READY"));
        assert!(
            !entry
                .instructions
                .to_lowercase()
                .contains("echo the user's goal")
        );
    }

    #[test]
    fn parse_proceed_done() {
        let v = parse_verdict_from_str("PROCEED: judge=DONE")
            .unwrap()
            .unwrap();
        assert!(matches!(v, BudgetVerdict::ProceedDone));
    }

    #[test]
    fn parse_proceed_continue_short() {
        let v = parse_verdict_from_str("PROCEED: judge=CONTINUE refined_goal=fix the regression")
            .unwrap()
            .unwrap();
        match v {
            BudgetVerdict::ProceedContinue { refined_goal } => {
                assert_eq!(refined_goal, "fix the regression");
            }
            _ => panic!("expected ProceedContinue"),
        }
    }

    #[test]
    fn parse_proceed_continue_artifact_placeholder_returns_none() {
        // When budget output uses the artifact placeholder, parser falls back to disk.
        let v =
            parse_verdict_from_str("PROCEED: judge=CONTINUE refined_goal=<see artifact file path>")
                .unwrap();
        assert!(v.is_none());
    }

    #[test]
    fn parse_halt_regression() {
        let v = parse_verdict_from_str("HALT: regression pre=3 post=7")
            .unwrap()
            .unwrap();
        match v {
            BudgetVerdict::Halt(HaltReason::Regression { pre, post }) => {
                assert_eq!(pre, 3);
                assert_eq!(post, 7);
            }
            _ => panic!("expected Halt(Regression)"),
        }
    }

    #[test]
    fn parse_halt_stop_file() {
        let v = parse_verdict_from_str("HALT: stop-file").unwrap().unwrap();
        assert!(matches!(v, BudgetVerdict::Halt(HaltReason::StopFile)));
    }

    #[test]
    fn parse_halt_judge_blocked() {
        let v = parse_verdict_from_str("HALT: judge-blocked plan doc not found")
            .unwrap()
            .unwrap();
        match v {
            BudgetVerdict::Halt(HaltReason::JudgeBlocked(msg)) => {
                assert_eq!(msg, "plan doc not found");
            }
            _ => panic!("expected Halt(JudgeBlocked)"),
        }
    }

    #[test]
    fn parse_halt_judge_malformed() {
        let v = parse_verdict_from_str("HALT: judge-malformed")
            .unwrap()
            .unwrap();
        assert!(matches!(v, BudgetVerdict::Halt(HaltReason::JudgeMalformed)));
    }

    #[test]
    fn parse_halt_error() {
        let v = parse_verdict_from_str("HALT: error cargo test panic")
            .unwrap()
            .unwrap();
        match v {
            BudgetVerdict::Halt(HaltReason::Error(msg)) => {
                assert_eq!(msg, "cargo test panic");
            }
            _ => panic!("expected Halt(Error)"),
        }
    }

    #[test]
    fn parse_unknown_returns_none() {
        let v = parse_verdict_from_str("garbage").unwrap();
        assert!(v.is_none());
    }
}
