//! #1928: the one resolver for satellite dispatch scope.
//!
//! A declared scope entry is either a fixed session id (its rotation lineage
//! tip is the seat, #1112) or a project (its current appointed manager is the
//! seat, resolved live). The host that owns the sessions resolves it: the
//! satellite resolves its own seat; the hub only verifies the declared
//! address, because it cannot see the satellite's appointments. Message
//! delivery, manager-policy push, hub deploy and satellite reports all use the
//! functions here, so the three paths cannot disagree about who is in scope.

use super::Store;
use crate::error::Result;
use rsi_common::SessionStatus;
use rsi_common::satellite::SatelliteUuidV1;
use rsi_common::satellite_dispatch::{
    SatelliteInboundPolicyV1, SatelliteScopeEntryStatusV1, SatelliteScopeKindV1,
    SatelliteScopeStateV1, SatelliteScopeStatusV1,
};
use rusqlite::{Connection, params};
use std::collections::HashSet;
use uuid::Uuid;

/// Bound on the parent walk that decides whether a target is in scope.
pub const SATELLITE_SCOPE_MAX_DEPTH: usize = 32;

/// SQL predicate: peer `?1` declares at least one scope entry of either kind.
pub(crate) const HUB_SCOPE_DECLARED_SQL: &str =
    "(EXISTS(SELECT 1 FROM satellite_peer_scope sc WHERE sc.peer_id=?1)
        OR EXISTS(SELECT 1 FROM satellite_peer_project_scope sp WHERE sp.peer_id=?1))";

/// Hub side: does `peer` declare `address` as a session id or a project id?
/// This is the only thing the hub can verify; the satellite resolves the seat.
pub(crate) fn peer_declares_address(
    conn: &Connection,
    peer: &str,
    address: &str,
) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM satellite_peer_scope WHERE peer_id=?1 AND remote_session_id=?2)
             OR EXISTS(SELECT 1 FROM satellite_peer_project_scope WHERE peer_id=?1 AND project_id=?2)",
        params![peer, address],
        |row| row.get(0),
    )
}

/// Hub side: does `peer` declare `project` as a project scope entry?
pub(crate) fn peer_declares_project(
    conn: &Connection,
    peer: &str,
    project: &str,
) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM satellite_peer_project_scope WHERE peer_id=?1 AND project_id=?2)",
        params![peer, project],
        |row| row.get(0),
    )
}

/// Hub side, message admission and every dispatch attempt: what authorizes the
/// target. With a `scope_project`, the project authorizes it and the target is
/// a session the hub cannot see (the project seat's tree, checked live by the
/// satellite); without one, the target itself must be a declared address.
/// Project authorization is carried apart from the session target, so a
/// project-only scope reaches descendants of the seat (#1928).
pub(crate) fn peer_authorizes_target(
    conn: &Connection,
    peer: &str,
    target: &str,
    scope_project: Option<&str>,
) -> rusqlite::Result<bool> {
    match scope_project {
        Some(project) => peer_declares_project(conn, peer, project),
        None => peer_declares_address(conn, peer, target),
    }
}

/// What one declared entry resolves to on the satellite right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeResolution {
    /// A live seat session; hub traffic for the entry goes to it.
    Live { seat: Uuid },
    /// Nothing: the state names why, for the operator view.
    Unresolved(SatelliteScopeStateV1),
}

impl ScopeResolution {
    #[must_use]
    pub const fn seat(self) -> Option<Uuid> {
        match self {
            Self::Live { seat } => Some(seat),
            Self::Unresolved(_) => None,
        }
    }

    #[must_use]
    pub const fn state(self) -> SatelliteScopeStateV1 {
        match self {
            Self::Live { .. } => SatelliteScopeStateV1::Live,
            Self::Unresolved(state) => state,
        }
    }
}

/// How a resolved seat stands against its project's published manager
/// appointment right now (#268).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeatStanding {
    /// The seat holds its project's appointment (`current_session_id`).
    Appointed,
    /// The seat's project has no stored appointment (or the seat has no
    /// project).
    Unappointed,
    /// The seat held its project's appointment and no longer does: another
    /// session holds it now, or the appointment's anchor has no live tip
    /// (archived, deleted, absent) or is revoked. A deposed or replaced seat
    /// is never the satellite's manager seat and never receives manager mail.
    Displaced,
    /// The project has a stored appointment but this seat never held it (an
    /// ordinary session declared as a root next to its managed project). It
    /// keeps direct delivery and is not the satellite's manager seat.
    Bystander,
}

/// #272: where manager-addressed traffic for a session goes now, compared
/// with its project's published appointment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DisplacedSeat {
    /// The session holds its project's appointment, the project has none, it
    /// never held the project's appointment (an ordinary declared root), or
    /// its standing cannot be read: deliver to the session itself.
    NotDisplaced,
    /// Another session holds the project's appointment and is its live seat.
    Reseated { project: Uuid, seat: Uuid },
    /// The session held the appointment, but it resolves to no live seat
    /// (revoked policy, archived or deleted replacement): nobody may receive
    /// it as the seat.
    NoLiveSeat { project: Uuid },
}

/// Phase two's answer (#272): the session that receives a hub delivery and,
/// when the hub addressed a replaced seat, that seat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InboundDelivery {
    pub seat: Uuid,
    /// The displaced seat the hub's address resolved to; `seat` is then the
    /// project's current appointed seat.
    pub displaced: Option<Uuid>,
}

/// One declared entry with its resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedScopeEntry {
    pub kind: SatelliteScopeKindV1,
    pub declared: Uuid,
    pub resolution: ScopeResolution,
    /// Standing of the resolved seat; `Unappointed` when nothing resolves.
    pub standing: SeatStanding,
}

/// The satellite's inbound policy with every declared scope entry resolved
/// against the live sessions, read under one store lock.
#[derive(Debug, Clone, Default)]
pub struct InboundScope {
    pub policy: SatelliteInboundPolicyV1,
    pub entries: Vec<ResolvedScopeEntry>,
}

/// Why a hub delivery target is inside the scope (phase one, static).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboundHit {
    /// The target IS a declared project address: deliver to that project's
    /// current seat, whoever it is now.
    ProjectAddress(Uuid),
    /// The target is, or descends from, this scope root.
    Root(Uuid),
    /// The hub named this declared project as the authority for the target:
    /// the target must be the project's live seat or inside its tree
    /// (decided in phase two, against the live appointment).
    ProjectScoped(Uuid),
}

impl InboundScope {
    #[must_use]
    pub fn allows_hub(&self, hub_installation: Uuid) -> bool {
        self.policy
            .allowed_hub_installations
            .iter()
            .any(|id| id.0 == hub_installation)
    }

    /// Every declared address, session ids first: the stable replay identity
    /// of a satellite deploy (it survives rotation and re-appointment).
    #[must_use]
    pub fn declared_addresses(&self) -> Vec<Uuid> {
        self.entries.iter().map(|entry| entry.declared).collect()
    }

    /// The live seat for exactly this declared address, if it resolves.
    #[must_use]
    pub fn seat_for_address(&self, address: Uuid) -> Option<Uuid> {
        self.entries
            .iter()
            .find(|entry| entry.declared == address)
            .and_then(|entry| entry.resolution.seat())
    }

    /// The satellite's manager seat, as `(declared, seat)`: it owns a hub
    /// deploy's settlement wake and a hub QA run. #268: the first entry whose
    /// seat holds its project's published appointment wins, whatever the
    /// declaration order; otherwise the first entry whose seat's project has
    /// no appointment. A seat another session replaced (a deposed manager
    /// still declared as a session root) is never chosen, so its manager
    /// notices cannot resume it; a bystander of a managed project never owns
    /// the manager seat either; with nothing left the caller refuses and the
    /// hub sees the refusal.
    #[must_use]
    pub fn first_live(&self) -> Option<(Uuid, Uuid)> {
        self.live_with(SeatStanding::Appointed)
            .or_else(|| self.live_with(SeatStanding::Unappointed))
    }

    fn live_with(&self, standing: SeatStanding) -> Option<(Uuid, Uuid)> {
        self.entries
            .iter()
            .filter(|entry| entry.standing == standing)
            .find_map(|entry| Some((entry.declared, entry.resolution.seat()?)))
    }

    /// Is `session` the live seat of a declared entry?
    #[must_use]
    pub fn is_seat(&self, session: Uuid) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.resolution.seat() == Some(session))
    }

    /// Roots of the descendant walk: every declared session root (as before,
    /// whether or not it is live) plus the live seat of every declared project.
    fn walk_roots(&self) -> HashSet<Uuid> {
        self.entries
            .iter()
            .filter_map(|entry| match entry.kind {
                SatelliteScopeKindV1::Session => Some(entry.declared),
                SatelliteScopeKindV1::Project => entry.resolution.seat(),
            })
            .collect()
    }

    fn declares_project(&self, address: Uuid) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.kind == SatelliteScopeKindV1::Project && entry.declared == address)
    }
}

