use super::{
    BoundedText, ReadError, Result, SourcePage, bounded_column, bounded_text,
    require_session_project, source_uuid,
};
use chrono::{DateTime, Utc};
use rsi_common::remote_read::DecisionId;
use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingSource {
    Questions,
    NativePublications,
    NativeHistorical,
    LegacyApprovals,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingKey {
    Id(Uuid),
    LegacyRowid(i64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingCandidate {
    pub source: PendingSource,
    pub id: Uuid,
    pub rowid: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeRuntimeApprovalRow {
    pub id: Uuid,
    pub incarnation_id: Uuid,
    pub spawn_generation: u64,
    pub method: Option<BoundedText>,
    pub description: Option<BoundedText>,
    pub resolution_observed: bool,
    pub resolution_persisted: bool,
    pub writer_live: bool,
    pub writer_capacity: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeRuntimeApprovalState {
    Present(NativeRuntimeApprovalRow),
    Missing,
    SourceChanged,
}

#[derive(Debug, Clone)]
pub struct NativeRuntimeApprovalSnapshot {
    pub session: Uuid,
    pub selected: Uuid,
    pub observed_at: DateTime<Utc>,
    pub state: NativeRuntimeApprovalState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeRuntimeApprovalRecheck {
    NoObservedChange,
    Changed,
}

#[derive(Debug)]
pub struct NativeRuntimeApprovalRecheckObservation {
    pub state: NativeRuntimeApprovalRecheck,
    pub observed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeRuntimeApprovalListState {
    Present(Vec<NativeRuntimeApprovalRow>),
    Missing,
    SourceChanged,
}

#[derive(Debug, Clone)]
pub struct NativeRuntimeApprovalListSnapshot {
    pub session: Uuid,
    pub observed_at: DateTime<Utc>,
    pub state: NativeRuntimeApprovalListState,
}

#[derive(Debug)]
pub struct NativeRuntimeApprovalListRecheckObservation {
    pub state: NativeRuntimeApprovalRecheck,
    pub observed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurablePendingSelection {
    pub items: Vec<PendingCandidate>,
    /// Examined keys in questions, native publications, native historical,
    /// and legacy approvals, respectively. A native mirror consumes both keys.
    pub examined: [usize; 4],
    /// The next of generic=0, native=1, legacy=2. The caller signs this with
    /// all four source positions; it is not a standalone cursor.
    pub next_bucket: u8,
    /// Only reports unconsumed keys in these supplied bounded input slices.
    /// It says nothing about later source pages or runtime observations.
    pub remaining_in_inputs: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingUnionCandidate {
    Question(PendingCandidate),
    Native {
        id: Uuid,
        runtime: bool,
        durable: Option<PendingCandidate>,
    },
    Legacy(PendingCandidate),
}

impl PendingUnionCandidate {
    pub fn id(self) -> Uuid {
        match self {
            Self::Question(candidate) | Self::Legacy(candidate) => candidate.id,
            Self::Native { id, .. } => id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingUnionSelection {
    pub items: Vec<PendingUnionCandidate>,
    /// Consumed question, runtime native, V107, V106, and legacy keys. The
    /// runtime position is absolute in the full fenced snapshot; other
    /// positions are relative to the supplied durable slices.
    pub examined: [usize; 5],
    pub next_bucket: u8,
    /// Only describes the supplied bounded slices, never later source pages.
    pub remaining_in_inputs: bool,
}

/// Schedule already captured runtime and durable keys before payload hydration.
/// The caller establishes project scope and supplies a post-Store runtime
/// reread with no observed bounded change. Even then, this is no stable
/// snapshot or full-source coverage claim. Exact native IDs from runtime,
/// V107 and V106 consume all matching keys but emit one candidate, with V107
/// as the durable preference. Source-page tails remain the caller's concern.
pub fn select_pending_union_keys(
    runtime: &NativeRuntimeApprovalListSnapshot,
    runtime_recheck: NativeRuntimeApprovalRecheck,
    runtime_offset: usize,
    questions: &[PendingCandidate],
    native_publications: &[PendingCandidate],
    native_historical: &[PendingCandidate],
    legacy: &[PendingCandidate],
    start_bucket: u8,
    limit: usize,
) -> Result<PendingUnionSelection> {
    let NativeRuntimeApprovalListState::Present(runtime_rows) = &runtime.state else {
        return Err(ReadError::SourceUnavailable);
    };
    if runtime_recheck != NativeRuntimeApprovalRecheck::NoObservedChange {
        return Err(ReadError::SourceUnavailable);
    }
    if !(1..=32).contains(&limit)
        || start_bucket > 2
        || runtime_rows.len() > 64
        || runtime_offset > runtime_rows.len()
        || runtime_rows.windows(2).any(|pair| pair[0].id >= pair[1].id)
        || runtime_rows
            .iter()
            .any(|row| !row.writer_live || row.writer_capacity > 64)
        || runtime_rows.first().is_some_and(|first| {
            runtime_rows.iter().any(|row| {
                row.incarnation_id != first.incarnation_id
                    || row.spawn_generation != first.spawn_generation
                    || row.writer_capacity != first.writer_capacity
            })
        })
        || !valid_candidates(questions, PendingSource::Questions)
        || !valid_candidates(native_publications, PendingSource::NativePublications)
        || !valid_candidates(native_historical, PendingSource::NativeHistorical)
        || !valid_candidates(legacy, PendingSource::LegacyApprovals)
    {
        return Err(ReadError::InvalidSource);
    }
    let mut examined = [0, runtime_offset, 0, 0, 0];
    let mut next_bucket = start_bucket;
    let mut items = Vec::with_capacity(limit);
    while items.len() < limit {
        let mut chosen = None;
        for _ in 0..3 {
            chosen = match next_bucket {
                0 if examined[0] < questions.len() => {
                    let candidate = questions[examined[0]];
                    examined[0] += 1;
                    Some(PendingUnionCandidate::Question(candidate))
                }
                1 if examined[1] < runtime_rows.len()
                    || examined[2] < native_publications.len()
                    || examined[3] < native_historical.len() =>
                {
                    let id = [
                        runtime_rows.get(examined[1]).map(|row| row.id),
                        native_publications.get(examined[2]).map(|row| row.id),
                        native_historical.get(examined[3]).map(|row| row.id),
                    ]
                    .into_iter()
                    .flatten()
                    .min()
                    .ok_or(ReadError::InvalidSource)?;
                    let runtime_present = runtime_rows
                        .get(examined[1])
                        .is_some_and(|row| row.id == id);
                    let publication = native_publications
                        .get(examined[2])
                        .copied()
                        .filter(|row| row.id == id);
                    let historical = native_historical
                        .get(examined[3])
                        .copied()
                        .filter(|row| row.id == id);
                    examined[1] += usize::from(runtime_present);
                    examined[2] += usize::from(publication.is_some());
                    examined[3] += usize::from(historical.is_some());
                    Some(PendingUnionCandidate::Native {
                        id,
                        runtime: runtime_present,
                        durable: publication.or(historical),
                    })
                }
                2 if examined[4] < legacy.len() => {
                    let candidate = legacy[examined[4]];
                    examined[4] += 1;
                    Some(PendingUnionCandidate::Legacy(candidate))
                }
                _ => None,
            };
            next_bucket = (next_bucket + 1) % 3;
            if chosen.is_some() {
                break;
            }
        }
        let Some(item) = chosen else { break };
        items.push(item);
    }
    Ok(PendingUnionSelection {
        items,
        examined,
        next_bucket,
        remaining_in_inputs: examined[0] < questions.len()
            || examined[1] < runtime_rows.len()
            || examined[2] < native_publications.len()
            || examined[3] < native_historical.len()
            || examined[4] < legacy.len(),
    })
}

pub(super) fn valid_candidates(items: &[PendingCandidate], source: PendingSource) -> bool {
    items.len() <= 33
        && items
            .iter()
            .all(|item| item.source == source && item.rowid > 0)
        && items.windows(2).all(|pair| {
            if source == PendingSource::LegacyApprovals {
                pair[0].rowid < pair[1].rowid
            } else {
                pair[0].id < pair[1].id
            }
        })
}

/// Round-robin over bounded durable keys before hydrating payloads. This
/// deliberately excludes runtime approvals and question slots; the caller
/// must union those separately and keep coverage incomplete until observed.
/// Native historical rows with an exact V107 publication mirror consume both
/// keys but emit only V107, preserving the source's fallback precedence.
pub fn select_durable_pending_keys(
    questions: &[PendingCandidate],
    native_publications: &[PendingCandidate],
    native_historical: &[PendingCandidate],
    legacy: &[PendingCandidate],
    start_bucket: u8,
    limit: usize,
) -> Result<DurablePendingSelection> {
    if !(1..=32).contains(&limit)
        || start_bucket > 2
        || !valid_candidates(questions, PendingSource::Questions)
        || !valid_candidates(native_publications, PendingSource::NativePublications)
        || !valid_candidates(native_historical, PendingSource::NativeHistorical)
        || !valid_candidates(legacy, PendingSource::LegacyApprovals)
    {
        return Err(ReadError::InvalidSource);
    }
    let mut examined = [0; 4];
    let mut next_bucket = start_bucket;
    let mut items = Vec::with_capacity(limit);
    while items.len() < limit {
        let mut chosen = None;
        for _ in 0..3 {
            chosen = match next_bucket {
                0 if examined[0] < questions.len() => {
                    let item = questions[examined[0]];
                    examined[0] += 1;
                    Some(item)
                }
                1 if examined[1] < native_publications.len()
                    || examined[2] < native_historical.len() =>
                {
                    let publication = native_publications.get(examined[1]);
                    let historical = native_historical.get(examined[2]);
                    match (publication, historical) {
                        (Some(p), Some(h)) if p.id == h.id => {
                            examined[1] += 1;
                            examined[2] += 1;
                            Some(*p)
                        }
                        (Some(p), Some(h)) if p.id < h.id => {
                            examined[1] += 1;
                            Some(*p)
                        }
                        (Some(_), Some(h)) => {
                            examined[2] += 1;
                            Some(*h)
                        }
                        (Some(p), None) => {
                            examined[1] += 1;
                            Some(*p)
                        }
                        (None, Some(h)) => {
                            examined[2] += 1;
                            Some(*h)
                        }
                        (None, None) => None,
                    }
                }
                2 if examined[3] < legacy.len() => {
                    let item = legacy[examined[3]];
                    examined[3] += 1;
                    Some(item)
                }
                _ => None,
            };
            next_bucket = (next_bucket + 1) % 3;
            if chosen.is_some() {
                break;
            }
        }
        let Some(item) = chosen else { break };
        items.push(item);
    }
    Ok(DurablePendingSelection {
        items,
        examined,
        next_bucket,
        remaining_in_inputs: examined[0] < questions.len()
            || examined[1] < native_publications.len()
            || examined[2] < native_historical.len()
            || examined[3] < legacy.len(),
    })
}

#[derive(Debug, Clone)]
pub struct PendingSourceRow {
    pub id: Uuid,
    pub state: String,
    pub closure_state: Option<String>,
    pub incarnation_id: Option<Uuid>,
    pub epoch: Option<u64>,
    pub method: Option<BoundedText>,
    pub description: Option<BoundedText>,
    pub tool_name: Option<BoundedText>,
    /// Complete bounded JSON only. The RPC owner parses this into the closed
    /// question projection; an unreadable carrier remains a visible card.
    pub question_json: Option<String>,
    pub details_unavailable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestionSlotMirror {
    Tracked,
    Session,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestionSlotGeneration {
    Spawn(u64),
    Completed,
}

#[derive(Debug, Clone)]
pub enum SelectedPendingSource {
    Durable {
        source: PendingSource,
        row: PendingSourceRow,
    },
    QuestionFallback(PendingSourceRow),
    RuntimeSlot {
        generation: QuestionSlotGeneration,
        mirror: QuestionSlotMirror,
    },
    Missing,
}

/// Resolve a validated, kind-qualified selection to its exact source. Runtime
/// slots are returned as an identity fence for the caller's bounded runtime
/// observation; absence is not proof of a tombstone until coverage completes.
pub fn selected_pending_source(
    conn: &Connection,
    project: Uuid,
    session: Uuid,
    selected: &DecisionId,
) -> Result<SelectedPendingSource> {
    require_session_project(conn, project, session)?;
    let parts: Vec<_> = selected.as_str().split(':').collect();
    match parts.as_slice() {
        ["question", raw] => {
            let id = source_uuid(raw)?;
            Ok(
                pending_source_exact(conn, project, session, PendingSource::Questions, id)?
                    .map(|row| SelectedPendingSource::Durable {
                        source: PendingSource::Questions,
                        row,
                    })
                    .unwrap_or(SelectedPendingSource::Missing),
            )
        }
        ["native", raw] => {
            let id = source_uuid(raw)?;
            for source in [
                PendingSource::NativePublications,
                PendingSource::NativeHistorical,
            ] {
                if let Some(row) = pending_source_exact(conn, project, session, source, id)? {
                    return Ok(SelectedPendingSource::Durable { source, row });
                }
            }
            Ok(SelectedPendingSource::Missing)
        }
        ["legacy", raw] => {
            let id = source_uuid(raw)?;
            Ok(
                pending_source_exact(conn, project, session, PendingSource::LegacyApprovals, id)?
                    .map(|row| SelectedPendingSource::Durable {
                        source: PendingSource::LegacyApprovals,
                        row,
                    })
                    .unwrap_or(SelectedPendingSource::Missing),
            )
        }
        ["question-fallback", raw] if source_uuid(raw)? == session => {
            Ok(durable_question_fallback(conn, project, session)?
                .map(SelectedPendingSource::QuestionFallback)
                .unwrap_or(SelectedPendingSource::Missing))
        }
        ["question-slot", raw, generation, mirror] if source_uuid(raw)? == session => {
            let generation = if *generation == "completed" {
                QuestionSlotGeneration::Completed
            } else {
                QuestionSlotGeneration::Spawn(
                    generation.parse().map_err(|_| ReadError::InvalidSource)?,
                )
            };
            let mirror = match *mirror {
                "tracked" => QuestionSlotMirror::Tracked,
                "session" => QuestionSlotMirror::Session,
                _ => return Err(ReadError::InvalidSource),
            };
            Ok(SelectedPendingSource::RuntimeSlot { generation, mirror })
        }
        ["question-fallback", _] | ["question-slot", ..] => Err(ReadError::NotFound),
        _ => Err(ReadError::InvalidSource),
    }
}

/// Scan only per-source keys before union selection. Each seek examines at
/// most `limit+1` candidate keys without reading question/target/tool payloads.
/// The caller holds its read snapshot while selecting and hydrating rows.
pub fn pending_source_key_page(
    conn: &Connection,
    project: Uuid,
    session: Uuid,
    source: PendingSource,
    after: Option<PendingKey>,
    limit: usize,
) -> Result<SourcePage<PendingCandidate, PendingKey>> {
    if !(1..=32).contains(&limit) {
        return Err(ReadError::ResourceLimit);
    }
    require_session_project(conn, project, session)?;
    let after_param = match (source, after) {
        (PendingSource::LegacyApprovals, Some(PendingKey::LegacyRowid(id))) if id > 0 => {
            rusqlite::types::Value::Integer(id)
        }
        (PendingSource::LegacyApprovals, None) => rusqlite::types::Value::Integer(0),
        (PendingSource::LegacyApprovals, _) => return Err(ReadError::InvalidSource),
        (_, Some(PendingKey::Id(id))) => rusqlite::types::Value::Text(id.to_string()),
        (_, None) => rusqlite::types::Value::Text(String::new()),
        (_, _) => return Err(ReadError::InvalidSource),
    };
    let sql = match source {
        PendingSource::Questions => {
            "SELECT publication_id,rowid FROM pending_question_publications WHERE session_id=?1 AND publication_id>?2 ORDER BY publication_id LIMIT ?3"
        }
        PendingSource::NativePublications => {
            "SELECT publication_id,rowid FROM appserver_approval_publications INDEXED BY appserver_approval_publications_session WHERE session_id=?1 AND publication_id>?2 ORDER BY publication_id LIMIT ?3"
        }
        PendingSource::NativeHistorical => {
            "SELECT publication_id,rowid FROM pending_appserver_approvals WHERE session_id=?1 AND publication_id>?2 ORDER BY publication_id LIMIT ?3"
        }
        PendingSource::LegacyApprovals => {
            "SELECT id,rowid FROM approvals INDEXED BY idx_approvals_session_id WHERE session_id=?1 AND rowid>?2 ORDER BY rowid LIMIT ?3"
        }
    };
    let mut statement = conn.prepare_cached(sql)?;
    let candidates = statement
        .query_map(
            params![session.to_string(), after_param, (limit + 1) as i64],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let has_more = candidates.len() > limit;
    let examined: Vec<_> = candidates.into_iter().take(limit).collect();
    let next = if has_more {
        examined
            .last()
            .map(|(id, rowid)| match source {
                PendingSource::LegacyApprovals => Ok(PendingKey::LegacyRowid(*rowid)),
                _ => source_uuid(id).map(PendingKey::Id),
            })
            .transpose()?
    } else {
        None
    };
    let items = examined
        .into_iter()
        .map(|(raw_id, rowid)| {
            Ok(PendingCandidate {
                source,
                id: source_uuid(&raw_id)?,
                rowid,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(SourcePage {
        items,
        next,
        has_more,
    })
}

/// Compatibility source page. The union owner instead uses the key scan and
/// hydrates only chosen candidates after round-robin selection.
pub fn pending_source_page(
    conn: &Connection,
    project: Uuid,
    session: Uuid,
    source: PendingSource,
    after: Option<PendingKey>,
    limit: usize,
    retained: bool,
) -> Result<SourcePage<PendingSourceRow, PendingKey>> {
    let keys = pending_source_key_page(conn, project, session, source, after, limit)?;
    let mut items = Vec::with_capacity(keys.items.len());
    for candidate in keys.items {
        if let Some(row) = pending_candidate_hydrate(conn, project, session, candidate, retained)? {
            items.push(row);
        }
    }
    Ok(SourcePage {
        items,
        next: keys.next,
        has_more: keys.has_more,
    })
}

/// Hydrate one chosen key only. Recheck its id/session under the caller's read
/// snapshot so a stale rowid cannot copy another occurrence's payload.
pub fn pending_candidate_hydrate(
    conn: &Connection,
    project: Uuid,
    session: Uuid,
    candidate: PendingCandidate,
    retained: bool,
) -> Result<Option<PendingSourceRow>> {
    require_session_project(conn, project, session)?;
    let source = candidate.source;
    let sql = match source {
        PendingSource::Questions => {
            "SELECT publication_id,session_id FROM pending_question_publications WHERE rowid=?1"
        }
        PendingSource::NativePublications => {
            "SELECT publication_id,session_id FROM appserver_approval_publications WHERE rowid=?1"
        }
        PendingSource::NativeHistorical => {
            "SELECT publication_id,session_id FROM pending_appserver_approvals WHERE rowid=?1"
        }
        PendingSource::LegacyApprovals => "SELECT id,session_id FROM approvals WHERE rowid=?1",
    };
    let identity: Option<(String, String)> = conn
        .query_row(sql, [candidate.rowid], |row| Ok((row.get(0)?, row.get(1)?)))
        .optional()?;
    let Some((id, owner)) = identity else {
        return Err(ReadError::SourceUnavailable);
    };
    if source_uuid(&id)? != candidate.id || owner != session.to_string() {
        return Err(ReadError::SourceUnavailable);
    }
    project_candidate(
        conn,
        session,
        source,
        candidate.id.to_string(),
        candidate.rowid,
        retained,
    )
}

/// Reread one durable decision by its complete occurrence ID regardless of
/// page position or retained/attention filter. The caller handles runtime
/// slots, source coverage and the selected tombstone when this returns None.
pub fn pending_source_exact(
    conn: &Connection,
    project: Uuid,
    session: Uuid,
    source: PendingSource,
    id: Uuid,
) -> Result<Option<PendingSourceRow>> {
    require_session_project(conn, project, session)?;
    let sql = match source {
        PendingSource::Questions => {
            "SELECT rowid FROM pending_question_publications WHERE publication_id=?1 AND session_id=?2"
        }
        PendingSource::NativePublications => {
            "SELECT rowid FROM appserver_approval_publications WHERE publication_id=?1 AND session_id=?2"
        }
        PendingSource::NativeHistorical => {
            "SELECT rowid FROM pending_appserver_approvals WHERE publication_id=?1 AND session_id=?2"
        }
        PendingSource::LegacyApprovals => {
            "SELECT rowid FROM approvals WHERE id=?1 AND session_id=?2"
        }
    };
    let raw = id.to_string();
    let candidate: Option<i64> = conn
        .query_row(sql, params![raw, session.to_string()], |row| row.get(0))
        .optional()?;
    let Some(rowid) = candidate else {
        return Ok(None);
    };
    project_candidate(conn, session, source, raw, rowid, true)
}

fn bounded_state(
    conn: &Connection,
    table: &'static str,
    column: &'static str,
    rowid: i64,
) -> Result<String> {
    let value = bounded_column(conn, table, column, rowid, 128)?;
    if value.truncated || value.text.is_empty() {
        return Err(ReadError::InvalidSource);
    }
    Ok(value.text)
}

fn project_candidate(
    conn: &Connection,
    session: Uuid,
    source: PendingSource,
    raw_id: String,
    rowid: i64,
    retained: bool,
) -> Result<Option<PendingSourceRow>> {
    let (table, state_column) = match source {
        PendingSource::Questions => ("pending_question_publications", "state"),
        PendingSource::NativePublications => ("appserver_approval_publications", "state"),
        PendingSource::NativeHistorical => ("pending_appserver_approvals", "state"),
        PendingSource::LegacyApprovals => ("approvals", "status"),
    };
    let state = bounded_state(conn, table, state_column, rowid)?;
    let closure = if source == PendingSource::NativePublications {
        let present: bool = conn.query_row(
            "SELECT closure_state IS NOT NULL FROM appserver_approval_publications WHERE rowid=?1",
            [rowid],
            |row| row.get(0),
        )?;
        present
            .then(|| bounded_state(conn, table, "closure_state", rowid))
            .transpose()?
    } else {
        None
    };
    if !retained
        && match source {
            PendingSource::Questions => state == "cleared",
            PendingSource::NativePublications => {
                closure.as_deref() == Some("closed") || state == "superseded"
            }
            PendingSource::NativeHistorical => false,
            PendingSource::LegacyApprovals => state != "Pending",
        }
    {
        return Ok(None);
    }
    if matches!(
        source,
        PendingSource::NativeHistorical | PendingSource::LegacyApprovals
    ) {
        let native: bool = conn.query_row(
                if source == PendingSource::NativeHistorical {
                    "SELECT EXISTS(SELECT 1 FROM appserver_approval_publications WHERE publication_id=?1 AND session_id=?2)"
                } else {
                    "SELECT EXISTS(SELECT 1 FROM appserver_approval_publications WHERE approval_id=?1 AND session_id=?2) OR EXISTS(SELECT 1 FROM pending_appserver_approvals WHERE approval_id=?1 AND session_id=?2)"
                },
                params![raw_id, session.to_string()], |row| row.get(0),
            )?;
        if native {
            return Ok(None);
        }
    }
    let id = source_uuid(&raw_id)?;
    let mut projected = PendingSourceRow {
        id,
        state: String::new(),
        closure_state: None,
        incarnation_id: None,
        epoch: None,
        method: None,
        description: None,
        tool_name: None,
        question_json: None,
        details_unavailable: false,
    };
    match source {
        PendingSource::Questions => {
            let (epoch, has_json): (i64, bool) = conn.query_row(
                    "SELECT epoch,question_json IS NOT NULL FROM pending_question_publications WHERE publication_id=?1 AND session_id=?2",
                    params![raw_id, session.to_string()],
                    |r| Ok((r.get(0)?,r.get(1)?)),
                )?;
            projected.state = state;
            projected.epoch = Some(u64::try_from(epoch).map_err(|_| ReadError::InvalidSource)?);
            projected.question_json = (if has_json {
                read_json_carrier(
                    conn,
                    "pending_question_publications",
                    "question_json",
                    rowid,
                )?
            } else {
                None
            })
            .filter(|raw| {
                parse_json_limited(raw).is_some_and(|value| {
                    serde_json::from_value::<rsi_common::types::PendingQuestion>(value)
                        .is_ok_and(|question| !question.questions.is_empty())
                })
            });
            projected.details_unavailable = projected.question_json.is_none();
        }
        PendingSource::NativePublications | PendingSource::NativeHistorical => {
            let table = if source == PendingSource::NativePublications {
                "appserver_approval_publications"
            } else {
                "pending_appserver_approvals"
            };
            // The table name is selected only from the enum above.
            let sql = format!(
                "SELECT incarnation_id,target_json IS NOT NULL FROM {table} WHERE publication_id=?1 AND session_id=?2"
            );
            let (incarnation, has_target): (String, bool) =
                conn.query_row(&sql, params![raw_id, session.to_string()], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?;
            projected.state = state;
            projected.closure_state = closure;
            projected.incarnation_id = Some(source_uuid(&incarnation)?);
            let target = if has_target {
                read_json_carrier(conn, table, "target_json", rowid)?
            } else {
                None
            };
            let display = target.as_deref().and_then(parse_json_limited);
            projected.details_unavailable = display.is_none();
            if let Some(value) = display {
                projected.method = native_field(value.get("method"), 512)?;
                projected.description = native_field(value.get("description"), 2048)?;
            }
        }
        PendingSource::LegacyApprovals => {
            let has_name: bool = conn.query_row(
                "SELECT tool_name IS NOT NULL FROM approvals WHERE id=?1 AND session_id=?2",
                params![raw_id, session.to_string()],
                |r| r.get(0),
            )?;
            projected.state = state;
            projected.tool_name = if has_name {
                optional_display(conn, "approvals", "tool_name", rowid, 512)?
            } else {
                None
            };
            projected.details_unavailable = projected.tool_name.is_none();
        }
    }
    Ok(Some(projected))
}

fn native_field(value: Option<&serde_json::Value>, cap: usize) -> Result<Option<BoundedText>> {
    let Some(text) = value
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return Ok(None);
    };
    Ok(Some(bounded_text(text.to_owned(), text.len() as i64, cap)?))
}

fn read_json_carrier(
    conn: &Connection,
    table: &'static str,
    column: &'static str,
    rowid: i64,
) -> Result<Option<String>> {
    match bounded_column(conn, table, column, rowid, 65_536) {
        Ok(value) if !value.truncated => Ok(Some(value.text)),
        Ok(_) | Err(ReadError::InvalidSource) => Ok(None),
        Err(error) => Err(error),
    }
}

fn optional_display(
    conn: &Connection,
    table: &'static str,
    column: &'static str,
    rowid: i64,
    cap: usize,
) -> Result<Option<BoundedText>> {
    match bounded_column(conn, table, column, rowid, cap) {
        Ok(value) if !value.text.is_empty() => Ok(Some(value)),
        Ok(_) | Err(ReadError::InvalidSource) => Ok(None),
        Err(error) => Err(error),
    }
}

pub(super) fn parse_json_limited(raw: &str) -> Option<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let mut stack = vec![(&value, 0_usize)];
    let mut nodes = 0;
    while let Some((node, depth)) = stack.pop() {
        nodes += 1;
        if depth > 16 || nodes > 256 {
            return None;
        }
        match node {
            serde_json::Value::Array(items) => stack.extend(items.iter().map(|v| (v, depth + 1))),
            serde_json::Value::Object(items) => {
                stack.extend(items.values().map(|v| (v, depth + 1)))
            }
            _ => {}
        }
    }
    Some(value)
}

/// A durable question slot has no occurrence identity. Return its bounded
/// carrier even when a publication exists, so a disagreeing mirror remains
/// visible instead of silently replacing the publication's identity.
pub fn durable_question_fallback(
    conn: &Connection,
    project: Uuid,
    session: Uuid,
) -> Result<Option<PendingSourceRow>> {
    require_session_project(conn, project, session)?;
    let (rowid, has_json): (i64, bool) = conn.query_row(
        "SELECT rowid,pending_question_json IS NOT NULL FROM sessions WHERE id=?1 AND project_id=?2",
        params![session.to_string(), project.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if !has_json {
        return Ok(None);
    }
    let question_json = read_json_carrier(conn, "sessions", "pending_question_json", rowid)?
        .filter(|raw| {
            parse_json_limited(raw).is_some_and(|value| {
                serde_json::from_value::<rsi_common::types::PendingQuestion>(value)
                    .is_ok_and(|question| !question.questions.is_empty())
            })
        });
    Ok(Some(PendingSourceRow {
        id: session,
        state: "slot".into(),
        closure_state: None,
        incarnation_id: None,
        epoch: None,
        method: None,
        description: None,
        tool_name: None,
        details_unavailable: question_json.is_none(),
        question_json,
    }))
}
