//! Converge branched Epic lineages the pinned V112 identity backfill cannot
//! represent, without losing the authority attribution the branch encodes.
//!
//! V112 models an Epic child lineage as linear: at most one `continued_from`
//! successor per predecessor. Committed, immutable receipts prove that is a
//! modelling shortcut rather than a fact about the data — two distinct logical
//! spawn reservations can share one predecessor, and only one of them inherits
//! that predecessor's Epic ordinal. A branch therefore aborts startup.
//!
//! Three cooperating pieces live here:
//!
//! 1. [`Store::normalize_v111_branched_epic_lineage`] — a strictly data-only,
//!    idempotent normalization that runs at V111, *before* V112 reads the
//!    lineage. It re-roots the non-canonical successor of each branch
//!    (`continued_from := NULL`) and journals every detachment into
//!    `rotation_events`. It creates no schema object, so the exact V111
//!    `sqlite_master` fingerprint V112 authenticates against is preserved.
//! 2. The receipt lookups the manager subsystem uses to prove an original
//!    attribution once `continued_from` no longer carries it. Their backing
//!    relation is installed by the V114 migration in `store/mod.rs`.
//! 3. [`annotate_v112_branched_lineage_fault`] — accurate operator context for
//!    the pinned, stale-labelled V112 branch refusal.
//!
//! The decision rules read receipts only: never a title, never a timestamp,
//! never a kind. Anything unclassifiable aborts the whole transaction with a
//! message naming the predecessor.

use std::collections::BTreeMap;

use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::Serialize;
use uuid::Uuid;

use crate::error::{DaemonError, Result};
use crate::store::Store;

/// Journal `phase` written for every V111 branch detachment. Distinct from all
/// four values the rotation pipeline already writes.
pub(crate) const V111_NORMALIZATION_PHASE: &str = "v111_branch_normalization";

/// Journal `event_type` written for every V111 branch detachment. Distinct from
/// all six values the rotation pipeline already writes.
pub(crate) const V111_DETACHED_EVENT_TYPE: &str = "lineage_detached";

/// Stable schema tag shared by the journal `metadata` payload and the V114
/// receipt rows, so a row is self-describing to a human reading it raw.
pub(crate) const V111_NORMALIZATION_SCHEMA: &str = "v111-branch-normalization/1";

/// Bounded fan-out. A predecessor with more successors than this is not a race,
/// it is corruption, and normalization refuses rather than guessing.
const MAX_BRANCH_FAN_OUT: usize = 16;

/// Receipt class that proves an original `continued_from` edge survived the
/// detachment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LineageReceiptClass {
    /// `harness_manager_rotation_edges` — manager appointment identity.
    HarnessManagerRotationEdge,
    /// `agent_successor_reservations` at `state='committed'` — the Epic
    /// lead-turnover kernel.
    CommittedReservation,
}

impl LineageReceiptClass {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::HarnessManagerRotationEdge => "harness_manager_rotation_edge",
            Self::CommittedReservation => "committed_reservation",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "harness_manager_rotation_edge" => Some(Self::HarnessManagerRotationEdge),
            "committed_reservation" => Some(Self::CommittedReservation),
            _ => None,
        }
    }
}

/// One branch successor with the receipts that prove its authority path.
#[derive(Clone, Debug)]
struct BranchSuccessor {
    session_id: String,
    reservation_id: Option<String>,
    expected_lead_generation: Option<i64>,
    rotation_edge_committed_at: Option<String>,
}

impl BranchSuccessor {
    fn has_committed_reservation(&self) -> bool {
        self.reservation_id.is_some()
    }

    fn has_rotation_edge(&self) -> bool {
        self.rotation_edge_committed_at.is_some()
    }

    fn receipt_class(&self) -> Option<LineageReceiptClass> {
        if self.has_committed_reservation() {
            Some(LineageReceiptClass::CommittedReservation)
        } else if self.has_rotation_edge() {
            Some(LineageReceiptClass::HarnessManagerRotationEdge)
        } else {
            None
        }
    }
}