impl Store {
    /// Hub side: the declared project scope of one peer, sorted.
    pub fn satellite_peer_project_scope(&self, peer_id: Uuid) -> Result<Vec<SatelliteUuidV1>> {
        let mut statement = self.conn.prepare(
            "SELECT project_id FROM satellite_peer_project_scope WHERE peer_id=?1
             ORDER BY project_id",
        )?;
        let rows = statement.query_map([peer_id.to_string()], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            let id = Uuid::parse_str(&row?).map_err(|_| {
                crate::error::DaemonError::Store("invalid satellite scope project id".into())
            })?;
            out.push(SatelliteUuidV1(id));
        }
        Ok(out)
    }

    /// Hub side: does `peer` declare any scope entry at all? (deploy, reports)
    pub fn hub_peer_declares_scope(&self, peer_id: Uuid) -> Result<bool> {
        Ok(self.conn.query_row(
            &format!("SELECT {HUB_SCOPE_DECLARED_SQL}"),
            [peer_id.to_string()],
            |row| row.get(0),
        )?)
    }

    /// Hub side: does `peer` declare `address` (a session id or a project id)?
    /// Authorizes message queueing, delivery and manager-policy push alike.
    pub fn hub_peer_declares_address(&self, peer_id: Uuid, address: Uuid) -> Result<bool> {
        Ok(peer_declares_address(
            &self.conn,
            &peer_id.to_string(),
            &address.to_string(),
        )?)
    }

    /// Satellite side: resolve one declared entry against the live sessions.
    /// Fails closed: any unreadable or ambiguous state resolves to nothing.
    pub fn resolve_satellite_scope_entry(
        &self,
        kind: SatelliteScopeKindV1,
        id: Uuid,
    ) -> Result<ScopeResolution> {
        let live = |seat: Uuid| -> Result<ScopeResolution> {
            let status = self.get_session(seat)?.map(|session| session.status);
            Ok(match status {
                Some(SessionStatus::Archived | SessionStatus::Deleted) | None => {
                    ScopeResolution::Unresolved(SatelliteScopeStateV1::NoLiveSeat)
                }
                Some(_) => ScopeResolution::Live { seat },
            })
        };
        match kind {
            SatelliteScopeKindV1::Session => match self.satellite_delivery_tip(id, id)? {
                Some(tip) => live(tip),
                None => Ok(ScopeResolution::Unresolved(
                    SatelliteScopeStateV1::NoLiveSeat,
                )),
            },
            SatelliteScopeKindV1::Project => {
                // A stored-state error is not an appointment: fail closed.
                let Ok(config) = self.get_harness_manager(id) else {
                    return Ok(ScopeResolution::Unresolved(
                        SatelliteScopeStateV1::NoLiveSeat,
                    ));
                };
                let Some(config) = config else {
                    return Ok(ScopeResolution::Unresolved(
                        SatelliteScopeStateV1::Unappointed,
                    ));
                };
                let policy_revoked = self
                    .get_harness_manager_policy(id)
                    .map_or(true, |policy| policy.is_some_and(|policy| policy.revoked));
                if config.is_revoked() || policy_revoked {
                    return Ok(ScopeResolution::Unresolved(SatelliteScopeStateV1::Revoked));
                }
                match config.current_session_id {
                    Some(seat) => live(seat),
                    None => Ok(ScopeResolution::Unresolved(
                        SatelliteScopeStateV1::NoLiveSeat,
                    )),
                }
            }
        }
    }

    /// Satellite side: the inbound policy with every declared entry resolved.
    pub fn inbound_scope(&self) -> Result<InboundScope> {
        let policy = self.satellite_inbound_policy()?;
        let mut entries = Vec::new();
        for (kind, ids) in [
            (SatelliteScopeKindV1::Session, &policy.scope_roots),
            (SatelliteScopeKindV1::Project, &policy.scope_projects),
        ] {
            for id in ids {
                let resolution = self.resolve_satellite_scope_entry(kind, id.0)?;
                entries.push(ResolvedScopeEntry {
                    kind,
                    declared: id.0,
                    resolution,
                    standing: resolution
                        .seat()
                        .map_or(SeatStanding::Unappointed, |seat| self.seat_standing(seat)),
                });
            }
        }
        Ok(InboundScope { policy, entries })
    }

    /// #268: compare `seat` with its project's published appointment. A
    /// stored-state error is not an appointment either way: fail closed.
    /// #2880/#2881: a stored appointment whose anchor has no live tip
    /// (`current_session_id` is `None`) is still an appointment, and a seat
    /// is `Displaced` only with evidence it held that appointment.
    fn seat_standing(&self, seat: Uuid) -> SeatStanding {
        let Ok(Some(session)) = self.get_session(seat) else {
            return SeatStanding::Displaced;
        };
        let Some(project) = session.project_id else {
            return SeatStanding::Unappointed;
        };
        let Ok(config) = self.get_harness_manager_notice_config(project) else {
            return SeatStanding::Displaced;
        };
        let Some(config) = config else {
            return SeatStanding::Unappointed;
        };
        if config.current_session_id == Some(seat) {
            return SeatStanding::Appointed;
        }
        if self.held_project_appointment(project, config.manager_session_id, seat) {
            SeatStanding::Displaced
        } else {
            SeatStanding::Bystander
        }
    }

