use super::{
    HistoryObservedPage, ProjectRow, ReadError, Result, SourcePage, history_event, project,
    tool_pair_ambiguous,
};
use chrono::{DateTime, SecondsFormat, Utc};
use rsi_common::remote_read::{
    CoverageStateV1, CursorV1, DecimalU64, DegradationV1, EventKeyV1, HistoryEventV1,
    HistoryIntervalV1, HistoryPositionV1, HistoryResponseV1, HistoryWindowV1, InfoResponseV1,
    InfoV1, ProjectV1, ProjectionLimitsV1, ProjectsResponseV1, REQUIRED_CAPABILITIES,
    ReadResponseV1, RemoteGetHistoryPageV1, RemoteGetSessionV1, RemoteListSessionsV1,
    SessionDetailV1, SessionResponseV1, SessionSummaryV1, SessionsResponseV1, SourceCoverageV1,
    SourceV1, Text, Timestamp, WireDocumentV1, WireUuid,
};
use rusqlite::Connection;
use uuid::Uuid;

/// Assemble the fixed Remote v1 contract discovery response.
pub fn info_response(daemon_epoch: Uuid, observed_at: DateTime<Utc>) -> Result<InfoResponseV1> {
    let epoch = WireUuid::new(daemon_epoch.to_string()).map_err(|_| ReadError::InvalidSource)?;
    let response = InfoResponseV1 {
        version: Text::<3>::new(rsi_common::remote_read::VERSION.into())
            .map_err(|_| ReadError::InvalidSource)?,
        daemon_epoch: epoch.clone(),
        observed_at: Timestamp::new(observed_at.to_rfc3339_opts(SecondsFormat::Nanos, true))
            .map_err(|_| ReadError::InvalidSource)?,
        next_cursor: None,
        complete: true,
        projection_limits: ProjectionLimitsV1 {
            page_items: 1,
            name_bytes: 512,
            event_text_bytes: 8192,
            decision_text_bytes: 8192,
            item_bytes: 65_536,
            envelope_bytes: 524_288,
        },
        degraded: Vec::new(),
        coverage: Vec::new(),
        item: InfoV1 {
            protocol: Text::<3>::new(rsi_common::remote_read::VERSION.into())
                .map_err(|_| ReadError::InvalidSource)?,
            daemon_boot_id: epoch,
            required_capabilities: REQUIRED_CAPABILITIES.to_vec(),
        },
    };
    rsi_common::remote_read::encode(&WireDocumentV1::Response(ReadResponseV1::RemoteGetInfoV1(
        response.clone(),
    )))
    .map_err(|_| ReadError::InvalidSource)?;
    Ok(response)
}