/// Self-describing `rotation_events.metadata` payload. Serialized from a struct
/// with a fixed field order — never hand-built JSON text.
#[derive(Clone, Debug, Serialize)]
struct DetachmentJournalMetadata {
    schema: &'static str,
    predecessor: String,
    detached_successor: String,
    prior_continued_from: String,
    canonical_successor: String,
    decision_rule: &'static str,
    receipt_class: &'static str,
    expected_lead_generation: Option<i64>,
    canonical_expected_lead_generation: Option<i64>,
    reservation_id: Option<String>,
    rotation_edge_committed_at: Option<String>,
}

/// Deterministic journal `rotation_id`, so a replay is recognisable by value
/// rather than by position.
pub(crate) fn detachment_rotation_id(predecessor: &str, detached: &str) -> Uuid {
    Uuid::new_v5(
        &Uuid::NAMESPACE_OID,
        format!("{V111_NORMALIZATION_SCHEMA}\0{predecessor}\0{detached}").as_bytes(),
    )
}

/// Append accurate context to the pinned, stale-labelled V112 branch refusal.
///
/// The message the released V112 body raises still says "V99". That text is
/// inside a protected section and cannot be repaired in place, so the forward
/// path is this unpinned wrapper at `init_schema`. Every other error passes
/// through byte-identical and the variant is never reclassified.
pub(crate) fn annotate_v112_branched_lineage_fault(error: DaemonError) -> DaemonError {
    const PINNED_TEXT: &str = "V99 identity backfill found a branched Epic lineage";
    match &error {
        DaemonError::Store(text) if text == PINNED_TEXT => DaemonError::Store(format!(
            "{PINNED_TEXT} (the label is stale: this is the V112 identity backfill). \
             The V111 branch normalization either did not run — the source catalog did not \
             authenticate as an exact V111 catalog, in which case V112 raises its own catalog \
             refusal instead — or it could not classify a branch from committed receipts and \
             refused rather than guessing. See docs/v111-branched-lineage-recovery.md."
        )),
        _ => error,
    }
}