    /// Did `seat` hold `project`'s manager appointment? True when it is in the
    /// rotation lineage of the current anchor or of an anchor a superseded
    /// scope generation recorded (`harness_manager_notice_scope_retirements`
    /// is retained and forward-only). A storage error answers `true`: fail
    /// closed to the displaced route rather than deliver as the seat.
    fn held_project_appointment(&self, project: Uuid, current_anchor: Uuid, seat: Uuid) -> bool {
        let mut anchors = vec![current_anchor];
        let retired = self
            .conn
            .prepare(
                "SELECT DISTINCT manager_session_id
                 FROM harness_manager_notice_scope_retirements WHERE project_id=?1",
            )
            .and_then(|mut statement| {
                statement
                    .query_map([project.to_string()], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()
            });
        match retired {
            Ok(retired) => anchors.extend(retired.iter().filter_map(|id| Uuid::parse_str(id).ok())),
            Err(_) => return true,
        }
        anchors.iter().any(|anchor| {
            super::harness_manager::manager_lineage_hops_on(&self.conn, *anchor, project)
                .contains(&seat)
        })
    }

    /// #272: is `session` a seat another session replaced, and if so which
    /// live seat holds its project's appointment now?
    pub(crate) fn displaced_seat_route(&self, session: Uuid) -> Result<DisplacedSeat> {
        if self.seat_standing(session) != SeatStanding::Displaced {
            return Ok(DisplacedSeat::NotDisplaced);
        }
        let Some(project) = self.get_session(session)?.and_then(|row| row.project_id) else {
            return Ok(DisplacedSeat::NotDisplaced);
        };
        let resolution =
            self.resolve_satellite_scope_entry(SatelliteScopeKindV1::Project, project)?;
        Ok(match resolution.seat() {
            Some(seat) if seat == session => DisplacedSeat::NotDisplaced,
            Some(seat) => DisplacedSeat::Reseated { project, seat },
            None => DisplacedSeat::NoLiveSeat { project },
        })
    }

    /// Satellite side, phase one of hub delivery: is `target` in scope? Static
    /// with respect to the seat, so a recorded delivery can still be replayed
    /// after the seat changed or the project was revoked. `scope_project` is
    /// the project the hub named as the target's authority (#1928); it must be
    /// declared here, and phase two then checks the target against that
    /// project's live seat only.
    pub fn inbound_scope_hit(
        &self,
        scope: &InboundScope,
        target: Uuid,
        scope_project: Option<Uuid>,
    ) -> Result<Option<InboundHit>> {
        if let Some(project) = scope_project {
            // The hub named the project that authorizes this target; only that
            // project may. An undeclared project is refused like any other
            // out-of-scope target.
            return Ok(scope
                .declares_project(project)
                .then_some(InboundHit::ProjectScoped(project)));
        }
        if scope.declares_project(target) {
            return Ok(Some(InboundHit::ProjectAddress(target)));
        }
        let roots = scope.walk_roots();
        let mut cursor = Some(target);
        for _ in 0..SATELLITE_SCOPE_MAX_DEPTH {
            let Some(id) = cursor else { break };
            let Some(session) = self.get_session(id)? else {
                break;
            };
            if roots.contains(&id) {
                return Ok(Some(InboundHit::Root(id)));
            }
            cursor = session.parent_id;
        }
        Ok(None)
    }

    /// Satellite side, phase two: the session that receives the delivery. A
    /// project address goes to that project's current seat; anything under a
    /// root goes to the target's own rotation tip (#1112), except a replaced
    /// seat (#272, see [`Self::inbound_scope_delivery`]).
    pub fn inbound_scope_delivery_seat(
        &self,
        scope: &InboundScope,
        hit: InboundHit,
        target: Uuid,
    ) -> Result<Option<Uuid>> {
        Ok(self
            .inbound_scope_delivery(scope, hit, target)?
            .map(|delivery| delivery.seat))
    }

    /// [`Self::inbound_scope_delivery_seat`] with the redirection recorded:
    /// `displaced` names the replaced seat the hub's address reached.
    pub fn inbound_scope_delivery(
        &self,
        scope: &InboundScope,
        hit: InboundHit,
        target: Uuid,
    ) -> Result<Option<InboundDelivery>> {
        let direct = |seat: Option<Uuid>| {
            seat.map(|seat| InboundDelivery {
                seat,
                displaced: None,
            })
        };
        match hit {
            InboundHit::ProjectAddress(project) => Ok(direct(scope.seat_for_address(project))),
            InboundHit::Root(root) => {
                let Some(tip) = self.satellite_delivery_tip(target, root)? else {
                    return Ok(None);
                };
                self.root_delivery(scope, root, tip)
            }
            InboundHit::ProjectScoped(project) => {
                let Some(seat) = scope.seat_for_address(project) else {
                    return Ok(None);
                };
                if target == project {
                    return Ok(direct(Some(seat)));
                }
                Ok(direct(self.project_member_tip(project, seat, target)?))
            }
        }
    }

    /// #272: a hub message addressed to a declared root's own lineage (the old
    /// seat's id, or one of its rotations) whose tip another session replaced
    /// reaches the declared project's current seat instead; with no live seat
    /// it is refused, so the hub sees it and retargets the project address.
    /// A descendant of the root keeps its own tip, and a replaced seat whose
    /// project this host does not declare keeps today's delivery (redirecting
    /// it would reach a seat outside the declared scope).
    fn root_delivery(
        &self,
        scope: &InboundScope,
        root: Uuid,
        tip: Uuid,
    ) -> Result<Option<InboundDelivery>> {
        let direct = InboundDelivery {
            seat: tip,
            displaced: None,
        };
        if self.satellite_delivery_tip(root, root)? != Some(tip) {
            return Ok(Some(direct));
        }
        let project = match self.displaced_seat_route(tip)? {
            DisplacedSeat::NotDisplaced => return Ok(Some(direct)),
            DisplacedSeat::Reseated { project, .. } | DisplacedSeat::NoLiveSeat { project } => {
                project
            }
        };
        if !scope.declares_project(project) {
            return Ok(Some(direct));
        }
        Ok(scope.seat_for_address(project).map(|seat| InboundDelivery {
            seat,
            displaced: Some(tip),
        }))
    }

    /// The rotation lineage of a project seat: the seat and its `continued_from`
    /// predecessors inside the project, so a worker spawned by an earlier seat
    /// stays reachable after the manager rotated. `None` (fail closed) when the
    /// chain loops or exceeds `SATELLITE_LINEAGE_MAX_HOPS`.
    pub(crate) fn project_seat_lineage(
        &self,
        project: Uuid,
        seat: Uuid,
    ) -> Result<Option<HashSet<Uuid>>> {
        let mut lineage = HashSet::new();
        let mut cursor = Some(seat);
        for _ in 0..=super::satellite_dispatch::SATELLITE_LINEAGE_MAX_HOPS {
            let Some(id) = cursor else {
                return Ok(Some(lineage));
            };
            let Some(session) = self.get_session(id)? else {
                return Ok(Some(lineage));
            };
            if session.project_id != Some(project) {
                return Ok(Some(lineage));
            }
            if !lineage.insert(id) {
                return Ok(None);
            }
            // Walk back only over published rotation edges (#1989): a
            // `continued_from` that no committed reservation or active receipt
            // backs transfers no seat authority.
            cursor = match session.continued_from {
                Some(previous)
                    if super::harness_manager::manager_successors_on(
                        &self.conn, previous, true,
                    )? == [id.to_string()] =>
                {
                    Some(previous)
                }
                _ => None,
            };
        }
        Ok(None)
    }

    /// Phase two for a project-scoped target: the target is a session of the
    /// project that is, or descends (by `parent_id`) from, the live seat or one
    /// of the seat's rotation predecessors; the delivery goes to its own
    /// rotation tip (#1112). Ancestry is read live, so a re-appointment takes
    /// the old seat's tree out of scope with no scope edit on either host.
    fn project_member_tip(&self, project: Uuid, seat: Uuid, target: Uuid) -> Result<Option<Uuid>> {
        let Some(session) = self.get_session(target)? else {
            return Ok(None);
        };
        if session.project_id != Some(project) {
            return Ok(None);
        }
        let Some(lineage) = self.project_seat_lineage(project, seat)? else {
            return Ok(None);
        };
        let mut cursor = Some(session);
        for _ in 0..SATELLITE_SCOPE_MAX_DEPTH {
            let Some(current) = cursor else { break };
            if lineage.contains(&current.id) {
                return self.satellite_delivery_tip(target, seat);
            }
            cursor = match current.parent_id {
                Some(parent) => self.get_session(parent)?,
                None => None,
            };
        }
        Ok(None)
    }

    /// Operator read: this host's inbound scope with each entry resolved.
    pub fn satellite_scope_status(&self) -> Result<SatelliteScopeStatusV1> {
        Ok(SatelliteScopeStatusV1 {
            inbound: self
                .inbound_scope()?
                .entries
                .into_iter()
                .map(|entry| SatelliteScopeEntryStatusV1 {
                    kind: entry.kind,
                    id: SatelliteUuidV1(entry.declared),
                    state: entry.resolution.state(),
                    seat_session_id: entry.resolution.seat().map(SatelliteUuidV1),
                })
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::make_test_session;
    use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
    use rsi_common::harness_manager_v2::{ConfigureHarnessManagerPolicyRequestV2, ManagerPolicyV2};
    use rsi_common::manager_reseat::{
        RESEAT_NOT_CONFIGURED, RESEAT_SEAT_LIVE, RESEAT_SEAT_NOT_FAILED,
    };
    use rsi_common::satellite_manager_policy::{
        AgentPushSatelliteManagerPolicyRequestV1, SatelliteManagerPolicyPatchV1,
        SatelliteManagerPolicyRequestV1,
    };

    /// A satellite store with one project, an allowlisted hub, and the project
    /// declared as an inbound scope entry.
    struct Rig {
        store: Store,
        project: Uuid,
        hub: Uuid,
    }

    fn rig() -> Rig {
        let store = Store::open_in_memory().expect("in-memory store");
        // The store seeds the d04 test project that `make_test_session` uses.
        let project = crate::store::d04_test_project_id();
        let hub = Uuid::new_v4();
        store
            .put_satellite_inbound_policy(&SatelliteInboundPolicyV1 {
                allowed_hub_installations: vec![SatelliteUuidV1(hub)],
                scope_projects: vec![SatelliteUuidV1(project)],
                ..Default::default()
            })
            .unwrap();
        Rig {
            store,
            project,
            hub,
        }
    }

    /// Raw reservation fixture (foreign keys off: the transition ledger is
    /// not what these tests exercise).
    fn insert_reservation(store: &Store, predecessor: Uuid, candidate: Uuid, state: &str) {
        let digest = format!(
            "sha256:{}{}",
            Uuid::new_v4().simple(),
            Uuid::new_v4().simple()
        );
        let at = "2026-01-01T00:00:00.000000000Z";
        let (attempt, invocation, evidence, evidence_digest, reason, class) = match state {
            "reserved" => (None, None, None, None, None, None),
            "uncertain" => (
                Some(Uuid::new_v4().to_string()),
                Some(Uuid::new_v4().to_string()),
                None,
                None,
                Some("unknown"),
                Some("unknown"),
            ),
            "committed" => (
                Some(Uuid::new_v4().to_string()),
                Some(Uuid::new_v4().to_string()),
                Some("{}"),
                Some(digest.as_str()),
                None,
                None,
            ),
            _ => (None, None, None, None, Some("failed"), Some("failed")),
        };
        store.conn.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
        store
            .conn
            .execute(
                "INSERT INTO agent_successor_reservations (reservation_id,predecessor_session_id,
                    epic_id,candidate_session_id,caller_key_digest,request_json,request_fingerprint,
                    candidate_kind,inherited_launch_json,expected_lead_session_id,
                    expected_lead_generation,state,state_version,launch_attempt_id,
                    model_invocation_id,establishment_evidence_json,establishment_digest,
                    terminal_reason,safe_error_class,reserved_at,updated_at)
                 VALUES (?1,?2,?2,?3,?4,'{}',?4,'Task','{}',?2,1,?5,1,?6,?7,?8,?9,?10,?11,?12,?12)",
                rusqlite::params![
                    Uuid::new_v4().to_string(),
                    predecessor.to_string(),
                    candidate.to_string(),
                    digest,
                    state,
                    attempt,
                    invocation,
                    evidence,
                    evidence_digest,
                    reason,
                    class,
                    at,
                ],
            )
            .unwrap();
        store.conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
    }

    impl Rig {
        fn session(&self, status: SessionStatus) -> Uuid {
            let mut session = make_test_session();
            session.status = status;
            self.store.insert_session(&session).unwrap();
            session.id
        }

        /// The operator seats `session` as the project's manager (a new
        /// appointment when it is not the persisted anchor's lineage).
        fn appoint(&self, session: Uuid, epic_ids: Option<Vec<Uuid>>) {
            let version = self
                .store
                .get_harness_manager(self.project)
                .unwrap()
                .map_or(0, |config| config.row_version);
            self.store
                .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                    project_id: self.project,
                    session_id: session,
                    epic_ids,
                    group_ids: Vec::new(),
                    expected_row_version: version,
                })
                .unwrap();
        }

        /// The operator saves (re-grants) the manager policy.
        fn grant(&self, key: &str) {
            let scope = self
                .store
                .get_harness_manager(self.project)
                .unwrap()
                .unwrap()
                .row_version;
            let version = self
                .store
                .get_harness_manager_policy(self.project)
                .unwrap()
                .map_or(0, |policy| policy.row_version);
            self.store
                .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                    project_id: self.project,
                    expected_scope_version: scope,
                    expected_policy_version: version,
                    idempotency_key: key.into(),
                    policy: ManagerPolicyV2::default(),
                })
                .unwrap();
        }