/// Build one fully observed project page. `configured` is the authorized ID
/// set; the Store reader must already have scanned the same bounded slice.
/// A missing Store row still consumes its configured ID and may leave an
/// empty visible page with a valid continuation. The RPC owner signs `cursor`.
pub fn projects_response(
    configured: &[Uuid],
    after: Option<Uuid>,
    limit: usize,
    page: SourcePage<ProjectRow, Uuid>,
    daemon_epoch: Uuid,
    observed_at: DateTime<Utc>,
    cursor: Option<CursorV1>,
) -> Result<ProjectsResponseV1> {
    if configured.len() > 32 || !(1..=50).contains(&limit) {
        return Err(ReadError::ResourceLimit);
    }
    let mut ids = configured.to_vec();
    ids.sort_unstable();
    if ids.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(ReadError::InvalidSource);
    }
    let remaining: Vec<_> = ids
        .into_iter()
        .filter(|id| after.is_none_or(|after| *id > after))
        .collect();
    let examined = &remaining[..remaining.len().min(limit)];
    let has_more = remaining.len() > limit;
    if page.has_more != has_more
        || page.next != has_more.then(|| *examined.last().expect("limit is positive"))
        || cursor.is_some() != has_more
        || cursor
            .as_ref()
            .is_some_and(|cursor| !matches!(cursor, CursorV1::Projects { .. }))
    {
        return Err(ReadError::InvalidSource);
    }
    let mut prior = None;
    for row in &page.items {
        if examined.binary_search(&row.id).is_err() || prior.is_some_and(|id| id >= row.id) {
            return Err(ReadError::InvalidSource);
        }
        prior = Some(row.id);
    }
    let timestamp = Timestamp::new(observed_at.to_rfc3339_opts(SecondsFormat::Nanos, true))
        .map_err(|_| ReadError::InvalidSource)?;
    let coverage_row = |source, lower_bound: usize, order, has_more| -> Result<_> {
        Ok(SourceCoverageV1 {
            source,
            state: CoverageStateV1::Complete,
            has_more,
            lower_bound: DecimalU64::new(lower_bound.to_string())
                .map_err(|_| ReadError::InvalidSource)?,
            observed_at: timestamp.clone(),
            observation_order: order,
        })
    };
    let mut coverage = vec![coverage_row(
        SourceV1::ConfiguredProjects,
        examined.len(),
        1,
        has_more,
    )?];
    if !configured.is_empty() {
        // Store rows in future configured slices are unknown, so this row
        // counts only the visible rows observed in the current slice.
        coverage.push(coverage_row(
            SourceV1::StoreProjects,
            page.items.len(),
            2,
            false,
        )?);
    }
    let items = page
        .items
        .into_iter()
        .map(project)
        .collect::<Result<Vec<_>>>()?;
    let response = ProjectsResponseV1 {
        version: Text::<3>::new(rsi_common::remote_read::VERSION.into())
            .map_err(|_| ReadError::InvalidSource)?,
        daemon_epoch: WireUuid::new(daemon_epoch.to_string())
            .map_err(|_| ReadError::InvalidSource)?,
        observed_at: timestamp,
        next_cursor: cursor,
        complete: true,
        projection_limits: ProjectionLimitsV1 {
            page_items: limit as u32,
            name_bytes: 512,
            event_text_bytes: 8192,
            decision_text_bytes: 8192,
            item_bytes: 65_536,
            envelope_bytes: 524_288,
        },
        degraded: Vec::new(),
        coverage,
        items,
    };
    rsi_common::remote_read::encode(&WireDocumentV1::Response(
        ReadResponseV1::RemoteListProjectsV1(response.clone()),
    ))
    .map_err(|_| ReadError::InvalidSource)?;
    Ok(response)
}

/// Assemble one merged active/completed/Store keyset page. `next` is the last
/// examined UUID, which may be hidden from `items`; it must advance even for
/// an empty visible page. Owners refuse a changed or busy runtime reread
/// before using this complete-source assembler.
pub struct SessionsResponseSources {
    pub project: ProjectV1,
    pub after: Option<Uuid>,
    pub page: SourcePage<SessionSummaryV1, Uuid>,
    pub coverage: [SourceCoverageV1; 3],
    pub cursor: Option<CursorV1>,
}

pub fn sessions_response(
    request: &RemoteListSessionsV1,
    sources: SessionsResponseSources,
    daemon_epoch: Uuid,
    observed_at: DateTime<Utc>,
) -> Result<SessionsResponseV1> {
    let SessionsResponseSources {
        project,
        after,
        page,
        coverage,
        cursor,
    } = sources;
    let item_ids = page
        .items
        .iter()
        .map(|item| Uuid::parse_str(item.id.as_str()).map_err(|_| ReadError::InvalidSource))
        .collect::<Result<Vec<_>>>()?;
    if project.id != request.project_id
        || !(1..=100).contains(&request.limit)
        || page.items.len() > request.limit as usize
        || page.has_more != page.next.is_some()
        || page.has_more != cursor.is_some()
        || cursor
            .as_ref()
            .is_some_and(|cursor| !matches!(cursor, CursorV1::Sessions { .. }))
        || page
            .next
            .is_some_and(|next| after.is_some_and(|after| next <= after))
        || page.items.iter().any(|item| item.project_id != project.id)
        || item_ids.iter().any(|id| {
            after.is_some_and(|after| *id <= after) || page.next.is_some_and(|next| *id > next)
        })
        || item_ids.windows(2).any(|pair| pair[0] >= pair[1])
    {
        return Err(ReadError::InvalidSource);
    }
    for (row, (source, order)) in coverage.iter().zip([
        (SourceV1::ActiveSessions, 1_u32),
        (SourceV1::CompletedSessions, 2_u32),
        (SourceV1::StoreSessions, 3_u32),
    ]) {
        let at = DateTime::parse_from_rfc3339(row.observed_at.as_str())
            .map_err(|_| ReadError::InvalidSource)?
            .with_timezone(&Utc);
        if row.source != source
            || row.observation_order != order
            || row.state != CoverageStateV1::Complete
            || at > observed_at
        {
            return Err(ReadError::InvalidSource);
        }
    }
    let response = SessionsResponseV1 {
        version: Text::<3>::new(rsi_common::remote_read::VERSION.into())
            .map_err(|_| ReadError::InvalidSource)?,
        daemon_epoch: WireUuid::new(daemon_epoch.to_string())
            .map_err(|_| ReadError::InvalidSource)?,
        observed_at: Timestamp::new(observed_at.to_rfc3339_opts(SecondsFormat::Nanos, true))
            .map_err(|_| ReadError::InvalidSource)?,
        next_cursor: cursor,
        complete: true,
        projection_limits: ProjectionLimitsV1 {
            page_items: request.limit,
            name_bytes: 512,
            event_text_bytes: 8192,
            decision_text_bytes: 8192,
            item_bytes: 65_536,
            envelope_bytes: 524_288,
        },
        degraded: Vec::new(),
        coverage: Vec::from(coverage),
        project,
        items: page.items,
    };
    rsi_common::remote_read::encode(&WireDocumentV1::Response(
        ReadResponseV1::RemoteListSessionsV1(response.clone()),
    ))
    .map_err(|_| ReadError::InvalidSource)?;
    Ok(response)
}