impl Store {
    /// Converge branched Epic lineages the pinned V112 identity backfill cannot
    /// represent.
    ///
    /// Data-only and idempotent: it runs in its own `Immediate` transaction,
    /// issues only `UPDATE sessions` and `INSERT INTO rotation_events`, never
    /// creates a schema object, and never writes `PRAGMA user_version`. The
    /// work set is derived solely from the branch query, so once every branch
    /// is converged a re-run performs zero writes.
    pub(crate) fn normalize_v111_branched_epic_lineage(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;

        // Gate A. Never mutate a database V112 will refuse anyway: an
        // unauthenticated source catalog gets V112's own catalog refusal, with
        // its fingerprint left byte-identical.
        if super::classify_v112_source_catalog(&tx).is_err() {
            tx.commit()?;
            return Ok(());
        }

        // Gate B. Idempotency predicate: the exact input V112's branch guard
        // reads. Empty means a provable no-op.
        let branches = collect_branch_set(&tx)?;
        if branches.is_empty() {
            tx.commit()?;
            return Ok(());
        }

        let mut detached_total = 0usize;
        for (predecessor, successor_ids) in branches {
            if successor_ids.len() > MAX_BRANCH_FAN_OUT {
                return Err(DaemonError::Store(format!(
                    "V111 branch normalization refuses predecessor {predecessor}: \
                     {} successors exceed the bounded fan-out of {MAX_BRANCH_FAN_OUT}",
                    successor_ids.len()
                )));
            }

            let mut successors = Vec::with_capacity(successor_ids.len());
            for successor_id in successor_ids {
                let receipts = load_branch_receipts(&tx, &predecessor, &successor_id)?;
                if receipts.receipt_class().is_none() {
                    // R0 — fails closed. A successor with no receipt could not
                    // be recorded by V114 either, whose validate trigger demands
                    // a matching receipt. Aborting here keeps V114 consistent.
                    return Err(DaemonError::Store(format!(
                        "V111 branch normalization cannot classify successor {successor_id} \
                         of predecessor {predecessor}: no committed reservation and no manager \
                         rotation edge"
                    )));
                }
                successors.push(receipts);
            }

            // R2 — every successor is rotation-edge only. There is no
            // ordinal-inheriting continuation to keep; refuse rather than pick.
            let Some(canonical_index) = choose_canonical(&successors) else {
                return Err(DaemonError::Store(format!(
                    "V111 branch normalization found no committed reservation among {} \
                     successors of predecessor {predecessor}",
                    successors.len()
                )));
            };
            let canonical = successors[canonical_index].clone();

            for (index, successor) in successors.iter().enumerate() {
                if index == canonical_index {
                    continue;
                }
                assert_detachable(&tx, &successor.session_id)?;
                detach_successor(&tx, &predecessor, &successor.session_id)?;
                journal_detachment(&tx, &predecessor, successor, &canonical)?;
                // One line per detachment. An operator running this against a
                // live database should be able to read exactly which rows moved
                // straight out of the startup log, without opening the journal.
                tracing::info!(
                    detached = %successor.session_id,
                    predecessor = %predecessor,
                    canonical = %canonical.session_id,
                    receipt = successor
                        .receipt_class()
                        .map_or("unclassified", LineageReceiptClass::as_str),
                    rule = "R1",
                    "V111 branch normalization detached a non-canonical Epic lineage successor"
                );
                detached_total += 1;
            }
        }

        tx.commit()?;
        tracing::info!(
            detached = detached_total,
            "V111 branch normalization converged branched Epic lineages"
        );
        Ok(())
    }

    /// Bounded lookup used by `manager_v2_cohort` to prove an original
    /// attribution after a normalization detachment. Read-only; the only
    /// writer of `session_lineage_detachments` is the V114 migration.
    ///
    /// There is deliberately no `_on(tx, ..)` variant: the manager rotation
    /// write path is unreachable for these rows and is not being changed.
    pub(crate) fn lineage_detachment_predecessor(&self, session_id: Uuid) -> Result<Option<Uuid>> {
        let found: Option<String> = self
            .conn
            .query_row(
                "SELECT detached_from_session_id FROM session_lineage_detachments
                 WHERE session_id=?1",
                params![session_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        found.map(parse_stored_uuid).transpose()
    }
}

fn parse_stored_uuid(value: String) -> Result<Uuid> {
    Uuid::parse_str(&value).map_err(|_| {
        DaemonError::Store(format!(
            "stored lineage detachment carries a non-canonical identity: {value}"
        ))
    })
}

/// The exact branch input V112's guard reads: `continued_from` children of an
/// Epic parent whose predecessor is shared by more than one such child.
fn collect_branch_set(tx: &Transaction<'_>) -> Result<BTreeMap<String, Vec<String>>> {
    let mut statement = tx.prepare(
        "SELECT child.continued_from AS predecessor_id, child.id AS successor_id
           FROM sessions child
           JOIN sessions epic
             ON epic.id = child.parent_id AND epic.session_kind = 'Epic'
          WHERE child.continued_from IS NOT NULL
            AND child.continued_from IN (
                 SELECT c2.continued_from
                   FROM sessions c2
                   JOIN sessions e2 ON e2.id = c2.parent_id AND e2.session_kind = 'Epic'
                  WHERE c2.continued_from IS NOT NULL
                  GROUP BY c2.continued_from
                 HAVING count(*) > 1)
          ORDER BY child.continued_from, child.id",
    )?;
    let mut branches: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let predecessor: String = row.get(0)?;
        let successor: String = row.get(1)?;
        branches.entry(predecessor).or_default().push(successor);
    }
    Ok(branches)
}

/// `harness_manager_rotation_edges` is queried without a `retired_at` filter:
/// the edge row is the receipt, and a retired edge still proves the original
/// attribution — the same reasoning the manager-v2 cohort traversal records.
fn load_branch_receipts(
    tx: &Transaction<'_>,
    predecessor: &str,
    successor: &str,
) -> Result<BranchSuccessor> {
    let (reservation_id, expected_lead_generation, rotation_edge_committed_at) = tx.query_row(
        "SELECT
           (SELECT reservation_id FROM agent_successor_reservations
             WHERE predecessor_session_id = ?1 AND candidate_session_id = ?2
               AND state = 'committed'
             ORDER BY reservation_id LIMIT 1),
           (SELECT expected_lead_generation FROM agent_successor_reservations
             WHERE predecessor_session_id = ?1 AND candidate_session_id = ?2
               AND state = 'committed'
             ORDER BY reservation_id LIMIT 1),
           (SELECT committed_at FROM harness_manager_rotation_edges
             WHERE predecessor_session_id = ?1 AND successor_session_id = ?2)",
        params![predecessor, successor],
        |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        },
    )?;
    Ok(BranchSuccessor {
        session_id: successor.to_string(),
        reservation_id,
        expected_lead_generation,
        rotation_edge_committed_at,
    })
}

