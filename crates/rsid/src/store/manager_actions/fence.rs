//! K2 lead-generation continuation fence (design d0f2817fa, aligned with
//! F3 RPC-1 C1/C2).
//!
//! An automated or agent continuation captures the published lineage tip and
//! its Epic's lead generation at dispatch, then re-checks both, the tip's
//! publication state and its retirement witness as the first action after it
//! acquires the tip's spawn guard. Every lead writer and the rotation
//! publication (C1) take the same guards, so a lead or tip change commits
//! strictly before the check (refused, retryable) or strictly after the
//! provider is installed (the delivery was valid).

use super::refused;
use crate::error::{DaemonError, Result};
use crate::store::Store;
use rusqlite::OptionalExtension;
use uuid::Uuid;

/// Retryable: the tip has a reserved successor that is not yet published
/// (between RPC-1 C3 launch and C1 publish).
pub const CONTINUATION_PUBLICATION_PENDING: &str = "continuation_publication_pending";
/// Retryable: the published tip moved since capture.
pub const CONTINUATION_TIP_CHANGED: &str = "continuation_tip_changed";
/// Retryable: the tip's Epic lead generation moved since capture.
pub const CONTINUATION_LEAD_GENERATION_CHANGED: &str = "continuation_lead_generation_changed";
/// Retryable: the tip owns a live provider (checked under its guard).
pub const CONTINUATION_TARGET_BUSY: &str = "continuation_target_busy";
/// Retryable: the tip row is Starting/Running but has no live owner (#620).
pub const CONTINUATION_TIP_UNESTABLISHED: &str = "continuation_tip_unestablished";
/// Terminal: a live manager retirement witness covers the tip.
pub const CONTINUATION_TARGET_RETIRED: &str = "continuation_target_retired";
/// Terminal: the retry budget of a retryable refusal is spent.
pub const CONTINUATION_RETRY_EXHAUSTED: &str = "continuation_retry_exhausted";
/// Terminal: the agent that authorized an `AgentContinueChild` no longer
/// holds a scope over the tip (review round 2 `agent_continue_lead_authority_race`).
pub const CONTINUATION_ACTOR_AUTHORITY_CHANGED: &str = "continuation_actor_authority_changed";

const RETRYABLE: [&str; 5] = [
    CONTINUATION_PUBLICATION_PENDING,
    CONTINUATION_TIP_CHANGED,
    CONTINUATION_LEAD_GENERATION_CHANGED,
    CONTINUATION_TARGET_BUSY,
    CONTINUATION_TIP_UNESTABLISHED,
];

/// Rotation lineage chase bound, shared with watch planning.
const LINEAGE_CAP: usize = crate::session::WATCH_LINEAGE_DEPTH_CAP;

/// Who authorizes the continuation.
/// - `Automated`: every check.
/// - `AgentChild` (`AgentContinueChild`): the verb's documented contract is to
///   interrupt a running child, so it skips only the busy refusal. `caller`
///   must still hold its agent scope over the tip under the tip's guard, and
///   the fence carries the lead generation bound before authorization.
/// - `ManagerRecovery` (manager `ResumeLead` with a live claim): waives only the
///   retirement witness; its own manager gate keeps the typed busy refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContinuationAuthorityV1 {
    Automated,
    AgentChild { caller: Uuid },
    ManagerRecovery { operation_id: Uuid },
}

/// Dispatch-time capture, re-checked under the tip's spawn guard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContinuationFenceV1 {
    pub origin: Uuid,
    pub tip: Uuid,
    pub epic: Option<Uuid>,
    pub lead_generation: Option<i64>,
    pub authority: ContinuationAuthorityV1,
}

/// The typed fence code carried by `error`, if any. A refusal may append
/// `:<detail>` (the busy refusal names the tip); the code is the head.
pub fn continuation_fence_code(error: &DaemonError) -> Option<&'static str> {
    let DaemonError::InvalidParam(message) = error else {
        return None;
    };
    let head = message.split(':').next().unwrap_or_default();
    RETRYABLE
        .iter()
        .chain(
            [
                CONTINUATION_TARGET_RETIRED,
                CONTINUATION_RETRY_EXHAUSTED,
                CONTINUATION_ACTOR_AUTHORITY_CHANGED,
            ]
            .iter(),
        )
        .find(|known| head == **known)
        .copied()
}

/// True for the refusals that must retain the wake and retry with backoff.
pub fn continuation_fence_retryable(error: &DaemonError) -> bool {
    continuation_fence_code(error).is_some_and(|code| RETRYABLE.contains(&code))
}

impl Store {
    /// Chase the published rotation lineage (RPC-1 C2) from `origin`.
    /// `None` when `origin` has no row.
    pub(crate) fn published_lineage_tip(&self, origin: Uuid) -> Result<Option<Uuid>> {
        if self.get_session(origin)?.is_none() {
            return Ok(None);
        }
        let mut tip = origin;
        for _ in 0..LINEAGE_CAP {
            let Some(next) = self.find_published_rotation_successor(tip)? else {
                return Ok(Some(tip));
            };
            tip = next;
        }
        tracing::warn!(
            %origin,
            %tip,
            "published lineage chase reached its depth cap; using the reached hop"
        );
        Ok(Some(tip))
    }

