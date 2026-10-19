//! #2750: the store half of `ReseatHarnessManager`.
//!
//! The daemon launches the successor (a session on another provider whose
//! `continued_from` is the failed seat); this module decides whether a seat
//! may be replaced and commits the re-appointment. The re-appointment is the
//! existing one: the scope save names the successor, so the displaced seat's
//! wakes, queued tier mail and open requests follow it (#1553, #2468, #2469),
//! and the project's saved policy is re-granted to the new tuple in the same
//! transaction instead of being left as a revoked draft. A policy that was
//! already revoked stays a draft: a reseat never confirms authority nobody
//! confirmed.

use super::Store;
use super::harness_manager_v2::refused;
use crate::error::Result;
use rsi_common::harness_manager::{
    ConfigureHarnessManagerRequestV1, HarnessManagerConfigV1, HarnessManagerScopeModeV1,
};
use rsi_common::harness_manager_v2::{
    ConfigureHarnessManagerPolicyRequestV2, HarnessManagerPolicyConfigV2,
};
use rsi_common::manager_reseat::*;
use rsi_common::types::{Session, SessionKind, SessionStatus};
use rusqlite::{Transaction, TransactionBehavior};
use uuid::Uuid;

/// The seat a reseat replaces and what the successor inherits.
#[derive(Debug, Clone)]
pub struct ManagerReseatPlan {
    pub config: HarnessManagerConfigV1,
    /// The saved policy when it is a live grant; `None` when the project has
    /// none or it is a revoked draft.
    pub policy: Option<HarnessManagerPolicyConfigV2>,
    pub seat: Session,
    /// The seat's tags, which the successor carries.
    pub tags: Vec<String>,
}

/// The committed re-appointment.
#[derive(Debug, Clone)]
pub struct ManagerReseatCommit {
    pub config: HarnessManagerConfigV1,
    pub policy: Option<HarnessManagerPolicyConfigV2>,
}

impl Store {
    /// Decide whether `project`'s appointed seat may be replaced now.
    ///
    /// A seat can be replaced when it is `Failed`, or when it is stopped
    /// (`Completed`, `Interrupted`) and the caller asserts its provider is
    /// unavailable. A running, starting or approval-waiting seat is live and a
    /// stopped seat is never guessed to be down. The seat must be a parentless
    /// Standard session, as a root project manager is.
    ///
    /// # Errors
    /// A stable `manager_reseat_*` / `manager_seat_*` refusal.
    pub fn manager_reseat_plan(
        &self,
        project: Uuid,
        provider_unavailable: bool,
    ) -> Result<ManagerReseatPlan> {
        let config = self
            .get_harness_manager(project)?
            .ok_or_else(|| refused(RESEAT_NOT_CONFIGURED))?;
        if config.is_revoked() {
            return Err(refused(RESEAT_REVOKED));
        }
        let seat_id = config
            .current_session_id
            .ok_or_else(|| refused(RESEAT_NO_SEAT))?;
        let seat = self
            .get_session(seat_id)?
            .ok_or_else(|| refused(RESEAT_NO_SEAT))?;
        match seat.status {
            SessionStatus::Failed => {}
            SessionStatus::Completed | SessionStatus::Interrupted => {
                if !provider_unavailable {
                    return Err(refused(RESEAT_SEAT_NOT_FAILED));
                }
            }
            _ => return Err(refused(RESEAT_SEAT_LIVE)),
        }
        if seat.parent_id.is_some() || seat.session_kind != SessionKind::Standard {
            return Err(refused(RESEAT_ROOT_REQUIRED));
        }
        let policy = self
            .get_harness_manager_policy(project)?
            .filter(|saved| !saved.revoked);
        let mut statement = self
            .conn
            .prepare("SELECT tag FROM session_tags WHERE session_id=?1 ORDER BY tag")?;
        let tags = statement
            .query_map([seat.id.to_string()], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ManagerReseatPlan {
            config,
            policy,
            seat,
            tags,
        })
    }

    /// Appoint `successor` to the seat `plan` described: the same scope, the
    /// same live policy grant and an audit record of the reason, in one
    /// transaction. The caller launched `successor` with `continued_from` set
    /// to the replaced seat.
    ///
    /// # Errors
    /// The re-appointment's own refusal (`manager_stale_scope_refresh_required`
    /// when the scope changed since the plan, `manager_project_mismatch`, ...);
    /// nothing is written then.
    pub fn commit_manager_reseat(
        &self,
        plan: &ManagerReseatPlan,
        successor: Uuid,
        reason: &str,
    ) -> Result<ManagerReseatCommit> {
        let previous = &plan.config;
        let scope = ConfigureHarnessManagerRequestV1 {
            project_id: previous.project_id,
            session_id: successor,
            epic_ids: match previous.scope_mode {
                HarnessManagerScopeModeV1::Project => None,
                HarnessManagerScopeModeV1::Selected => previous
                    .selected_epic_ids
                    .clone()
                    .or_else(|| Some(previous.epic_ids.clone())),
            },
            group_ids: previous.group_ids.clone(),
            expected_row_version: previous.row_version,
        };
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let (config, _outcome) = self.configure_harness_manager_in_tx(&tx, &scope)?;
        let policy = match &plan.policy {
            Some(saved) => Some(self.configure_harness_manager_policy_in_tx(
                &tx,
                &ConfigureHarnessManagerPolicyRequestV2 {
                    project_id: config.project_id,
                    expected_scope_version: config.row_version,
                    expected_policy_version: saved.row_version,
                    idempotency_key: format!("reseat:{successor}"),
                    policy: saved.policy.clone(),
                },
            )?),
            None => None,
        };
        self.manager_v2_event(
            &config,
            None,
            "seat_reseat",
            &format!("reseat:{successor}"),
            policy.as_ref().map_or(0, |granted| granted.row_version),
            &serde_json::json!({
                "predecessor_session_id": plan.seat.id,
                "predecessor_provider": plan.seat.provider,
                "successor_session_id": successor,
                "reason": reason,
            }),
        )?;
        tx.commit()?;
        Ok(ManagerReseatCommit { config, policy })
    }
}