/// R1 — the committed reservation with the lowest `expected_lead_generation`
/// is the ordinal-inheriting continuation and retains `continued_from`. The
/// fence is a persisted authority value, not a clock. Deterministic tie-break:
/// lowest `reservation_id` lexicographically. Returns `None` when no successor
/// carries a committed reservation (R2).
fn choose_canonical(successors: &[BranchSuccessor]) -> Option<usize> {
    successors
        .iter()
        .enumerate()
        .filter(|(_, successor)| successor.has_committed_reservation())
        .min_by(|(_, left), (_, right)| {
            left.expected_lead_generation
                .cmp(&right.expected_lead_generation)
                .then_with(|| left.reservation_id.cmp(&right.reservation_id))
        })
        .map(|(index, _)| index)
}

/// Turn a trigger `RAISE(ABORT)` — which would surface as an opaque SQLite
/// error — into an accurate, named refusal. `sessions_execution_origin_write_guard`
/// is the only one of the eight `sessions` triggers that can fire on a bare
/// `continued_from` update.
fn assert_detachable(tx: &Transaction<'_>, successor: &str) -> Result<()> {
    let safe: bool = tx.query_row(
        "SELECT execution_origin_claim_id IS NULL AND execution_origin_write_seq = 0
           FROM sessions WHERE id = ?1",
        params![successor],
        |row| row.get(0),
    )?;
    if !safe {
        return Err(DaemonError::Store(format!(
            "V111 branch normalization refuses to detach session {successor}: \
             it carries an execution-origin claim"
        )));
    }
    Ok(())
}

/// `updated_at` is deliberately not touched: every detach candidate is a
/// terminal row, and bumping it would reorder them in every operator-facing
/// recency view for no informational gain. The journal row carries the time.
fn detach_successor(tx: &Transaction<'_>, predecessor: &str, successor: &str) -> Result<()> {
    let changed = tx.execute(
        "UPDATE sessions SET continued_from = NULL WHERE id = ?1 AND continued_from = ?2",
        params![successor, predecessor],
    )?;
    if changed != 1 {
        return Err(DaemonError::Store(format!(
            "V111 branch normalization lost a race on session {successor}"
        )));
    }
    Ok(())
}