/// Assemble an exact selected saved session from separately acquired source
/// observations. The caller must retain its Store read context while building
/// `item`, and may mark runtime sources complete only after their generation
/// reread reports no observed scalar change. This does not establish a frozen
/// runtime/Store snapshot or solve project-wide unknown-kind discovery.
pub fn selected_session_response(
    request: &RemoteGetSessionV1,
    item: SessionDetailV1,
    coverage: [SourceCoverageV1; 3],
    daemon_epoch: Uuid,
    observed_at: DateTime<Utc>,
) -> Result<SessionResponseV1> {
    if item.summary.project_id != request.project_id || item.summary.id != request.session_id {
        return Err(ReadError::InvalidSource);
    }
    for (index, (row, source)) in coverage
        .iter()
        .zip([
            SourceV1::ActiveSessions,
            SourceV1::CompletedSessions,
            SourceV1::StoreSessions,
        ])
        .enumerate()
    {
        if row.source != source
            || row.observation_order != (index + 1) as u32
            || row.has_more
            || row.lower_bound.get() > 1
            || (row.state != CoverageStateV1::Complete && row.lower_bound.get() != 0)
        {
            return Err(ReadError::InvalidSource);
        }
    }
    if coverage[2].state != CoverageStateV1::Complete
        || (coverage[2].lower_bound.get() == 0
            && coverage[..2]
                .iter()
                .all(|row| row.state != CoverageStateV1::Complete || row.lower_bound.get() != 1))
    {
        return Err(ReadError::InvalidSource);
    }
    let pending_incomplete = item
        .pending_coverage
        .iter()
        .any(|row| row.state != CoverageStateV1::Complete || row.has_more);
    if item.summary.attention.incomplete != pending_incomplete {
        return Err(ReadError::InvalidSource);
    }
    let mut degraded = Vec::new();
    for row in coverage.iter().chain(&item.pending_coverage) {
        let reason = match row.state {
            CoverageStateV1::Busy => Some(DegradationV1::Busy),
            CoverageStateV1::Limited => Some(DegradationV1::Limited),
            CoverageStateV1::Unavailable => Some(DegradationV1::Unavailable),
            CoverageStateV1::Complete if row.has_more => Some(DegradationV1::Limited),
            CoverageStateV1::Complete => None,
        };
        if let Some(reason) = reason {
            if !degraded.contains(&reason) {
                degraded.push(reason);
            }
        }
    }
    let timestamp = Timestamp::new(observed_at.to_rfc3339_opts(SecondsFormat::Nanos, true))
        .map_err(|_| ReadError::InvalidSource)?;
    let response = SessionResponseV1 {
        version: Text::<3>::new(rsi_common::remote_read::VERSION.into())
            .map_err(|_| ReadError::InvalidSource)?,
        daemon_epoch: WireUuid::new(daemon_epoch.to_string())
            .map_err(|_| ReadError::InvalidSource)?,
        observed_at: timestamp,
        next_cursor: None,
        complete: degraded.is_empty(),
        projection_limits: ProjectionLimitsV1 {
            page_items: 1,
            name_bytes: 4096,
            event_text_bytes: 8192,
            decision_text_bytes: 8192,
            item_bytes: 65_536,
            envelope_bytes: 524_288,
        },
        degraded,
        coverage: coverage.to_vec(),
        item,
    };
    rsi_common::remote_read::encode(&WireDocumentV1::Response(
        ReadResponseV1::RemoteGetSessionV1(response.clone()),
    ))
    .map_err(|_| ReadError::InvalidSource)?;
    Ok(response)
}

