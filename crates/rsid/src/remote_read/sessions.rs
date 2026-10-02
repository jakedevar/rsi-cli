use super::{
    BoundedText, ReadError, Result, SourcePage, bounded_column, bounded_text, source_uuid,
};
use chrono::SecondsFormat;
use chrono::{DateTime, Utc};
use rsi_common::remote_read::{CoverageStateV1, DecimalU64, SourceCoverageV1, SourceV1, Timestamp};
use rsi_common::types::SessionStatus;
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct SessionRow {
    pub id: Uuid,
    pub parent_id: Option<Uuid>,
    pub continued_from: Option<Uuid>,
    pub kind: String,
    pub provider: String,
    pub status: String,
    pub updated_at: String,
    pub own_title: BoundedText,
}

/// Already bounded runtime key copied while holding one runtime map lock.
/// The caller must release that lock before invoking the Store merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeSessionCandidate {
    pub id: Uuid,
    pub project_id: Option<Uuid>,
    pub status: SessionStatus,
    pub is_eval: bool,
    pub updated_at_seconds: i64,
    pub updated_at_nanosecond: u32,
    pub spawn_generation: Option<u64>,
}

#[derive(Debug)]
pub struct RuntimeSessionCandidateSnapshot {
    pub active: Vec<RuntimeSessionCandidate>,
    pub active_observed_at: DateTime<Utc>,
    pub completed: Vec<RuntimeSessionCandidate>,
    pub completed_observed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeSessionRecheck {
    NoObservedChange,
    Changed,
}

#[derive(Debug)]
pub struct RuntimeSessionRecheckObservation {
    pub state: RuntimeSessionRecheck,
    pub active_observed_at: DateTime<Utc>,
    pub completed_observed_at: DateTime<Utc>,
}

/// A nonblocking runtime source read. The caller performs the capture before
/// Store work and the reread after releasing the Store lock.
pub enum RuntimeSessionObservation<'a, T> {
    Observed(&'a T),
    Busy(DateTime<Utc>),
    Unavailable(DateTime<Utc>),
}

/// Derive coverage for an exact saved session from its bounded runtime scalar
/// capture and post-Store reread. `NoObservedChange` means only that these
/// scalars matched at two observations; it is not a stable snapshot. A changed
/// or failed reread cannot become an absent runtime source claim.
pub fn selected_session_coverage(
    project: Uuid,
    session: Uuid,
    capture: RuntimeSessionObservation<'_, RuntimeSessionCandidateSnapshot>,
    reread: Option<RuntimeSessionObservation<'_, RuntimeSessionRecheckObservation>>,
    store_observed_at: DateTime<Utc>,
    store_present: bool,
) -> Result<[SourceCoverageV1; 3]> {
    fn row(
        source: SourceV1,
        state: CoverageStateV1,
        count: u64,
        at: DateTime<Utc>,
        order: u32,
    ) -> Result<SourceCoverageV1> {
        Ok(SourceCoverageV1 {
            source,
            state,
            has_more: false,
            lower_bound: DecimalU64::new(count.to_string())
                .map_err(|_| ReadError::InvalidSource)?,
            observed_at: Timestamp::new(at.to_rfc3339_opts(SecondsFormat::Nanos, true))
                .map_err(|_| ReadError::InvalidSource)?,
            observation_order: order,
        })
    }
    fn exact_count(rows: &[RuntimeSessionCandidate], project: Uuid, session: Uuid) -> Result<u64> {
        if rows.windows(2).any(|pair| pair[0].id >= pair[1].id) {
            return Err(ReadError::InvalidSource);
        }
        let Ok(index) = rows.binary_search_by_key(&session, |row| row.id) else {
            return Ok(0);
        };
        let candidate = rows[index];
        if candidate.project_id != Some(project)
            || candidate.is_eval
            || matches!(
                candidate.status,
                SessionStatus::Archived | SessionStatus::Deleted
            )
        {
            return Err(ReadError::SourceUnavailable);
        }
        Ok(1)
    }
    let (active_state, active_count, active_at, completed_state, completed_count, completed_at) =
        match capture {
            RuntimeSessionObservation::Busy(at) if reread.is_none() && at <= store_observed_at => {
                (CoverageStateV1::Busy, 0, at, CoverageStateV1::Busy, 0, at)
            }
            RuntimeSessionObservation::Unavailable(at)
                if reread.is_none() && at <= store_observed_at =>
            {
                (
                    CoverageStateV1::Unavailable,
                    0,
                    at,
                    CoverageStateV1::Unavailable,
                    0,
                    at,
                )
            }
            RuntimeSessionObservation::Observed(before) => {
                if before.active.len().saturating_add(before.completed.len()) > 10_000 {
                    return Err(ReadError::ResourceLimit);
                }
                if before.active_observed_at > store_observed_at
                    || before.completed_observed_at > store_observed_at
                {
                    return Err(ReadError::InvalidSource);
                }
                let active_count = exact_count(&before.active, project, session)?;
                let completed_count = exact_count(&before.completed, project, session)?;
                match reread.ok_or(ReadError::InvalidSource)? {
                    RuntimeSessionObservation::Observed(after) => {
                        if after.active_observed_at < store_observed_at
                            || after.completed_observed_at < store_observed_at
                        {
                            return Err(ReadError::InvalidSource);
                        }
                        match after.state {
                            RuntimeSessionRecheck::NoObservedChange => (
                                CoverageStateV1::Complete,
                                active_count,
                                after.active_observed_at,
                                CoverageStateV1::Complete,
                                completed_count,
                                after.completed_observed_at,
                            ),
                            RuntimeSessionRecheck::Changed => (
                                CoverageStateV1::Unavailable,
                                0,
                                after.active_observed_at,
                                CoverageStateV1::Unavailable,
                                0,
                                after.completed_observed_at,
                            ),
                        }
                    }
                    RuntimeSessionObservation::Busy(at) => {
                        if at < store_observed_at {
                            return Err(ReadError::InvalidSource);
                        }
                        (CoverageStateV1::Busy, 0, at, CoverageStateV1::Busy, 0, at)
                    }
                    RuntimeSessionObservation::Unavailable(at) if at >= store_observed_at => (
                        CoverageStateV1::Unavailable,
                        0,
                        at,
                        CoverageStateV1::Unavailable,
                        0,
                        at,
                    ),
                    RuntimeSessionObservation::Unavailable(_) => {
                        return Err(ReadError::InvalidSource);
                    }
                }
            }
            RuntimeSessionObservation::Busy(_) | RuntimeSessionObservation::Unavailable(_) => {
                return Err(ReadError::InvalidSource);
            }
        };
    Ok([
        row(
            SourceV1::ActiveSessions,
            active_state,
            active_count,
            active_at,
            1,
        )?,
        row(
            SourceV1::CompletedSessions,
            completed_state,
            completed_count,
            completed_at,
            2,
        )?,
        row(
            SourceV1::StoreSessions,
            CoverageStateV1::Complete,
            u64::from(store_present),
            store_observed_at,
            3,
        )?,
    ])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionCandidateOrigin {
    Active,
    Completed,
    Store,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionCandidateSelection {
    pub id: Uuid,
    pub origin: SessionCandidateOrigin,
}

#[derive(Debug, Clone, Copy)]
struct StoredCandidate {
    id: Uuid,
    visible: bool,
}

#[derive(Default)]
struct MergedCandidate {
    owner: Option<Option<Uuid>>,
    active_seen: bool,
    active: bool,
    completed_seen: bool,
    completed: bool,
    store: bool,
}

fn visible(status: SessionStatus, is_eval: bool) -> bool {
    !is_eval && !matches!(status, SessionStatus::Archived | SessionStatus::Deleted)
}

/// Merge bounded active/completed key snapshots with Store candidates. The
/// caller copies at most 10,000 runtime keys total without cloning Sessions,
/// releases both runtime locks, then calls this under its Store read lifetime.
/// Hidden keys still advance the continuation. A same-ID project conflict
/// fails closed rather than choosing a misleading owner.
pub fn list_merged_session_candidates(
    conn: &Connection,
    project: Uuid,
    after: Option<Uuid>,
    limit: usize,
    active: &[RuntimeSessionCandidate],
    completed: &[RuntimeSessionCandidate],
) -> Result<SourcePage<SessionCandidateSelection, Uuid>> {
    if active.len().saturating_add(completed.len()) > 10_000 {
        return Err(ReadError::ResourceLimit);
    }
    let saved = stored_candidate_page(conn, project, after, limit)?;
    let mut merged = BTreeMap::<Uuid, MergedCandidate>::new();
    for (origin, rows) in [
        (SessionCandidateOrigin::Active, active),
        (SessionCandidateOrigin::Completed, completed),
    ] {
        for row in rows {
            if after.is_some_and(|after| row.id <= after) {
                continue;
            }
            let candidate = merged.entry(row.id).or_default();
            if candidate.owner.is_some_and(|owner| owner != row.project_id) {
                return Err(ReadError::SourceUnavailable);
            }
            candidate.owner = Some(row.project_id);
            let (seen, visible_slot) = match origin {
                SessionCandidateOrigin::Active => {
                    (&mut candidate.active_seen, &mut candidate.active)
                }
                SessionCandidateOrigin::Completed => {
                    (&mut candidate.completed_seen, &mut candidate.completed)
                }
                SessionCandidateOrigin::Store => unreachable!(),
            };
            if *seen {
                return Err(ReadError::InvalidSource);
            }
            *seen = true;
            *visible_slot = visible(row.status, row.is_eval);
        }
    }
    for row in saved.items {
        let candidate = merged.entry(row.id).or_default();
        if candidate.owner.is_some_and(|owner| owner != Some(project)) {
            return Err(ReadError::SourceUnavailable);
        }
        candidate.owner = Some(Some(project));
        candidate.store = row.visible;
    }
    let mut candidates = merged
        .into_iter()
        .filter(|(_, row)| row.owner == Some(Some(project)));
    let examined: Vec<_> = candidates.by_ref().take(limit).collect();
    let has_more = candidates.next().is_some() || saved.has_more;
    let next = if has_more {
        examined.last().map(|(id, _)| *id)
    } else {
        None
    };
    let items = examined
        .into_iter()
        .filter_map(|(id, row)| {
            let origin = if row.active {
                Some(SessionCandidateOrigin::Active)
            } else if row.completed {
                Some(SessionCandidateOrigin::Completed)
            } else if row.store {
                Some(SessionCandidateOrigin::Store)
            } else {
                None
            }?;
            Some(SessionCandidateSelection { id, origin })
        })
        .collect();
    Ok(SourcePage {
        items,
        next,
        has_more,
    })
}

/// Stored sessions ordered by UUID. Only `limit+1` keys are examined; large
/// title/query values are projected only for emitted rows. Runtime-only rows
/// and their precedence are merged by the dispatch owner.
pub fn list_sessions(
    conn: &Connection,
    project: Uuid,
    after: Option<Uuid>,
    limit: usize,
) -> Result<SourcePage<SessionRow, Uuid>> {
    let candidates = stored_candidate_page(conn, project, after, limit)?;
    let items = candidates
        .items
        .into_iter()
        .filter(|candidate| candidate.visible)
        .map(|candidate| get_session(conn, project, candidate.id))
        .collect::<Result<Vec<_>>>()?;
    Ok(SourcePage {
        items,
        next: candidates.next,
        has_more: candidates.has_more,
    })
}

fn stored_candidate_page(
    conn: &Connection,
    project: Uuid,
    after: Option<Uuid>,
    limit: usize,
) -> Result<SourcePage<StoredCandidate, Uuid>> {
    if !(1..=100).contains(&limit) {
        return Err(ReadError::ResourceLimit);
    }
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM projects WHERE id=?1)",
        [project.to_string()],
        |row| row.get(0),
    )?;
    if !exists {
        return Err(ReadError::NotFound);
    }
    // Enumerate present kinds with indexed MIN seeks. A fixed enum silently
    // omits future kinds, while a project-wide ORDER BY id needs a large sort.
    // The sixty-fifth kind is observed only to refuse an over-cap source.
    let mut kinds_statement = conn.prepare_cached(
        "WITH RECURSIVE kinds(kind) AS (
             SELECT MIN(session_kind) FROM sessions INDEXED BY manager_project_scope_candidates
              WHERE project_id=?1
             UNION ALL
             SELECT (SELECT MIN(session_kind) FROM sessions INDEXED BY manager_project_scope_candidates
                      WHERE project_id=?1 AND session_kind>kinds.kind)
              FROM kinds WHERE kind IS NOT NULL
         )
         SELECT substr(kind,1,129),length(CAST(kind AS BLOB)) FROM kinds
          WHERE kind IS NOT NULL LIMIT 65",
    )?;
    let kinds = kinds_statement
        .query_map([project.to_string()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(kinds_statement);
    if kinds.len() > 64 {
        return Err(ReadError::ResourceLimit);
    }
    if kinds.iter().any(|(kind, bytes)| {
        *bytes < 1 || *bytes > 128 || usize::try_from(*bytes).ok() != Some(kind.len())
    }) {
        return Err(ReadError::ResourceLimit);
    }
    let mut candidates = Vec::with_capacity(kinds.len() * (limit + 1));
    let mut statement = conn.prepare_cached(
        "SELECT id,status,is_eval FROM sessions INDEXED BY manager_project_scope_candidates
         WHERE project_id=?1 AND session_kind=?2 AND id>?3 ORDER BY id LIMIT ?4",
    )?;
    for (kind, _) in kinds {
        let rows = statement.query_map(
            params![
                project.to_string(),
                kind,
                after.map(|id| id.to_string()).unwrap_or_default(),
                i64::try_from(limit + 1).map_err(|_| ReadError::ResourceLimit)?
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )?;
        candidates.extend(rows.collect::<rusqlite::Result<Vec<_>>>()?);
    }
    candidates.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let has_more = candidates.len() > limit;
    let examined: Vec<_> = candidates.into_iter().take(limit).collect();
    let next = if has_more {
        examined.last().map(|row| source_uuid(&row.0)).transpose()?
    } else {
        None
    };
    let items = examined
        .into_iter()
        .map(|(raw_id, status, is_eval)| {
            Ok(StoredCandidate {
                id: source_uuid(&raw_id)?,
                visible: !matches!(status.as_str(), "Archived" | "Deleted") && is_eval == 0,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(SourcePage {
        items,
        next,
        has_more,
    })
}

/// Check the Store source beyond the merged page edge. Hidden saved rows
/// still count as candidates for source coverage and continuation.
pub(super) fn stored_has_candidate_after(
    conn: &Connection,
    project: Uuid,
    edge: Uuid,
) -> Result<bool> {
    let page = stored_candidate_page(conn, project, Some(edge), 1)?;
    Ok(!page.items.is_empty())
}

/// Exact saved-session lookup for detail reads. It uses the primary ID key
/// and never scans or paginates the project-wide session list.
pub fn get_session(conn: &Connection, project: Uuid, session: Uuid) -> Result<SessionRow> {
    let raw = conn
        .query_row(
            "SELECT rowid,parent_id,continued_from,session_kind,provider,status,updated_at,
                    title IS NOT NULL,query IS NOT NULL
             FROM sessions WHERE id=?1 AND project_id=?2
               AND status NOT IN ('Archived','Deleted') AND COALESCE(is_eval,0)=0",
            params![session.to_string(), project.to_string()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, bool>(7)?,
                    row.get::<_, bool>(8)?,
                ))
            },
        )
        .optional()?
        .ok_or(ReadError::NotFound)?;
    let title = if raw.7 {
        let title = bounded_column(conn, "sessions", "title", raw.0, 512)?;
        if !title.text.is_empty() {
            Some(title)
        } else {
            None
        }
    } else {
        None
    };
    let own_title = match title {
        Some(title) => title,
        None if raw.8 => {
            let query = bounded_column(conn, "sessions", "query", raw.0, 512)?;
            if query.text.is_empty() {
                bounded_text("Untitled session".into(), 16, 512)?
            } else {
                query
            }
        }
        None => bounded_text("Untitled session".into(), 16, 512)?,
    };
    Ok(SessionRow {
        id: session,
        parent_id: raw.1.as_deref().map(source_uuid).transpose()?,
        continued_from: raw.2.as_deref().map(source_uuid).transpose()?,
        kind: raw.3,
        provider: raw.4,
        status: raw.5,
        updated_at: raw.6,
        own_title,
    })
}

#[cfg(all(
    test,
    any(not(feature = "test-shard-mode"), feature = "test-shard-store-04")
))]
mod tests {
    use super::*;
    use chrono::Duration;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn selected_coverage_keeps_changed_runtime_sources_incomplete() {
        let project = Uuid::new_v4();
        let session = Uuid::new_v4();
        let now = Utc::now();
        let candidate = RuntimeSessionCandidate {
            id: session,
            project_id: Some(project),
            status: SessionStatus::Running,
            is_eval: false,
            updated_at_seconds: now.timestamp(),
            updated_at_nanosecond: now.timestamp_subsec_nanos(),
            spawn_generation: Some(4),
        };
        let snapshot = RuntimeSessionCandidateSnapshot {
            active: vec![candidate],
            active_observed_at: now,
            completed: vec![],
            completed_observed_at: now,
        };
        let reread_at = now + Duration::milliseconds(1);
        let matching = RuntimeSessionRecheckObservation {
            state: RuntimeSessionRecheck::NoObservedChange,
            active_observed_at: reread_at,
            completed_observed_at: reread_at,
        };
        let complete = selected_session_coverage(
            project,
            session,
            RuntimeSessionObservation::Observed(&snapshot),
            Some(RuntimeSessionObservation::Observed(&matching)),
            reread_at,
            true,
        )
        .unwrap();
        assert_eq!(complete[0].state, CoverageStateV1::Complete);
        assert_eq!(complete[0].lower_bound.get(), 1);
        assert_eq!(complete[1].state, CoverageStateV1::Complete);
        assert_eq!(complete[1].lower_bound.get(), 0);
        assert_eq!(complete[2].lower_bound.get(), 1);

        let changed = RuntimeSessionRecheckObservation {
            state: RuntimeSessionRecheck::Changed,
            ..matching
        };
        let uncertain = selected_session_coverage(
            project,
            session,
            RuntimeSessionObservation::Observed(&snapshot),
            Some(RuntimeSessionObservation::Observed(&changed)),
            reread_at,
            true,
        )
        .unwrap();
        assert_eq!(uncertain[0].state, CoverageStateV1::Unavailable);
        assert_eq!(uncertain[0].lower_bound.get(), 0);
        assert_eq!(uncertain[1].state, CoverageStateV1::Unavailable);
        assert_eq!(uncertain[2].state, CoverageStateV1::Complete);

        let busy = selected_session_coverage(
            project,
            session,
            RuntimeSessionObservation::Busy(now),
            None,
            reread_at,
            true,
        )
        .unwrap();
        assert_eq!(busy[0].state, CoverageStateV1::Busy);
        assert_eq!(busy[1].state, CoverageStateV1::Busy);
        assert_eq!(busy[0].lower_bound.get(), 0);

        assert!(matches!(
            selected_session_coverage(
                project,
                session,
                RuntimeSessionObservation::Observed(&snapshot),
                None,
                reread_at,
                true,
            ),
            Err(ReadError::InvalidSource)
        ));
        let foreign = RuntimeSessionCandidateSnapshot {
            active: vec![RuntimeSessionCandidate {
                project_id: Some(Uuid::new_v4()),
                ..candidate
            }],
            active_observed_at: now,
            completed: vec![],
            completed_observed_at: now,
        };
        assert!(matches!(
            selected_session_coverage(
                project,
                session,
                RuntimeSessionObservation::Observed(&foreign),
                Some(RuntimeSessionObservation::Observed(&matching)),
                reread_at,
                true,
            ),
            Err(ReadError::SourceUnavailable)
        ));
        let early = RuntimeSessionRecheckObservation {
            state: RuntimeSessionRecheck::NoObservedChange,
            active_observed_at: now,
            completed_observed_at: now,
        };
        assert!(matches!(
            selected_session_coverage(
                project,
                session,
                RuntimeSessionObservation::Observed(&snapshot),
                Some(RuntimeSessionObservation::Observed(&early)),
                reread_at,
                true,
            ),
            Err(ReadError::InvalidSource)
        ));
    }
}