fn journal_detachment(
    tx: &Transaction<'_>,
    predecessor: &str,
    detached: &BranchSuccessor,
    canonical: &BranchSuccessor,
) -> Result<()> {
    let receipt_class = detached
        .receipt_class()
        .ok_or_else(|| {
            DaemonError::Store(format!(
                "V111 branch normalization cannot classify successor {} of predecessor \
                 {predecessor}: no committed reservation and no manager rotation edge",
                detached.session_id
            ))
        })?
        .as_str();
    let metadata = DetachmentJournalMetadata {
        schema: V111_NORMALIZATION_SCHEMA,
        predecessor: predecessor.to_string(),
        detached_successor: detached.session_id.clone(),
        prior_continued_from: predecessor.to_string(),
        canonical_successor: canonical.session_id.clone(),
        decision_rule: "R1",
        receipt_class,
        expected_lead_generation: detached.expected_lead_generation,
        canonical_expected_lead_generation: canonical.expected_lead_generation,
        reservation_id: detached.reservation_id.clone(),
        rotation_edge_committed_at: detached.rotation_edge_committed_at.clone(),
    };
    let payload = serde_json::to_string(&metadata).map_err(|error| {
        DaemonError::Store(format!(
            "V111 branch normalization could not serialize the detachment journal: {error}"
        ))
    })?;
    tx.execute(
        "INSERT INTO rotation_events
           (session_id, rotation_id, phase, event_type, metadata, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            detached.session_id,
            detachment_rotation_id(predecessor, &detached.session_id).to_string(),
            V111_NORMALIZATION_PHASE,
            V111_DETACHED_EVENT_TYPE,
            payload,
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
        ],
    )?;
    Ok(())
}

/// Prove the V114 migration installed exactly the catalog it declares —
/// nothing missing, nothing extra under the same name prefix.
pub(crate) fn validate_v114_catalog(tx: &Transaction<'_>) -> Result<()> {
    let mut statement = tx.prepare(
        "SELECT type,name FROM sqlite_master
          WHERE name LIKE 'session_lineage_detachment%'
            AND name NOT LIKE 'sqlite_autoindex_%'
          ORDER BY type,name",
    )?;
    let actual = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let expected = super::V114_LINEAGE_DETACHMENT_CATALOG_OBJECTS
        .into_iter()
        .map(|(kind, name)| (kind.to_string(), name.to_string()))
        .collect::<Vec<_>>();
    if actual != expected {
        return Err(DaemonError::Store(format!(
            "V114 lineage detachment catalog mismatch: {actual:?}"
        )));
    }
    Ok(())
}

/// Deterministically reconstruct the V114 detachment receipts from the receipt
/// tables themselves. Called by the sealed V114 migration driver.
///
/// A session with `continued_from IS NULL` that a committed reservation or a
/// manager rotation edge names as a successor can only exist because something
/// nulled the column. `harness_manager_rotation_edges` rows are only insertable
/// while `current.continued_from == Some(predecessor)`, a committed reservation
/// validates the same at commit, and no production writer nulls the column —
/// the V111 normalization is the only source of this state.
pub(crate) fn reconstruct_v114_detachment_rows(tx: &Transaction<'_>) -> Result<usize> {
    let mut claims: BTreeMap<String, BTreeMap<String, Vec<LineageReceiptClass>>> = BTreeMap::new();
    {
        let mut statement = tx.prepare(
            "SELECT s.id, e.predecessor_session_id, 'harness_manager_rotation_edge'
               FROM sessions s
               JOIN harness_manager_rotation_edges e ON e.successor_session_id = s.id
              WHERE s.continued_from IS NULL
             UNION
             SELECT s.id, r.predecessor_session_id, 'committed_reservation'
               FROM sessions s
               JOIN agent_successor_reservations r
                 ON r.candidate_session_id = s.id AND r.state = 'committed'
              WHERE s.continued_from IS NULL
              ORDER BY 1, 3",
        )?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let session_id: String = row.get(0)?;
            let predecessor: String = row.get(1)?;
            let class_text: String = row.get(2)?;
            let class = LineageReceiptClass::parse(&class_text).ok_or_else(|| {
                DaemonError::Store(format!(
                    "V114 reconstruction produced an unknown receipt class {class_text}"
                ))
            })?;
            claims
                .entry(session_id)
                .or_default()
                .entry(predecessor)
                .or_default()
                .push(class);
        }
    }

    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let mut inserted = 0usize;
    for (session_id, by_predecessor) in claims {
        if by_predecessor.len() > 1 {
            return Err(DaemonError::Store(format!(
                "V114 cannot reconstruct a unique detachment predecessor for session {session_id}"
            )));
        }
        let (predecessor, classes) = by_predecessor
            .into_iter()
            .next()
            .ok_or_else(|| DaemonError::Store("V114 reconstruction lost a claim".into()))?;
        // When one session carries both classes for the same predecessor,
        // prefer the rotation edge: it is the class the manager rotation write
        // path narrows to, so recording it keeps that lock effective.
        let class = if classes.contains(&LineageReceiptClass::HarnessManagerRotationEdge) {
            LineageReceiptClass::HarnessManagerRotationEdge
        } else {
            LineageReceiptClass::CommittedReservation
        };
        verify_journal_agrees(tx, &session_id, &predecessor, class)?;
        tx.execute(
            "INSERT INTO session_lineage_detachments
               (session_id, detached_from_session_id, receipt_class, normalization_schema, recorded_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                session_id,
                predecessor,
                class.as_str(),
                V111_NORMALIZATION_SCHEMA,
                now
            ],
        )?;
        inserted += 1;
    }
    Ok(inserted)
}