fn wire_event_key(key: (i32, i64)) -> Result<EventKeyV1> {
    if key.1 <= 0 {
        return Err(ReadError::InvalidSource);
    }
    Ok(EventKeyV1 {
        sequence: key.0,
        id: rsi_common::remote_read::DecimalI64::new(key.1.to_string())
            .map_err(|_| ReadError::InvalidSource)?,
    })
}

/// Project a page and its same-transaction head/interval observation into a
/// wire response. Complete tool IDs get an exact, capped Store duplicate-key
/// check; a saturated lookup refuses an exact-pairing claim. The caller keeps
/// the same transaction open and the owner signs `cursor` after selection.
pub fn observed_history_response(
    conn: &Connection,
    request: &RemoteGetHistoryPageV1,
    observed: HistoryObservedPage,
    daemon_epoch: Uuid,
    observed_at: DateTime<Utc>,
    cursor: Option<CursorV1>,
) -> Result<HistoryResponseV1> {
    if conn.is_autocommit() {
        return Err(ReadError::InvalidSource);
    }
    let session =
        Uuid::parse_str(request.session_id.as_str()).map_err(|_| ReadError::InvalidSource)?;
    let head = observed.head.map(wire_event_key).transpose()?;
    let interval = HistoryIntervalV1 {
        lower_exclusive: observed.lower_exclusive.map(wire_event_key).transpose()?,
        upper_inclusive: observed.upper_inclusive.map(wire_event_key).transpose()?,
    };
    let items = observed
        .page
        .items
        .into_iter()
        .map(|row| {
            let ambiguous = if let Some(key) = &row.tool_pair_key {
                tool_pair_ambiguous(conn, session, row.id, key)?
            } else {
                false
            };
            history_event(row, ambiguous)
        })
        .collect::<Result<Vec<_>>>()?;
    history_response(
        request,
        SourcePage {
            items,
            next: observed.page.next,
            has_more: observed.page.has_more,
        },
        head,
        interval,
        daemon_epoch,
        observed_at,
        cursor,
    )
}

/// The exact i64 source edge is checked against the emitted page. Locate and
/// relocation require a separate source calculation and are not represented.
fn history_response(
    request: &RemoteGetHistoryPageV1,
    page: SourcePage<HistoryEventV1, (i32, i64)>,
    head: Option<EventKeyV1>,
    interval: HistoryIntervalV1,
    daemon_epoch: Uuid,
    observed_at: DateTime<Utc>,
    cursor: Option<CursorV1>,
) -> Result<HistoryResponseV1> {
    if !(1..=50).contains(&request.limit)
        || page.items.len() > request.limit as usize
        || (page.has_more && page.items.len() != request.limit as usize)
        || matches!(&request.window, HistoryWindowV1::Locate { .. })
        || page.has_more != page.next.is_some()
        || page.has_more != cursor.is_some()
        || cursor
            .as_ref()
            .is_some_and(|cursor| !matches!(cursor, CursorV1::History { .. }))
    {
        return Err(ReadError::InvalidSource);
    }
    let edge = match &request.window {
        HistoryWindowV1::Latest {} | HistoryWindowV1::Older { .. } => page.items.first(),
        HistoryWindowV1::Newer { .. } | HistoryWindowV1::Interval { .. } => page.items.last(),
        HistoryWindowV1::Locate { .. } => unreachable!(),
    };
    if page.next.is_some() && edge.map(|event| (event.sequence, event.id.get())) != page.next {
        return Err(ReadError::InvalidSource);
    }
    let timestamp = Timestamp::new(observed_at.to_rfc3339_opts(SecondsFormat::Nanos, true))
        .map_err(|_| ReadError::InvalidSource)?;
    let response = HistoryResponseV1 {
        version: Text::<3>::new(rsi_common::remote_read::VERSION.into())
            .map_err(|_| ReadError::InvalidSource)?,
        daemon_epoch: WireUuid::new(daemon_epoch.to_string())
            .map_err(|_| ReadError::InvalidSource)?,
        observed_at: timestamp.clone(),
        next_cursor: cursor,
        complete: true,
        projection_limits: ProjectionLimitsV1 {
            page_items: request.limit,
            name_bytes: 512,
            event_text_bytes: 8192,
            decision_text_bytes: 8192,
            item_bytes: 65_536,
            envelope_bytes: 524_288,
        },
        degraded: Vec::new(),
        coverage: vec![SourceCoverageV1 {
            source: SourceV1::StoreHistory,
            state: CoverageStateV1::Complete,
            has_more: page.has_more,
            lower_bound: DecimalU64::new(page.items.len().to_string())
                .map_err(|_| ReadError::InvalidSource)?,
            observed_at: timestamp,
            observation_order: 1,
        }],
        project_id: request.project_id.clone(),
        session_id: request.session_id.clone(),
        items: page.items,
        window: request.window.clone(),
        head,
        interval,
        position: HistoryPositionV1::Unchanged {},
    };
    rsi_common::remote_read::encode(&WireDocumentV1::Response(
        ReadResponseV1::RemoteGetHistoryPageV1(response.clone()),
    ))
    .map_err(|_| ReadError::InvalidSource)?;
    Ok(response)
}

