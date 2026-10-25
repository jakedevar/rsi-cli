//! #235: `max_active_sessions` caps the worker sessions a manager launches.
//! The sessions that hold, held or are taking over that manager's own seat
//! (the appointed root, the current tip, a context-cap rotation's outgoing
//! predecessor still `Running`, a root-succession candidate) are not workers:
//! they never fill a slot of their own principal's cap and a call of theirs
//! is never refused by it. They are still listed, apart, so a manager can see
//! why its count is what it is.
//!
//! Only the principal's OWN seat lineage is excluded: a seat a portfolio node
//! appoints still fills that node's caps (#1330), because to the node it is a
//! session the node launched.

use std::collections::BTreeSet;

use super::*;

/// How many ids a refusal names per list; the snapshot names them all.
const REFUSAL_ID_LIMIT: usize = 16;

/// The seat lineage of one manager principal: its appointed root, its current
/// tip and every root-succession origin (and that origin's predecessor) it
/// recorded, closed over the rotation lineage in both directions (a committed
/// rotation edge, or a same-project, same-parent, same-kind `continued_from`
/// link, which already exists while the outgoing seat is still `Running`).
const SEAT_LINEAGE_SQL: &str = "WITH RECURSIVE seat(id,depth) AS (
        SELECT value,0 FROM json_each(?1)
        UNION SELECT session_id,0 FROM manager_root_resource_origins
          WHERE project_id=?2 AND manager_session_id=?3
        UNION SELECT predecessor_session_id,0 FROM manager_root_resource_origins
          WHERE project_id=?2 AND manager_session_id=?3 AND predecessor_session_id IS NOT NULL
        UNION SELECT e.successor_session_id,s.depth+1 FROM seat s
          JOIN harness_manager_rotation_edges e ON e.predecessor_session_id=s.id
          WHERE s.depth<?4
        UNION SELECT c.id,s.depth+1 FROM seat s
          JOIN sessions p ON p.id=s.id JOIN sessions c ON c.continued_from=p.id
          WHERE s.depth<?4 AND c.project_id IS p.project_id AND c.parent_id IS p.parent_id
            AND c.session_kind=p.session_kind
        UNION SELECT p.id,s.depth+1 FROM seat s
          JOIN sessions c ON c.id=s.id JOIN sessions p ON p.id=c.continued_from
          WHERE s.depth<?4 AND p.project_id IS c.project_id AND p.parent_id IS c.parent_id
            AND p.session_kind=c.session_kind
     ) SELECT DISTINCT id FROM seat LIMIT 1025";

/// The session whose admitted rotation invocation parents a rotation
/// reservation (a successor that may have no session row yet).
const RESERVATION_PARENT_SQL: &str = "SELECT parent.session_id FROM model_invocations child
     JOIN model_invocations parent ON parent.id=child.parent_invocation_id
     WHERE child.session_id=?1 AND child.purpose='session.rotate.child'
       AND child.admission_status='admitted' AND parent.session_id IS NOT NULL
     ORDER BY child.created_at DESC LIMIT 1";

/// The live members of one principal's cohort, split by #235: the ones its
/// `max_active_sessions` counts, and its own manager-seat sessions.
#[derive(Default)]
pub(super) struct SeatSplit {
    /// Counted against the cap.
    pub counted: BTreeMap<Uuid, SessionProvider>,
    /// Live sessions of the principal's own seat lineage: listed, not counted.
    pub seat_active: BTreeMap<Uuid, SessionProvider>,
    /// The whole seat lineage, live or not: a call of one of these is never
    /// refused by the concurrency caps.
    pub seat: BTreeSet<Uuid>,
}

impl SeatSplit {
    /// Whether `session` belongs to the principal's own seat lineage.
    pub(super) fn holds_seat(&self, session: Option<Uuid>) -> bool {
        session.is_some_and(|id| self.seat.contains(&id))
    }
}

impl Store {
    /// The seat lineage of `config`'s own principal ([`SEAT_LINEAGE_SQL`]),
    /// plus each rotation reservation in `reservations` whose rotation was
    /// admitted from a session of that lineage.
    pub(super) fn manager_v2_seat_lineage(
        &self,
        config: &HarnessManagerConfigV1,
        reservations: impl IntoIterator<Item = Uuid>,
    ) -> Result<BTreeSet<Uuid>> {
        let roots: Vec<Uuid> = std::iter::once(config.manager_session_id)
            .chain(config.current_session_id)
            .collect();
        let mut stmt = self.conn.prepare_cached(SEAT_LINEAGE_SQL)?;
        let rows = stmt
            .query_map(
                params![
                    serde_json::to_string(&roots)?,
                    config.project_id.to_string(),
                    config.manager_session_id.to_string(),
                    ROTATION_LIMIT
                ],
                |r| r.get::<_, String>(0),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut seat = BTreeSet::new();
        for id in rows {
            seat.insert(
                Uuid::parse_str(&id).map_err(|_| refused("manager_v2_invalid_stored_identity"))?,
            );
        }
        for id in reservations {
            if seat.contains(&id) {
                continue;
            }
            let parent: Option<String> = self
                .conn
                .prepare_cached(RESERVATION_PARENT_SQL)?
                .query_row([id.to_string()], |r| r.get(0))
                .optional()?;
            if parent
                .and_then(|parent| Uuid::parse_str(&parent).ok())
                .is_some_and(|parent| seat.contains(&parent))
            {
                seat.insert(id);
            }
        }
        Ok(seat)
    }

    /// Split `active` (one cohort's live members) into what `config`'s cap
    /// counts and `config`'s own manager-seat sessions. `reservations` are
    /// the rotation reservations among them.
    pub(super) fn manager_v2_split_seat(
        &self,
        config: &HarnessManagerConfigV1,
        active: BTreeMap<Uuid, SessionProvider>,
        reservations: &BTreeSet<Uuid>,
    ) -> Result<SeatSplit> {
        let seat = self.manager_v2_seat_lineage(
            config,
            active
                .keys()
                .filter(|id| reservations.contains(id))
                .copied()
                .collect::<Vec<_>>(),
        )?;
        let (seat_active, counted) = active.into_iter().partition(|(id, _)| seat.contains(id));
        Ok(SeatSplit {
            counted,
            seat_active,
            seat,
        })
    }
}

/// The `manager_v2_concurrency_capacity` refusal, naming what filled the cap:
/// `code: active_sessions=<n> max_active_sessions=<m> counted=[...]
/// excluded_manager_seat=[...]` (the code stays the prefix, #235).
pub(super) fn concurrency_refusal(
    active: u64,
    max: u16,
    counted: impl IntoIterator<Item = Uuid>,
    seat: impl IntoIterator<Item = Uuid>,
) -> crate::error::DaemonError {
    crate::error::DaemonError::InvalidParam(format!(
        "manager_v2_concurrency_capacity: active_sessions={active} max_active_sessions={max} \
         counted=[{}] excluded_manager_seat=[{}]",
        id_list(counted),
        id_list(seat)
    ))
}

fn id_list(ids: impl IntoIterator<Item = Uuid>) -> String {
    let ids: Vec<Uuid> = ids.into_iter().collect();
    let mut out = ids
        .iter()
        .take(REFUSAL_ID_LIMIT)
        .map(Uuid::to_string)
        .collect::<Vec<_>>()
        .join(",");
    if ids.len() > REFUSAL_ID_LIMIT {
        out.push_str(&format!(",+{} more", ids.len() - REFUSAL_ID_LIMIT));
    }
    out
}