/// Receipts prove the edge; the journal proves *this* normalization detached
/// it. Both are required, and any disagreement between them aborts.
///
/// Requiring the journal is what keeps the manager predicate honest. The
/// normalization writes the journal row and nulls `continued_from` in one
/// transaction, so every detachment this tree performs has one. A session whose
/// `continued_from` was nulled by anything else therefore gets no receipt, and
/// `manager_v2_cohort` keeps refusing it exactly as it does today. Tolerating a
/// missing journal would have inverted that: it would have handed the
/// alternative predicate branch to rows this code never touched.
fn verify_journal_agrees(
    tx: &Transaction<'_>,
    session_id: &str,
    predecessor: &str,
    class: LineageReceiptClass,
) -> Result<()> {
    let recorded: Option<String> = tx
        .query_row(
            "SELECT metadata FROM rotation_events
              WHERE session_id=?1 AND phase=?2 AND event_type=?3
              ORDER BY id DESC LIMIT 1",
            params![
                session_id,
                V111_NORMALIZATION_PHASE,
                V111_DETACHED_EVENT_TYPE
            ],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    let Some(metadata) = recorded else {
        return Err(DaemonError::Store(format!(
            "V114 refuses to record a lineage detachment for session {session_id}: the \
             receipts name predecessor {predecessor} but no V111 normalization journal row \
             proves this tree detached it"
        )));
    };
    let parsed: serde_json::Value = serde_json::from_str(&metadata).map_err(|error| {
        DaemonError::Store(format!(
            "V114 cannot parse the detachment journal for session {session_id}: {error}"
        ))
    })?;
    let journal_predecessor = parsed
        .get("predecessor")
        .and_then(serde_json::Value::as_str);
    if journal_predecessor != Some(predecessor) {
        return Err(DaemonError::Store(format!(
            "V114 detachment journal for session {session_id} names predecessor {} but the \
             receipts name {predecessor}",
            journal_predecessor.unwrap_or("none")
        )));
    }
    let journal_class = parsed
        .get("receipt_class")
        .and_then(serde_json::Value::as_str);
    if journal_class != Some(class.as_str()) {
        return Err(DaemonError::Store(format!(
            "V114 detachment journal for session {session_id} names receipt class {} but the \
             receipts prove {}",
            journal_class.unwrap_or("none"),
            class.as_str()
        )));
    }
    Ok(())
}