#[cfg(all(
    test,
    any(not(feature = "test-shard-mode"), feature = "test-shard-store-04")
))]
mod tests {
    use super::*;
    use crate::remote_read::{
        BoundedText, HistoryRow, RemoteCursorSigner, SessionDetailSource, SessionRow,
        history_event, list_projects, session_attention, session_detail, session_summary,
    };
    use rsi_common::remote_read::{
        DecimalI64, KnownSessionStatusV1, PairingStateV1, ReadRequestV1, SessionStatusV1,
    };
    use rusqlite::Connection;
    use serde_json::json;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn sessions_response_preserves_hidden_key_continuation() {
        let project_id = Uuid::new_v4();
        let hidden = Uuid::new_v4();
        let request: ReadRequestV1 = serde_json::from_value(json!({
            "method":"RemoteListSessionsV1",
            "params":{"project_id":project_id,"limit":1}
        }))
        .unwrap();
        let ReadRequestV1::RemoteListSessionsV1(params) = &request else {
            unreachable!()
        };
        let signer = RemoteCursorSigner::new(Uuid::new_v4());
        let cursor = signer
            .sign_list_position(&request, [3; 32], Some(hidden), true)
            .unwrap();
        let now = Utc::now();
        let at = Timestamp::new(now.to_rfc3339_opts(SecondsFormat::Nanos, true)).unwrap();
        let row = |source, order, has_more| SourceCoverageV1 {
            source,
            state: CoverageStateV1::Complete,
            has_more,
            lower_bound: DecimalU64::new("0".into()).unwrap(),
            observed_at: at.clone(),
            observation_order: order,
        };
        let coverage = [
            row(SourceV1::ActiveSessions, 1, false),
            row(SourceV1::CompletedSessions, 2, false),
            row(SourceV1::StoreSessions, 3, true),
        ];
        let project = project(ProjectRow {
            id: project_id,
            name: BoundedText {
                text: "Configured".into(),
                observed_bytes: 10,
                truncated: false,
            },
        })
        .unwrap();
        let page = SourcePage {
            items: Vec::new(),
            next: Some(hidden),
            has_more: true,
        };
        let response = sessions_response(
            params,
            SessionsResponseSources {
                project: project.clone(),
                after: None,
                page,
                coverage: coverage.clone(),
                cursor: cursor.clone(),
            },
            Uuid::new_v4(),
            now,
        )
        .unwrap();
        assert!(response.items.is_empty());
        assert!(response.next_cursor.is_some());
        assert!(matches!(
            sessions_response(
                params,
                SessionsResponseSources {
                    project,
                    after: Some(hidden),
                    page: SourcePage {
                        items: Vec::new(),
                        next: Some(hidden),
                        has_more: true,
                    },
                    coverage,
                    cursor,
                },
                Uuid::new_v4(),
                now,
            ),
            Err(ReadError::InvalidSource)
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn selected_session_response_preserves_source_uncertainty() {
        let now = Utc::now();
        let at = Timestamp::new(now.to_rfc3339_opts(SecondsFormat::Nanos, true)).unwrap();
        let project = Uuid::new_v4();
        let session = Uuid::new_v4();
        let request = RemoteGetSessionV1 {
            project_id: WireUuid::new(project.to_string()).unwrap(),
            session_id: WireUuid::new(session.to_string()).unwrap(),
        };
        let row = |source: SourceV1, state: CoverageStateV1, lower_bound: u64, order: u32| {
            SourceCoverageV1 {
                source,
                state,
                has_more: false,
                lower_bound: DecimalU64::new(lower_bound.to_string()).unwrap(),
                observed_at: at.clone(),
                observation_order: order,
            }
        };
        let mut pending: Vec<_> = [
            SourceV1::QuestionPublications,
            SourceV1::TrackedQuestionSlot,
            SourceV1::SessionQuestionSlot,
            SourceV1::DurableQuestionFallback,
            SourceV1::NativeRuntime,
            SourceV1::NativePublications,
            SourceV1::NativeHistoricalFallback,
            SourceV1::LegacyApprovals,
        ]
        .into_iter()
        .enumerate()
        .map(|(index, source)| row(source, CoverageStateV1::Complete, 0, index as u32 + 4))
        .collect();
        let make_item = |pending: Vec<SourceCoverageV1>| {
            let status = SessionStatusV1::Known {
                value: KnownSessionStatusV1::Running,
            };
            let attention = session_attention(&status, 0, &pending).unwrap();
            let title = BoundedText {
                text: "Selected session".into(),
                observed_bytes: 16,
                truncated: false,
            };
            let summary = session_summary(
                SessionRow {
                    id: session,
                    parent_id: None,
                    continued_from: None,
                    kind: "Standard".into(),
                    provider: "Codex".into(),
                    status: "Running".into(),
                    updated_at: at.as_str().into(),
                    own_title: title.clone(),
                },
                project,
                attention,
            )
            .unwrap();
            session_detail(
                SessionDetailSource {
                    own_title: title,
                    query: None,
                    model: None,
                    saved_history_head: None,
                },
                summary,
                vec![],
                pending,
            )
            .unwrap()
        };
        let sources = [
            row(SourceV1::ActiveSessions, CoverageStateV1::Complete, 0, 1),
            row(SourceV1::CompletedSessions, CoverageStateV1::Complete, 0, 2),
            row(SourceV1::StoreSessions, CoverageStateV1::Complete, 1, 3),
        ];
        let item = make_item(pending.clone());
        let complete =
            selected_session_response(&request, item.clone(), sources.clone(), project, now)
                .unwrap();
        assert!(complete.complete);
        assert_eq!(complete.coverage[2].lower_bound.get(), 1);

        let mut runtime_busy = sources.clone();
        runtime_busy[0].state = CoverageStateV1::Busy;
        let busy = selected_session_response(&request, item, runtime_busy, project, now).unwrap();
        assert!(!busy.complete);
        assert_eq!(busy.degraded, vec![DegradationV1::Busy]);

        pending[0].state = CoverageStateV1::Limited;
        pending[0].has_more = true;
        let limited =
            selected_session_response(&request, make_item(pending), sources.clone(), project, now)
                .unwrap();
        assert!(!limited.complete);
        assert_eq!(limited.degraded, vec![DegradationV1::Limited]);
        let mut missing_store = sources;
        missing_store[2].lower_bound = DecimalU64::new("0".into()).unwrap();
        assert!(matches!(
            selected_session_response(&request, make_item(vec![]), missing_store, project, now),
            Err(ReadError::InvalidSource)
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn project_response_keeps_empty_visible_page_and_examined_position() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE projects(id TEXT PRIMARY KEY,name TEXT NOT NULL)")
            .unwrap();
        let ids = [
            Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
            Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap(),
            Uuid::parse_str("00000000-0000-0000-0000-000000000003").unwrap(),
        ];
        for id in [ids[0], ids[2]] {
            conn.execute(
                "INSERT INTO projects(id,name) VALUES(?1,'visible')",
                [id.to_string()],
            )
            .unwrap();
        }
        let request: ReadRequestV1 = serde_json::from_value(json!({
            "method":"RemoteListProjectsV1",
            "params":{"project_ids":ids,"limit":1}
        }))
        .unwrap();
        let epoch = Uuid::new_v4();
        let signer = RemoteCursorSigner::new(epoch);
        let now = Utc::now();
        let first = list_projects(&conn, &ids, None, 1).unwrap();
        let first_cursor = signer
            .sign_list_position(&request, [7; 32], first.next, first.has_more)
            .unwrap();
        let first =
            projects_response(&ids, None, 1, first, epoch, now, first_cursor.clone()).unwrap();
        assert_eq!(first.items.len(), 1);
        assert_eq!(first.coverage[0].lower_bound.get(), 1);
        assert_eq!(first.coverage[1].lower_bound.get(), 1);
        let after = signer
            .verify_list_position(&request, [7; 32], &first_cursor.unwrap())
            .unwrap();
        let middle = list_projects(&conn, &ids, Some(after), 1).unwrap();
        let middle_cursor = signer
            .sign_list_position(&request, [7; 32], middle.next, middle.has_more)
            .unwrap();
        let middle = projects_response(
            &ids,
            Some(after),
            1,
            middle,
            epoch,
            now,
            middle_cursor.clone(),
        )
        .unwrap();
        assert!(middle.items.is_empty());
        assert!(middle.complete);
        assert_eq!(middle.coverage[0].lower_bound.get(), 1);
        assert_eq!(middle.coverage[1].lower_bound.get(), 0);
        assert!(middle.next_cursor.is_some());
        let after = signer
            .verify_list_position(&request, [7; 32], &middle_cursor.unwrap())
            .unwrap();
        assert_eq!(after, ids[1]);
        let last = list_projects(&conn, &ids, Some(after), 1).unwrap();
        let last = projects_response(&ids, Some(after), 1, last, epoch, now, None).unwrap();
        assert_eq!(last.items[0].id.as_str(), ids[2].to_string());
        assert!(last.next_cursor.is_none());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn project_response_rejects_fabricated_page_progress() {
        let id = Uuid::new_v4();
        let empty = SourcePage::<ProjectRow, Uuid> {
            items: Vec::new(),
            next: Some(id),
            has_more: true,
        };
        assert!(matches!(
            projects_response(&[id], None, 1, empty, Uuid::new_v4(), Utc::now(), None),
            Err(ReadError::InvalidSource)
        ));
    }

    fn raw_event(sequence: i32, id: i64) -> HistoryRow {
        HistoryRow {
            id,
            sequence,
            event_type: "message".into(),
            role: None,
            created_at: "2026-09-27T00:00:00.000000000Z".into(),
            content: BoundedText {
                text: "ok".into(),
                observed_bytes: 2,
                truncated: false,
            },
            tool_name: None,
            tool_pair_key: None,
            tool_id_display: None,
            offloaded: false,
        }
    }

    fn event(sequence: i32, id: i64) -> HistoryEventV1 {
        history_event(raw_event(sequence, id), false).unwrap()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn history_response_preserves_lossless_oldest_edge_and_refuses_wrong_edge() {
        let project = Uuid::new_v4();
        let session = Uuid::new_v4();
        let request: ReadRequestV1 = serde_json::from_value(json!({
            "method":"RemoteGetHistoryPageV1",
            "params":{"project_id":project,"session_id":session,"window":{"kind":"latest"},"limit":2}
        }))
        .unwrap();
        let ReadRequestV1::RemoteGetHistoryPageV1(params) = &request else {
            unreachable!()
        };
        let epoch = Uuid::new_v4();
        let signer = RemoteCursorSigner::new(epoch);
        let older = 9_007_199_254_740_993_i64;
        let newer = older + 1;
        let cursor = signer
            .sign_history_position(&request, [3; 32], Some((1, older)), true)
            .unwrap();
        let key = |sequence: i32, id: i64| EventKeyV1 {
            sequence,
            id: DecimalI64::new(id.to_string()).unwrap(),
        };
        let page = SourcePage {
            items: vec![event(1, older), event(2, newer)],
            next: Some((1, older)),
            has_more: true,
        };
        let response = history_response(
            params,
            page.clone(),
            Some(key(2, newer)),
            HistoryIntervalV1 {
                lower_exclusive: None,
                upper_inclusive: Some(key(2, newer)),
            },
            epoch,
            Utc::now(),
            cursor.clone(),
        )
        .unwrap();
        assert_eq!(response.items[0].id.get(), older);
        assert_eq!(response.coverage[0].lower_bound.get(), 2);
        assert!(matches!(
            signer.verify_history_position(&request, [3; 32], &cursor.unwrap()),
            Ok(crate::remote_read::HistoryRange::Older { anchor }) if anchor == (1, older)
        ));
        let wrong = SourcePage {
            next: Some((2, newer)),
            ..page
        };
        assert!(matches!(
            history_response(
                params,
                wrong,
                Some(key(2, newer)),
                HistoryIntervalV1 {
                    lower_exclusive: None,
                    upper_inclusive: Some(key(2, newer)),
                },
                epoch,
                Utc::now(),
                response.next_cursor,
            ),
            Err(ReadError::InvalidSource)
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn observed_history_response_carries_exact_source_interval() {
        let project = Uuid::new_v4();
        let session = Uuid::new_v4();
        let request: ReadRequestV1 = serde_json::from_value(json!({
            "method":"RemoteGetHistoryPageV1",
            "params":{"project_id":project,"session_id":session,"window":{"kind":"latest"},"limit":2}
        }))
        .unwrap();
        let ReadRequestV1::RemoteGetHistoryPageV1(params) = &request else {
            unreachable!()
        };
        let epoch = Uuid::new_v4();
        let signer = RemoteCursorSigner::new(epoch);
        let older = 9_007_199_254_740_993_i64;
        let newer = older + 1;
        let cursor = signer
            .sign_history_position(&request, [4; 32], Some((1, older)), true)
            .unwrap();
        let observed = HistoryObservedPage {
            page: SourcePage {
                items: vec![raw_event(1, older), raw_event(2, newer)],
                next: Some((1, older)),
                has_more: true,
            },
            head: Some((2, newer)),
            lower_exclusive: Some((0, older - 1)),
            upper_inclusive: Some((2, newer)),
        };
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("BEGIN").unwrap();
        let response =
            observed_history_response(&conn, params, observed, epoch, Utc::now(), cursor).unwrap();
        assert_eq!(
            response.interval.lower_exclusive.unwrap().id.get(),
            older - 1
        );
        assert_eq!(response.interval.upper_inclusive.unwrap().id.get(), newer);
        assert_eq!(response.head.unwrap().id.get(), newer);
        conn.execute_batch("ROLLBACK").unwrap();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn observed_history_response_marks_reused_complete_tool_key_ambiguous() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE conversation_events(id INTEGER PRIMARY KEY,session_id TEXT,tool_use_id TEXT);
             CREATE INDEX idx_events_tool_use_id ON conversation_events(tool_use_id);",
        )
        .unwrap();
        let project = Uuid::new_v4();
        let session = Uuid::new_v4();
        for id in [1_i64, 2] {
            conn.execute(
                "INSERT INTO conversation_events(id,session_id,tool_use_id) VALUES(?1,?2,'shared')",
                rusqlite::params![id, session.to_string()],
            )
            .unwrap();
        }
        let request: ReadRequestV1 = serde_json::from_value(json!({
            "method":"RemoteGetHistoryPageV1",
            "params":{"project_id":project,"session_id":session,"window":{"kind":"latest"},"limit":1}
        }))
        .unwrap();
        let ReadRequestV1::RemoteGetHistoryPageV1(params) = &request else {
            unreachable!()
        };
        let mut row = raw_event(2, 2);
        row.tool_pair_key = Some("shared".into());
        row.tool_id_display = Some(BoundedText {
            text: "shared".into(),
            observed_bytes: 6,
            truncated: false,
        });
        let epoch = Uuid::new_v4();
        let cursor = RemoteCursorSigner::new(epoch)
            .sign_history_position(&request, [5; 32], Some((2, 2)), true)
            .unwrap();
        conn.execute_batch("BEGIN").unwrap();
        let response = observed_history_response(
            &conn,
            params,
            HistoryObservedPage {
                page: SourcePage {
                    items: vec![row],
                    next: Some((2, 2)),
                    has_more: true,
                },
                head: Some((2, 2)),
                lower_exclusive: Some((1, 1)),
                upper_inclusive: Some((2, 2)),
            },
            epoch,
            Utc::now(),
            cursor,
        )
        .unwrap();
        assert_eq!(response.items[0].pairing_state, PairingStateV1::Ambiguous);
        conn.execute_batch("ROLLBACK").unwrap();
    }
}