        /// The satellite declares these session roots and projects.
        fn declare(&self, roots: Vec<Uuid>, projects: Vec<Uuid>) {
            self.store
                .put_satellite_inbound_policy(&SatelliteInboundPolicyV1 {
                    allowed_hub_installations: vec![SatelliteUuidV1(self.hub)],
                    scope_roots: roots.into_iter().map(SatelliteUuidV1).collect(),
                    scope_projects: projects.into_iter().map(SatelliteUuidV1).collect(),
                    ..Default::default()
                })
                .unwrap();
        }

        /// Both phases of hub delivery for a message addressed to `target`
        /// by its own id: the delivery, or nothing when it is refused.
        fn deliver(&self, target: Uuid) -> Option<InboundDelivery> {
            let scope = self.store.inbound_scope().unwrap();
            let hit = self
                .store
                .inbound_scope_hit(&scope, target, None)
                .unwrap()?;
            self.store
                .inbound_scope_delivery(&scope, hit, target)
                .unwrap()
        }

        fn direct(seat: Uuid) -> Option<InboundDelivery> {
            Some(InboundDelivery {
                seat,
                displaced: None,
            })
        }

        fn manager_seat(&self) -> Option<(Uuid, Uuid)> {
            self.store.inbound_scope().unwrap().first_live()
        }

        fn child_of(&self, parent: Uuid) -> Uuid {
            let mut session = make_test_session();
            session.parent_id = Some(parent);
            session.status = SessionStatus::Completed;
            self.store.insert_session(&session).unwrap();
            session.id
        }

        /// Self-succession: `seat` rotates to a recorded successor, which is
        /// now the project's manager.
        fn rotate_seat(&self, seat: Uuid) -> Uuid {
            self.store
                .update_session_status(seat, SessionStatus::Archived)
                .unwrap();
            let mut successor = make_test_session();
            successor.status = SessionStatus::Completed;
            successor.continued_from = Some(seat);
            successor.rotation_depth += 1;
            self.store.insert_session(&successor).unwrap();
            assert!(
                self.store
                    .record_harness_manager_rotation(seat, successor.id)
                    .unwrap()
            );
            successor.id
        }

        /// A rotation candidate that continues `predecessor` and carries a
        /// reservation in `state` (`failed`, `uncertain`, `reserved`), as a
        /// failed or unsettled successor attempt leaves it: `continued_from`
        /// stays set but no authority transferred (#1989).
        fn unpublished_candidate(&self, predecessor: Uuid, state: &str) -> Uuid {
            let mut candidate = make_test_session();
            candidate.status = SessionStatus::Failed;
            candidate.continued_from = Some(predecessor);
            candidate.rotation_depth += 1;
            self.store.insert_session(&candidate).unwrap();
            insert_reservation(&self.store, predecessor, candidate.id, state);
            candidate.id
        }

        /// Both phases of hub delivery for a message whose target the hub
        /// authorized with the declared project: the session that would
        /// receive it, or nothing.
        fn bound_delivery(&self, target: Uuid, project: Uuid) -> Option<Uuid> {
            let scope = self.store.inbound_scope().unwrap();
            let hit = self
                .store
                .inbound_scope_hit(&scope, target, Some(project))
                .unwrap()?;
            self.store
                .inbound_scope_delivery_seat(&scope, hit, target)
                .unwrap()
        }

        fn resolution(&self) -> ScopeResolution {
            self.store
                .resolve_satellite_scope_entry(SatelliteScopeKindV1::Project, self.project)
                .unwrap()
        }