    /// The Epic whose lead generation fences continuations of `tip`: its
    /// parent Epic, else an Epic it leads.
    fn continuation_epic(&self, tip: Uuid) -> Result<Option<(Uuid, i64)>> {
        self.conn
            .query_row(
                "SELECT e.id, g.generation FROM sessions t
                 JOIN sessions e ON e.session_kind='Epic'
                   AND (e.id=t.parent_id OR e.lead_session_id=t.id)
                 JOIN epic_lead_generations g ON g.epic_id=e.id
                 WHERE t.id=?1
                 ORDER BY (e.id=t.parent_id) DESC LIMIT 1",
                [tip.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?
            .map(|(id, generation)| {
                Uuid::parse_str(&id)
                    .map(|id| (id, generation))
                    .map_err(|error| DaemonError::Store(error.to_string()))
            })
            .transpose()
    }

    /// Capture in one store-lock hold: published tip and lead generation.
    pub(crate) fn capture_continuation_fence(
        &self,
        origin: Uuid,
        authority: ContinuationAuthorityV1,
    ) -> Result<Option<ContinuationFenceV1>> {
        let Some(tip) = self.published_lineage_tip(origin)? else {
            return Ok(None);
        };
        let epic = self.continuation_epic(tip)?;
        Ok(Some(ContinuationFenceV1 {
            origin,
            tip,
            epic: epic.map(|(id, _)| id),
            lead_generation: epic.map(|(_, generation)| generation),
            authority,
        }))
    }

    /// Store half of the check, run under the tip's spawn guard. The session
    /// layer adds `busy` (active map) between `generation_changed` and
    /// `unestablished`; see `SessionManager::check_continuation_fence`.
    pub(crate) fn check_continuation_fence_durable(
        &self,
        fence: &ContinuationFenceV1,
    ) -> Result<()> {
        if self.continuation_publication_pending(fence.tip)? {
            return Err(refused(CONTINUATION_PUBLICATION_PENDING));
        }
        if self.published_lineage_tip(fence.origin)? != Some(fence.tip) {
            return Err(refused(CONTINUATION_TIP_CHANGED));
        }
        if let ContinuationAuthorityV1::AgentChild { caller } = fence.authority
            && !self.agent_continue_actor_authorized(caller, fence.tip)?
        {
            return Err(refused(CONTINUATION_ACTOR_AUTHORITY_CHANGED));
        }
        let epic = self.continuation_epic(fence.tip)?;
        if epic.map(|(id, _)| id) != fence.epic
            || epic.map(|(_, generation)| generation) != fence.lead_generation
        {
            return Err(refused(CONTINUATION_LEAD_GENERATION_CHANGED));
        }
        Ok(())
    }

    /// Effect claim (review round 3 `agent_continue_lead_authority_race_remains`):
    /// ONE IMMEDIATE transaction that re-runs the durable fence (publication,
    /// tip, `AgentChild` actor scope, Epic lead generation) and commits the
    /// continuation's first durable write, a touch of the tip's row version
    /// (`sessions.updated_at`), before any effect on the tip (retry
    /// suppression, completed-map takeover, interrupt, admission or spawn).
    /// Every lead writer commits its generation bump through this same
    /// connection, so `SQLite` orders the two: a lead change committed first
    /// refuses the claim; one committed after it is legal, because the
    /// continuation was already committed while the actor held authority.
    pub(crate) fn claim_continuation_effect(&self, fence: &ContinuationFenceV1) -> Result<()> {
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        self.check_continuation_fence_durable(fence)?;
        let claimed = tx.execute(
            "UPDATE sessions SET updated_at=?2 WHERE id=?1",
            rusqlite::params![
                fence.tip.to_string(),
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            ],
        )?;
        if claimed != 1 {
            return Err(refused(CONTINUATION_TIP_CHANGED));
        }
        tx.commit()?;
        Ok(())
    }

    /// Final durable half: orphan tip and retirement witness.
    pub(crate) fn check_continuation_fence_owner(
        &self,
        fence: &ContinuationFenceV1,
        tip_active: bool,
    ) -> Result<()> {
        let status: Option<String> = self
            .conn
            .query_row(
                "SELECT status FROM sessions WHERE id=?1",
                [fence.tip.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if !tip_active && matches!(status.as_deref(), Some("Starting" | "Running")) {
            return Err(refused(CONTINUATION_TIP_UNESTABLISHED));
        }
        if !matches!(
            fence.authority,
            ContinuationAuthorityV1::ManagerRecovery { .. }
        ) && self.manager_lead_program_outcome_superseded(fence.tip)?
        {
            return Err(refused(CONTINUATION_TARGET_RETIRED));
        }
        Ok(())
    }

    /// The `AgentContinueChild` mutation scope, recomputed in this store
    /// hold: `caller` is the tip's direct parent, the lead of its parent, or
    /// the appointed manager with a live `SessionControl` grant over it.
    fn agent_continue_actor_authorized(&self, caller: Uuid, tip: Uuid) -> Result<bool> {
        let direct_parent: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1 AND parent_id=?2)",
            [tip.to_string(), caller.to_string()],
            |row| row.get(0),
        )?;
        if direct_parent || self.parent_lead_authorizes_child(caller, tip)? {
            return Ok(true);
        }
        Ok(matches!(
            self.manager_session_control_scope(caller, tip, true),
            Ok(Some(_))
        ))
    }

    /// `tip` has a non-Failed `continued_from = tip` row that C2 does not
    /// return: a successor launched but not yet published (or published by
    /// its own reservation commit, not yet committed). Delivering to `tip`
    /// now would restart a predecessor while its successor runs.
    fn continuation_publication_pending(&self, tip: Uuid) -> Result<bool> {
        let published = self.find_published_rotation_successor(tip)?;
        let pending: Vec<String> = self
            .conn
            .prepare(
                "SELECT id FROM sessions WHERE continued_from=?1
                   AND status NOT IN ('Failed','Archived','Deleted')",
            )?
            .query_map([tip.to_string()], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(pending
            .iter()
            .any(|id| published.map(|p| p.to_string()).as_deref() != Some(id.as_str())))
    }
}