        /// Every satellite-side authorization path asked about the same
        /// project address, as `(message, push, deploy, report)` verdicts
        /// naming the session each one would act on.
        fn verdicts(&self, reporter: Uuid) -> [Option<Uuid>; 4] {
            let scope = self.store.inbound_scope().unwrap();
            let message = self
                .store
                .inbound_scope_hit(&scope, self.project, None)
                .unwrap()
                .and_then(|hit| {
                    self.store
                        .inbound_scope_delivery_seat(&scope, hit, self.project)
                        .unwrap()
                });
            let push = scope.seat_for_address(self.project);
            let deploy = scope.first_live().map(|(_, seat)| seat);
            let report = scope.is_seat(reporter).then_some(reporter);
            [message, push, deploy, report]
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn a_project_scope_follows_rotation_and_reappointment() {
        let rig = rig();
        let first = rig.session(SessionStatus::Completed);
        rig.appoint(first, None);
        assert_eq!(rig.resolution(), ScopeResolution::Live { seat: first });

        // Self-succession: the manager rotates to a successor session.
        rig.store
            .update_session_status(first, SessionStatus::Archived)
            .unwrap();
        let mut successor = make_test_session();
        successor.status = SessionStatus::Completed;
        successor.continued_from = Some(first);
        successor.rotation_depth += 1;
        rig.store.insert_session(&successor).unwrap();
        assert!(
            rig.store
                .record_harness_manager_rotation(first, successor.id)
                .unwrap()
        );
        assert_eq!(
            rig.resolution(),
            ScopeResolution::Live { seat: successor.id }
        );

        // Operator re-appointment: a different session becomes the manager.
        let reseated = rig.session(SessionStatus::Completed);
        rig.appoint(reseated, None);
        assert_eq!(rig.resolution(), ScopeResolution::Live { seat: reseated });
        let status = rig.store.satellite_scope_status().unwrap();
        assert_eq!(status.inbound.len(), 1);
        assert_eq!(status.inbound[0].state, SatelliteScopeStateV1::Live);
        assert_eq!(
            status.inbound[0].seat_session_id,
            Some(SatelliteUuidV1(reseated))
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn unpublished_successor_candidates_never_divert_project_bound_mail() {
        for state in ["failed", "uncertain", "reserved"] {
            let rig = rig();
            let seat = rig.session(SessionStatus::Completed);
            rig.appoint(seat, None);
            let worker = rig.child_of(seat);
            let candidate = rig.unpublished_candidate(seat, state);
            // The current seat and its workers still receive their own mail.
            assert_eq!(rig.bound_delivery(seat, rig.project), Some(seat), "{state}");
            assert_eq!(
                rig.bound_delivery(worker, rig.project),
                Some(worker),
                "{state}"
            );
            // The retained candidate is no seat and no recipient.
            assert_eq!(rig.bound_delivery(candidate, rig.project), None, "{state}");
            assert_eq!(rig.resolution(), ScopeResolution::Live { seat }, "{state}");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn a_rotation_after_a_failed_attempt_forwards_the_predecessor_address_to_the_published_tip() {
        let rig = rig();
        let first = rig.session(SessionStatus::Completed);
        rig.appoint(first, None);
        let worker = rig.child_of(first);
        let failed = rig.unpublished_candidate(first, "failed");
        let published = rig.rotate_seat(first);
        assert_eq!(rig.resolution(), ScopeResolution::Live { seat: published });
        // A valid predecessor address, and a worker of the earlier seat,
        // resolve to the published tip, never to the failed candidate.
        assert_eq!(
            rig.bound_delivery(first, rig.project),
            Some(published),
            "predecessor address"
        );
        assert_eq!(rig.bound_delivery(worker, rig.project), Some(worker));
        assert_eq!(rig.bound_delivery(published, rig.project), Some(published));
        assert_eq!(rig.bound_delivery(failed, rig.project), None);
        // An unrelated session of the project is still refused.
        let stranger = rig.session(SessionStatus::Completed);
        assert_eq!(rig.bound_delivery(stranger, rig.project), None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn a_committed_reservation_publishes_the_edge_and_a_retired_receipt_does_not() {
        let rig = rig();
        let seat = rig.session(SessionStatus::Completed);
        rig.appoint(seat, None);
        let failed = rig.unpublished_candidate(seat, "failed");
        let mut committed = make_test_session();
        committed.status = SessionStatus::Completed;
        committed.continued_from = Some(seat);
        committed.rotation_depth += 1;
        rig.store.insert_session(&committed).unwrap();
        insert_reservation(&rig.store, seat, committed.id, "committed");
        assert_eq!(
            rig.store.satellite_delivery_tip(seat, seat).unwrap(),
            Some(committed.id)
        );
        // A published edge with no seat-side authority change is a worker
        // rotation: a second published successor is a fork and fails closed.
        let mut fork = make_test_session();
        fork.continued_from = Some(seat);
        rig.store.insert_session(&fork).unwrap();
        insert_reservation(&rig.store, seat, fork.id, "committed");
        assert_eq!(rig.store.satellite_delivery_tip(seat, seat).unwrap(), None);
        let _ = failed;

        // A retired receipt (restored predecessor) publishes nothing.
        let retired = rig.session(SessionStatus::Archived);
        let mut next = make_test_session();
        next.continued_from = Some(retired);
        rig.store.insert_session(&next).unwrap();
        rig.store
            .conn
            .execute(
                "INSERT INTO harness_manager_rotation_edges
                 (predecessor_session_id,successor_session_id,committed_at,retired_at)
                 VALUES (?1,?2,'2026-01-01T00:00:00.000000000Z','2026-01-02T00:00:00.000000000Z')",
                rusqlite::params![retired.to_string(), next.id.to_string()],
            )
            .unwrap();
        assert_eq!(
            rig.store.satellite_delivery_tip(retired, retired).unwrap(),
            Some(retired)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn ordinary_reserved_and_failed_candidates_do_not_divert_mail_but_legacy_rotation_resolves() {
        let rig = rig();
        let seat = rig.session(SessionStatus::Completed);
        let reserve = |status: SessionStatus| {
            let mut candidate = make_test_session();
            candidate.status = status;
            candidate.continued_from = Some(seat);
            candidate.rotation_depth += 1;
            rig.store.insert_session(&candidate).unwrap();
            // Ordinary rotations write only the rotation-event marker, never
            // an agent_successor_reservations row.
            rig.store
                .insert_rotation_event(
                    seat,
                    &Uuid::new_v4().to_string(),
                    "reserved",
                    "successor_reserved",
                    Some(&serde_json::json!({ "successor_id": candidate.id }).to_string()),
                )
                .unwrap();
            candidate.id
        };
        let reserved = reserve(SessionStatus::Starting);
        assert_eq!(
            rig.store.satellite_delivery_tip(seat, seat).unwrap(),
            Some(seat),
            "reserved ordinary candidate"
        );
        let failed = reserve(SessionStatus::Failed);
        assert_eq!(
            rig.store.satellite_delivery_tip(seat, seat).unwrap(),
            Some(seat),
            "failed ordinary candidate"
        );
        // Publishing the reserved candidate (a completed event) makes it the tip.
        rig.store
            .insert_rotation_event(
                seat,
                &Uuid::new_v4().to_string(),
                "completed",
                "completed",
                Some(&serde_json::json!({ "successor_id": reserved }).to_string()),
            )
            .unwrap();
        assert_eq!(
            rig.store.satellite_delivery_tip(seat, seat).unwrap(),
            Some(reserved)
        );
        let _ = failed;

        // A genuine pre-reservation rotation: no marker, no reservation, no edge.
        let old_seat = rig.session(SessionStatus::Completed);
        let mut legacy = make_test_session();
        legacy.status = SessionStatus::Completed;
        legacy.continued_from = Some(old_seat);
        rig.store.insert_session(&legacy).unwrap();
        assert_eq!(
            rig.store
                .satellite_delivery_tip(old_seat, old_seat)
                .unwrap(),
            Some(legacy.id)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn an_unappointed_or_revoked_project_resolves_to_nothing() {
        let rig = rig();
        // No manager is appointed for the project yet.
        assert_eq!(
            rig.resolution(),
            ScopeResolution::Unresolved(SatelliteScopeStateV1::Unappointed)
        );
        let seat = rig.session(SessionStatus::Completed);
        rig.appoint(seat, None);
        rig.grant("grant");
        assert_eq!(rig.resolution(), ScopeResolution::Live { seat });

        // An explicit empty scope revokes the appointment.
        rig.appoint(seat, Some(Vec::new()));
        assert_eq!(
            rig.resolution(),
            ScopeResolution::Unresolved(SatelliteScopeStateV1::Revoked)
        );
        let status = rig.store.satellite_scope_status().unwrap();
        assert_eq!(status.inbound[0].state, SatelliteScopeStateV1::Revoked);
        assert_eq!(status.inbound[0].seat_session_id, None);

        // A project that does not exist on this host is unappointed too.
        let unknown = rig
            .store
            .resolve_satellite_scope_entry(SatelliteScopeKindV1::Project, Uuid::new_v4())
            .unwrap();
        assert_eq!(
            unknown,
            ScopeResolution::Unresolved(SatelliteScopeStateV1::Unappointed)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn a_seat_that_is_archived_leaves_no_live_seat() {
        let rig = rig();
        let seat = rig.session(SessionStatus::Completed);
        rig.appoint(seat, None);
        rig.store
            .update_session_status(seat, SessionStatus::Archived)
            .unwrap();
        assert_eq!(
            rig.resolution(),
            ScopeResolution::Unresolved(SatelliteScopeStateV1::NoLiveSeat)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn message_push_deploy_and_report_paths_all_use_the_same_resolution() {
        let rig = rig();
        // Nothing appointed: every path refuses.
        let stranger = rig.session(SessionStatus::Completed);
        assert_eq!(rig.verdicts(stranger), [None; 4]);

        // Appointed: every path names the same seat, and only it may report.
        let seat = rig.session(SessionStatus::Completed);
        rig.appoint(seat, None);
        assert_eq!(rig.verdicts(seat), [Some(seat); 4]);
        assert_eq!(rig.verdicts(stranger)[3], None);

        // Re-appointment moves all four at once.
        let reseated = rig.session(SessionStatus::Completed);
        rig.appoint(reseated, None);
        assert_eq!(rig.verdicts(reseated), [Some(reseated); 4]);
        assert_eq!(rig.verdicts(seat)[3], None);

        // Revocation closes all four at once.
        rig.appoint(reseated, Some(Vec::new()));
        assert_eq!(rig.verdicts(reseated), [None; 4]);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn the_real_policy_push_and_report_entry_points_follow_the_project_seat() {
        use rsi_common::harness_manager_v2::{ManagerLaunchChoiceV2, ManagerProviderLimitV2};
        use rsi_common::satellite_dispatch::{AgentReportToHubRequestV1, SatelliteReportKindV1};
        let rig = rig();
        let choice = ManagerLaunchChoiceV2 {
            provider: rsi_common::SessionProvider::Claude,
            model: "model".into(),
            effort: Some("high".into()),
        };
        let push = |rig: &Rig, key: &str| {
            let config = rig.store.get_harness_manager(rig.project).unwrap().unwrap();
            let policy_version = rig
                .store
                .get_harness_manager_policy(rig.project)
                .unwrap()
                .map_or(0, |policy| policy.row_version);
            rig.store
                .apply_hub_manager_policy(&SatelliteManagerPolicyRequestV1 {
                    wire_version: rsi_common::satellite::SATELLITE_WIRE_VERSION_V1,
                    hub_installation_id: SatelliteUuidV1(rig.hub),
                    sender_session_id: Uuid::new_v4(),
                    request: AgentPushSatelliteManagerPolicyRequestV1 {
                        project_id: None,
                        peer_id: SatelliteUuidV1(Uuid::new_v4()),
                        // The stable declared address: the project, not a session.
                        remote_session_id: SatelliteUuidV1(rig.project),
                        expected_scope_version: config.row_version,
                        expected_policy_version: policy_version,
                        idempotency_key: key.into(),
                        policy: SatelliteManagerPolicyPatchV1 {
                            allowed_launches: vec![choice.clone()],
                            max_created_sessions: 1,
                            max_active_sessions: 1,
                            provider_limits: vec![ManagerProviderLimitV2 {
                                provider: rsi_common::SessionProvider::Claude,
                                max_active: 1,
                            }],
                        },
                    },
                })
        };
        let report = |rig: &Rig, caller: Uuid| {
            rig.store.queue_hub_report(
                caller,
                &AgentReportToHubRequestV1 {
                    kind: SatelliteReportKindV1::Result,
                    text: "RESULT ok".into(),
                },
            )
        };

        let first = rig.session(SessionStatus::Completed);
        rig.appoint(first, None);
        rig.grant("operator-grant-1");
        push(&rig, "push-1").unwrap();
        report(&rig, first).unwrap();

        // The operator re-seats a new manager and saves its policy: the
        // pinned-session failure of #1928. The same declared project reaches
        // the new seat with no scope edit on either host.
        let reseated = rig.session(SessionStatus::Completed);
        rig.appoint(reseated, None);
        rig.grant("operator-grant-2");
        push(&rig, "push-2").unwrap();
        report(&rig, reseated).unwrap();
        let refused = report(&rig, first).unwrap_err().to_string();
        assert!(
            refused.contains("satellite_report_not_authorized"),
            "{refused}"
        );

        // A revoked appointment refuses the push and the report.
        rig.appoint(reseated, Some(Vec::new()));
        assert!(push(&rig, "push-3").is_err());
        assert!(report(&rig, reseated).is_err());
    }

    /// #3028: a spent created-sessions cap on a seat whose policy a hub pushed
    /// tells the seat to report to the hub (whose PM raises it with
    /// `AgentPushSatelliteManagerPolicy`); a seat the operator configured keeps
    /// the operator path only.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn a_spent_creation_cap_on_a_hub_pushed_seat_names_the_hub_report() {
        use rsi_common::harness_manager_v2::{ManagerLaunchChoiceV2, ManagerProviderLimitV2};
        use rsi_common::manager_budget::{BudgetNodeKindV1, CreationCapRefusalV1};
        let rig = rig();
        let seat = rig.session(SessionStatus::Completed);
        rig.appoint(seat, None);
        rig.grant("operator-grant");
        let refusal = |container: bool| {
            let config = rig.store.get_harness_manager(rig.project).unwrap().unwrap();
            let error = rig
                .store
                .own_creation_refusal(&config, container, 32, 32)
                .unwrap()
                .to_string();
            CreationCapRefusalV1::parse(&error).unwrap_or_else(|| panic!("{error}"))
        };
        let operator = refusal(false);
        assert_eq!(operator.node_kind, BudgetNodeKindV1::Project);
        assert!(operator.raise_in.starts_with("TUI: gm"), "{operator:?}");

        let config = rig.store.get_harness_manager(rig.project).unwrap().unwrap();
        let policy_version = rig
            .store
            .get_harness_manager_policy(rig.project)
            .unwrap()
            .map_or(0, |policy| policy.row_version);
        rig.store
            .apply_hub_manager_policy(&SatelliteManagerPolicyRequestV1 {
                wire_version: rsi_common::satellite::SATELLITE_WIRE_VERSION_V1,
                hub_installation_id: SatelliteUuidV1(rig.hub),
                sender_session_id: Uuid::new_v4(),
                request: AgentPushSatelliteManagerPolicyRequestV1 {
                    project_id: None,
                    peer_id: SatelliteUuidV1(Uuid::new_v4()),
                    remote_session_id: SatelliteUuidV1(rig.project),
                    expected_scope_version: config.row_version,
                    expected_policy_version: policy_version,
                    idempotency_key: "push-cap".into(),
                    policy: SatelliteManagerPolicyPatchV1 {
                        allowed_launches: vec![ManagerLaunchChoiceV2 {
                            provider: rsi_common::SessionProvider::Claude,
                            model: "model".into(),
                            effort: Some("high".into()),
                        }],
                        max_created_sessions: 32,
                        max_active_sessions: 1,
                        provider_limits: vec![ManagerProviderLimitV2 {
                            provider: rsi_common::SessionProvider::Claude,
                            max_active: 1,
                        }],
                    },
                },
            })
            .unwrap();
        let pushed = refusal(false);
        assert!(
            pushed
                .raise_in
                .starts_with("report to the hub: AgentReportToHub kind=result"),
            "{pushed:?}"
        );
        assert!(
            pushed
                .raise_in
                .contains("max_created_sessions used 32 of 32")
        );
        assert!(pushed.raise_in.contains("AgentPushSatelliteManagerPolicy"));
        assert!(pushed.raise_in.ends_with(&operator.raise_in), "{pushed:?}");
        // The push patch carries no container cap: that stays operator-only.
        assert!(refusal(true).raise_in.starts_with("TUI: gm"));
    }

    /// #268: the old seat is still declared first as a fixed session root
    /// after the operator replaced it. The satellite's manager seat (the owner
    /// of a hub deploy's settlement wake and of a hub QA run) is the session
    /// that holds the published appointment now, read fresh as a restarted
    /// daemon reads it, whether the new seat is reached through the declared
    /// project or through its own declared root and rotation.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn a_replaced_seat_declared_first_as_a_root_yields_to_the_appointed_seat() {
        let rig = rig();
        let deposed = rig.session(SessionStatus::Completed);
        rig.appoint(deposed, None);
        let reseated = rig.session(SessionStatus::Completed);
        rig.appoint(reseated, None);
        let declare = |roots: Vec<Uuid>, projects: Vec<Uuid>| {
            rig.store
                .put_satellite_inbound_policy(&SatelliteInboundPolicyV1 {
                    allowed_hub_installations: vec![SatelliteUuidV1(rig.hub)],
                    scope_roots: roots.into_iter().map(SatelliteUuidV1).collect(),
                    scope_projects: projects.into_iter().map(SatelliteUuidV1).collect(),
                    ..Default::default()
                })
                .unwrap();
        };

        // Roots sort before projects: the deposed root comes first.
        declare(vec![deposed], vec![rig.project]);
        let scope = rig.store.inbound_scope().unwrap();
        assert_eq!(scope.first_live(), Some((rig.project, reseated)));

        // The new seat rotates; its declared root reaches the current tip.
        let tip = rig.rotate_seat(reseated);
        declare(vec![deposed, reseated], Vec::new());
        let scope = rig.store.inbound_scope().unwrap();
        assert_eq!(scope.first_live(), Some((reseated, tip)));

        // Only the deposed seat is declared: no manager seat, so the caller
        // refuses and the hub sees it, instead of resuming the deposed seat.
        declare(vec![deposed], Vec::new());
        let scope = rig.store.inbound_scope().unwrap();
        assert_eq!(scope.first_live(), None);

        // Re-appointing the old session makes it the seat again.
        rig.appoint(deposed, None);
        let scope = rig.store.inbound_scope().unwrap();
        assert_eq!(scope.first_live(), Some((deposed, deposed)));
    }

    /// #272: a hub message addressed to a replaced seat's id (still declared
    /// as a scope root) reaches the declared project's current seat, and the
    /// delivery names the replaced seat; the root's other descendants keep
    /// their own tips. A replaced seat whose project is not declared keeps its
    /// delivery, and with a revoked appointment the address is refused.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn a_hub_message_to_a_replaced_seat_root_reaches_the_current_seat() {
        let rig = rig();
        let deposed = rig.session(SessionStatus::Completed);
        rig.appoint(deposed, None);
        let worker = rig.child_of(deposed);
        let current = rig.session(SessionStatus::Completed);
        rig.appoint(current, None);
        let declare = |projects: Vec<Uuid>| {
            rig.store
                .put_satellite_inbound_policy(&SatelliteInboundPolicyV1 {
                    allowed_hub_installations: vec![SatelliteUuidV1(rig.hub)],
                    scope_roots: vec![SatelliteUuidV1(deposed)],
                    scope_projects: projects.into_iter().map(SatelliteUuidV1).collect(),
                    ..Default::default()
                })
                .unwrap();
        };
        let deliver = |target: Uuid| {
            let scope = rig.store.inbound_scope().unwrap();
            let hit = rig.store.inbound_scope_hit(&scope, target, None).unwrap()?;
            rig.store
                .inbound_scope_delivery(&scope, hit, target)
                .unwrap()
        };

        declare(vec![rig.project]);
        assert_eq!(
            deliver(deposed),
            Some(InboundDelivery {
                seat: current,
                displaced: Some(deposed),
            })
        );
        assert_eq!(
            deliver(worker),
            Some(InboundDelivery {
                seat: worker,
                displaced: None,
            })
        );

        // The current seat rotates: the old address reaches the new tip.
        let tip = rig.rotate_seat(current);
        assert_eq!(
            deliver(deposed),
            Some(InboundDelivery {
                seat: tip,
                displaced: Some(deposed),
            })
        );

        declare(Vec::new());
        assert_eq!(
            deliver(deposed),
            Some(InboundDelivery {
                seat: deposed,
                displaced: None,
            })
        );

        declare(vec![rig.project]);
        rig.appoint(tip, Some(Vec::new()));
        assert_eq!(deliver(deposed), None);
    }

    /// #2880: a replaced seat stays displaced when its replacement leaves no
    /// live seat (archived, deleted, or the appointment revoked): it owns no
    /// manager seat, receives no manager mail, and a later appointment
    /// restores the redirect. A descendant declared as its own root keeps its
    /// own delivery throughout.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn a_displaced_seat_stays_displaced_when_its_replacement_leaves_no_live_seat() {
        for end in ["archived", "deleted", "revoked"] {
            let rig = rig();
            let deposed = rig.session(SessionStatus::Completed);
            rig.appoint(deposed, None);
            let worker = rig.child_of(deposed);
            let replacement = rig.session(SessionStatus::Completed);
            rig.appoint(replacement, None);
            rig.declare(vec![deposed, worker], vec![rig.project]);
            assert_eq!(
                rig.manager_seat(),
                Some((rig.project, replacement)),
                "{end}"
            );
            assert_eq!(
                rig.deliver(deposed),
                Some(InboundDelivery {
                    seat: replacement,
                    displaced: Some(deposed),
                }),
                "{end}"
            );

            match end {
                "archived" => rig
                    .store
                    .update_session_status(replacement, SessionStatus::Archived)
                    .unwrap(),
                "deleted" => rig
                    .store
                    .update_session_status(replacement, SessionStatus::Deleted)
                    .unwrap(),
                _ => rig.appoint(replacement, Some(Vec::new())),
            }

            // Ownership: the displaced root is not the manager seat.
            assert_eq!(rig.manager_seat(), None, "{end}");
            // Mail: refused rather than delivered to the old seat.
            assert_eq!(rig.deliver(deposed), None, "{end}");
            assert_eq!(
                rig.store.displaced_seat_route(deposed).unwrap(),
                DisplacedSeat::NoLiveSeat {
                    project: rig.project
                },
                "{end}"
            );
            // A separately declared descendant keeps its own delivery.
            assert_eq!(rig.deliver(worker), Rig::direct(worker), "{end}");

            if end != "revoked" {
                // A successor appointment makes the old address reach it.
                let successor = rig.session(SessionStatus::Completed);
                rig.appoint(successor, None);
                assert_eq!(rig.manager_seat(), Some((rig.project, successor)), "{end}");
                assert_eq!(
                    rig.deliver(deposed),
                    Some(InboundDelivery {
                        seat: successor,
                        displaced: Some(deposed),
                    }),
                    "{end}"
                );
            }
        }
    }

    /// #2880: the evidence that a seat held the appointment follows its
    /// rotation lineage, so a replaced seat that had rotated is still
    /// displaced when its replacement is archived.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn a_rotated_replaced_seat_is_displaced_when_its_replacement_is_archived() {
        let rig = rig();
        let first = rig.session(SessionStatus::Completed);
        rig.appoint(first, None);
        let rotated = rig.rotate_seat(first);
        let replacement = rig.session(SessionStatus::Completed);
        rig.appoint(replacement, None);
        rig.declare(vec![first], vec![rig.project]);
        assert_eq!(
            rig.deliver(first),
            Some(InboundDelivery {
                seat: replacement,
                displaced: Some(rotated),
            })
        );
        rig.store
            .update_session_status(replacement, SessionStatus::Archived)
            .unwrap();
        assert_eq!(rig.manager_seat(), None);
        assert_eq!(rig.deliver(first), None);
    }

    /// #2881: a never-appointed live session declared as a root next to its
    /// managed project keeps direct delivery (it is not redirected to the
    /// project's manager), whether the manager is live, archived or revoked,
    /// and it never becomes the manager seat. Without any appointment it
    /// remains the satellite's seat as before.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn a_never_appointed_root_beside_its_managed_project_keeps_direct_delivery() {
        let rig = rig();
        let bystander = rig.session(SessionStatus::Completed);
        let worker = rig.child_of(bystander);
        // No appointment at all: the root is the seat, delivery is direct.
        // Only the bystander is declared here: two unappointed roots are
        // ordered by their random ids, which would make the seat arbitrary.
        rig.declare(vec![bystander], vec![rig.project]);
        assert_eq!(rig.manager_seat(), Some((bystander, bystander)));
        assert_eq!(rig.deliver(bystander), Rig::direct(bystander));
        rig.declare(vec![bystander, worker], vec![rig.project]);

        let manager = rig.session(SessionStatus::Completed);
        rig.appoint(manager, None);
        assert_eq!(rig.manager_seat(), Some((rig.project, manager)));
        assert_eq!(rig.deliver(bystander), Rig::direct(bystander));
        assert_eq!(rig.deliver(worker), Rig::direct(worker));
        assert_eq!(
            rig.store.displaced_seat_route(bystander).unwrap(),
            DisplacedSeat::NotDisplaced
        );

        // The manager has no live tip: the bystander still gets its mail,
        // and still does not own the manager seat.
        rig.store
            .update_session_status(manager, SessionStatus::Archived)
            .unwrap();
        assert_eq!(rig.manager_seat(), None);
        assert_eq!(rig.deliver(bystander), Rig::direct(bystander));
        assert_eq!(
            rig.store.displaced_seat_route(bystander).unwrap(),
            DisplacedSeat::NotDisplaced
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn fixed_session_declarations_still_resolve_and_descend() {
        let rig = rig();
        let root = rig.session(SessionStatus::Completed);
        let mut child = make_test_session();
        child.parent_id = Some(root);
        child.status = SessionStatus::Completed;
        rig.store.insert_session(&child).unwrap();
        rig.store
            .put_satellite_inbound_policy(&SatelliteInboundPolicyV1 {
                allowed_hub_installations: vec![SatelliteUuidV1(rig.hub)],
                scope_roots: vec![SatelliteUuidV1(root)],
                ..Default::default()
            })
            .unwrap();
        let scope = rig.store.inbound_scope().unwrap();
        assert_eq!(scope.seat_for_address(root), Some(root));
        assert_eq!(scope.first_live(), Some((root, root)));
        assert!(scope.is_seat(root));
        assert_eq!(
            rig.store.inbound_scope_hit(&scope, child.id, None).unwrap(),
            Some(InboundHit::Root(root))
        );
        assert_eq!(
            rig.store
                .inbound_scope_delivery_seat(&scope, InboundHit::Root(root), child.id)
                .unwrap(),
            Some(child.id)
        );
        // A session outside every declared root is not in scope.
        let outsider = rig.session(SessionStatus::Completed);
        assert_eq!(
            rig.store.inbound_scope_hit(&scope, outsider, None).unwrap(),
            None
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn descendants_of_the_current_project_seat_are_in_scope_and_old_seats_are_not() {
        let rig = rig();
        let seat = rig.session(SessionStatus::Completed);
        rig.appoint(seat, None);
        let mut child = make_test_session();
        child.parent_id = Some(seat);
        child.status = SessionStatus::Completed;
        rig.store.insert_session(&child).unwrap();
        let scope = rig.store.inbound_scope().unwrap();
        assert_eq!(
            rig.store.inbound_scope_hit(&scope, child.id, None).unwrap(),
            Some(InboundHit::Root(seat))
        );
        // After re-appointment the old seat's children leave the scope.
        let reseated = rig.session(SessionStatus::Completed);
        rig.appoint(reseated, None);
        let scope = rig.store.inbound_scope().unwrap();
        assert_eq!(
            rig.store.inbound_scope_hit(&scope, child.id, None).unwrap(),
            None
        );
    }

    /// #1928 (review #1943): a project-bound target is validated against the
    /// live appointment, so a hub that declared only the project reaches the
    /// seat's tree across rotation, and loses an old seat's tree on
    /// re-appointment, with no scope edit on either host.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn a_project_bound_target_follows_the_live_seat_tree_across_rotation_and_reappointment() {
        let rig = rig();
        let project = rig.project;
        let seat = rig.session(SessionStatus::Completed);
        rig.appoint(seat, None);
        let child = rig.child_of(seat);
        let grandchild = rig.child_of(child);
        let outsider = rig.session(SessionStatus::Completed);

        // The address, the seat itself and its whole tree; nobody else.
        assert_eq!(rig.bound_delivery(project, project), Some(seat));
        assert_eq!(rig.bound_delivery(seat, project), Some(seat));
        assert_eq!(rig.bound_delivery(child, project), Some(child));
        assert_eq!(rig.bound_delivery(grandchild, project), Some(grandchild));
        assert_eq!(rig.bound_delivery(outsider, project), None);
        assert_eq!(rig.bound_delivery(Uuid::new_v4(), project), None);

        // A project this host does not declare binds nothing, whatever the
        // target: the hub cannot widen the satellite's own policy.
        let scope = rig.store.inbound_scope().unwrap();
        assert_eq!(
            rig.store
                .inbound_scope_hit(&scope, child, Some(Uuid::new_v4()))
                .unwrap(),
            None
        );

        // Self-succession: the tree of the rotated seat stays in scope, the
        // predecessor is reached at the successor, and new children count.
        let successor = rig.rotate_seat(seat);
        assert_eq!(rig.bound_delivery(child, project), Some(child));
        assert_eq!(rig.bound_delivery(grandchild, project), Some(grandchild));
        assert_eq!(rig.bound_delivery(seat, project), Some(successor));
        let fresh = rig.child_of(successor);
        assert_eq!(rig.bound_delivery(fresh, project), Some(fresh));
        assert_eq!(rig.bound_delivery(outsider, project), None);

        // Operator re-appointment: a different manager owns the project now.
        // The previous seat's tree is out of scope; the new tree is in.
        let reseated = rig.session(SessionStatus::Completed);
        rig.appoint(reseated, None);
        let reseated_child = rig.child_of(reseated);
        assert_eq!(rig.bound_delivery(project, project), Some(reseated));
        assert_eq!(
            rig.bound_delivery(reseated_child, project),
            Some(reseated_child)
        );
        assert_eq!(rig.bound_delivery(child, project), None);
        assert_eq!(rig.bound_delivery(fresh, project), None);
        assert_eq!(rig.bound_delivery(successor, project), None);

        // A revoked appointment resolves to nothing for every target.
        rig.appoint(reseated, Some(Vec::new()));
        for target in [project, reseated, reseated_child] {
            assert_eq!(rig.bound_delivery(target, project), None);
        }
    }

    /// #2750: a Codex seat whose provider is down, in a project the hub's peer
    /// scope pins by that seat's session id.
    fn failed_codex_seat(rig: &Rig) -> Uuid {
        let mut seat = make_test_session();
        seat.provider = rsi_common::types::SessionProvider::Codex;
        seat.status = SessionStatus::Failed;
        rig.store.insert_session(&seat).unwrap();
        seat.id
    }

    /// The Claude successor the daemon launches: `continued_from` the seat.
    fn claude_successor(rig: &Rig, seat: Uuid) -> Uuid {
        let mut successor = make_test_session();
        successor.provider = rsi_common::types::SessionProvider::Claude;
        successor.status = SessionStatus::Running;
        successor.continued_from = Some(seat);
        successor.rotation_depth += 1;
        rig.store.insert_session(&successor).unwrap();
        successor.id
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn a_codex_seat_reseated_onto_claude_keeps_the_hub_dispatch_reach() {
        use rsi_common::satellite::{SatellitePeerConfigV1, SatellitePutPeerRequestV1};
        let rig = rig();
        let seat = failed_codex_seat(&rig);
        rig.appoint(seat, None);
        rig.grant("grant-before-outage");
        // The hub pins the failed seat's session id; the satellite declares it
        // as an inbound root. Neither is edited by the reseat.
        let peer = Uuid::new_v4();
        rig.store
            .put_satellite_peer(
                &SatellitePutPeerRequestV1 {
                    expected_registry_revision: 1,
                    peer: SatellitePeerConfigV1 {
                        peer_id: SatelliteUuidV1(peer),
                        label: "laptop".into(),
                        expected_installation_id: Some(SatelliteUuidV1(Uuid::new_v4())),
                        enabled: true,
                        read_enabled: true,
                        dispatch_enabled: true,
                    },
                    repair_quarantine: false,
                },
                std::path::Path::new("/tmp/satellites"),
            )
            .unwrap();
        rig.store
            .put_satellite_peer_scope(2, peer, &[SatelliteUuidV1(seat)])
            .unwrap();
        rig.declare(vec![seat], vec![]);
        assert_eq!(rig.deliver(seat), Rig::direct(seat));

        let plan = rig.store.manager_reseat_plan(rig.project, false).unwrap();
        assert_eq!(plan.seat.id, seat);
        assert!(plan.policy.is_some(), "the live grant is carried");
        let successor = claude_successor(&rig, seat);
        let commit = rig
            .store
            .commit_manager_reseat(&plan, successor, "codex login expired")
            .unwrap();

        // The appointment and its scope moved to the successor.
        assert_eq!(commit.config.current_session_id, Some(successor));
        assert_eq!(commit.config.manager_session_id, successor);
        assert_eq!(commit.config.scope_mode, plan.config.scope_mode);
        let policy = rig
            .store
            .get_harness_manager_policy(rig.project)
            .unwrap()
            .unwrap();
        assert!(!policy.revoked, "the grant is re-granted, not left a draft");
        assert_eq!(policy.manager_session_id, successor);
        assert_eq!(policy.policy, plan.policy.as_ref().unwrap().policy);
        // The reason is a durable record on the new appointment.
        let recorded: String = rig
            .store
            .conn
            .query_row(
                "SELECT payload_json FROM harness_manager_v2_events WHERE kind='seat_reseat'
                 AND project_id=?1",
                [rig.project.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        let recorded: serde_json::Value = serde_json::from_str(&recorded).unwrap();
        assert_eq!(recorded["reason"], "codex login expired");
        assert_eq!(recorded["predecessor_session_id"], seat.to_string());
        assert_eq!(recorded["successor_session_id"], successor.to_string());

        // Hub dispatch keeps working with no scope edit on either host: the
        // hub still authorizes the declared (old) address, and the satellite
        // resolves that address to the Claude successor through lineage.
        assert!(
            rig.store
                .dispatch_peer_facts(peer, seat, None)
                .unwrap()
                .is_some()
        );
        assert!(rig.store.hub_peer_declares_address(peer, seat).unwrap());
        assert_eq!(
            rig.store
                .resolve_satellite_scope_entry(SatelliteScopeKindV1::Session, seat)
                .unwrap(),
            ScopeResolution::Live { seat: successor }
        );
        assert_eq!(rig.deliver(seat), Rig::direct(successor));
        assert_eq!(rig.manager_seat(), Some((seat, successor)));
        // A retry finds the healthy successor and does not launch a second.
        let again = rig
            .store
            .manager_reseat_plan(rig.project, true)
            .unwrap_err();
        assert!(again.to_string().contains(RESEAT_SEAT_LIVE), "{again}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn a_reseat_replaces_only_a_seat_that_is_down() {
        let rig = rig();
        // Running, and Completed without the assertion, are not down.
        let seat = rig.session(SessionStatus::Running);
        rig.appoint(seat, None);
        for (status, asserted, refusal) in [
            (SessionStatus::Running, true, RESEAT_SEAT_LIVE),
            (SessionStatus::Starting, true, RESEAT_SEAT_LIVE),
            (SessionStatus::WaitingApproval, true, RESEAT_SEAT_LIVE),
            (SessionStatus::Completed, false, RESEAT_SEAT_NOT_FAILED),
            (SessionStatus::Interrupted, false, RESEAT_SEAT_NOT_FAILED),
        ] {
            rig.store.update_session_status(seat, status).unwrap();
            let error = rig
                .store
                .manager_reseat_plan(rig.project, asserted)
                .unwrap_err();
            assert!(error.to_string().contains(refusal), "{status:?}: {error}");
        }
        // Failed needs no assertion; a stopped seat accepts the assertion.
        rig.store
            .update_session_status(seat, SessionStatus::Failed)
            .unwrap();
        assert!(rig.store.manager_reseat_plan(rig.project, false).is_ok());
        rig.store
            .update_session_status(seat, SessionStatus::Completed)
            .unwrap();
        assert!(rig.store.manager_reseat_plan(rig.project, true).is_ok());
        // A project with no appointment has nothing to reseat.
        let other = Store::open_in_memory().unwrap();
        let error = other
            .manager_reseat_plan(crate::store::d04_test_project_id(), true)
            .unwrap_err();
        assert!(error.to_string().contains(RESEAT_NOT_CONFIGURED), "{error}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn a_reseat_with_no_confirmed_grant_grants_nothing() {
        let rig = rig();
        let seat = failed_codex_seat(&rig);
        rig.appoint(seat, None);
        // No grant was ever confirmed: nothing to re-grant.
        let plan = rig.store.manager_reseat_plan(rig.project, false).unwrap();
        assert!(plan.policy.is_none());
        let successor = claude_successor(&rig, seat);
        let commit = rig
            .store
            .commit_manager_reseat(&plan, successor, "outage")
            .unwrap();
        assert!(commit.policy.is_none());
        assert_eq!(commit.config.current_session_id, Some(successor));
        assert!(
            rig.store
                .get_harness_manager_policy(rig.project)
                .unwrap()
                .is_none()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn the_hub_declares_and_verifies_session_and_project_addresses_alike() {
        use rsi_common::satellite::{SatellitePeerConfigV1, SatellitePutPeerRequestV1};
        let store = Store::open_in_memory().expect("in-memory store");
        let peer = Uuid::new_v4();
        store
            .put_satellite_peer(
                &SatellitePutPeerRequestV1 {
                    expected_registry_revision: 1,
                    peer: SatellitePeerConfigV1 {
                        peer_id: SatelliteUuidV1(peer),
                        label: "laptop".into(),
                        expected_installation_id: Some(SatelliteUuidV1(Uuid::new_v4())),
                        enabled: true,
                        read_enabled: true,
                        dispatch_enabled: true,
                    },
                    repair_quarantine: false,
                },
                std::path::Path::new("/tmp/satellites"),
            )
            .unwrap();
        assert!(!store.hub_peer_declares_scope(peer).unwrap());
        let (session, project, other) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        store
            .put_satellite_peer_scope_entries(
                2,
                peer,
                &[SatelliteUuidV1(session)],
                &[SatelliteUuidV1(project)],
            )
            .unwrap();
        assert!(store.hub_peer_declares_scope(peer).unwrap());
        assert!(store.hub_peer_declares_address(peer, session).unwrap());
        assert!(store.hub_peer_declares_address(peer, project).unwrap());
        assert!(!store.hub_peer_declares_address(peer, other).unwrap());
        assert_eq!(
            store.satellite_peer_project_scope(peer).unwrap(),
            vec![SatelliteUuidV1(project)]
        );
        let view = store.satellite_registry_view().unwrap();
        assert_eq!(
            view.peers[0].dispatch_project_scope,
            vec![SatelliteUuidV1(project)]
        );
        // A project-only declaration is a declaration (deploy, reports).
        store
            .put_satellite_peer_scope_entries(3, peer, &[], &[SatelliteUuidV1(project)])
            .unwrap();
        assert!(store.hub_peer_declares_scope(peer).unwrap());
        assert!(!store.hub_peer_declares_address(peer, session).unwrap());
        // Replacing with nothing clears both kinds.
        store
            .put_satellite_peer_scope_entries(4, peer, &[], &[])
            .unwrap();
        assert!(!store.hub_peer_declares_scope(peer).unwrap());
        assert!(!store.hub_peer_declares_address(peer, project).unwrap());
    }
}
