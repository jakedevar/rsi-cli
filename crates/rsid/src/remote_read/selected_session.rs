use super::{
    BoundedText, ReadError, Result, RuntimeSessionCandidate, RuntimeSessionCandidateSnapshot,
    RuntimeSessionObservation, RuntimeSessionRecheck, RuntimeSessionRecheckObservation,
    SessionCandidateOrigin, SessionDetailSource, SessionRow, get_session,
    selected_session_coverage, selected_session_response, session_attention, session_detail,
    session_detail_source, session_summary,
};
use chrono::{DateTime, Utc};
use rsi_common::remote_read::{
    AttentionV1, DecimalI64, DecimalU64, RemoteGetSessionV1, SequenceObservationV1,
    SessionResponseV1, SourceCoverageV1, SourceV1,
};
use rsi_common::types::{ConversationEvent, Session, SessionStatus};
use rusqlite::{Connection, OptionalExtension};
use uuid::Uuid;

/// One exact saved-session witness, fully copied while the Store transaction
/// remains active. Private fields prevent a caller from claiming a Store row
/// from runtime absence or an unscoped page.
pub struct SelectedSavedSession {
    project: Uuid,
    session: Uuid,
    row: SessionRow,
    detail: SessionDetailSource,
    observed_at: DateTime<Utc>,
}

impl SelectedSavedSession {
    pub(crate) const fn identity(&self) -> (Uuid, Uuid, DateTime<Utc>) {
        (self.project, self.session, self.observed_at)
    }
}

/// Bounded fields copied from one exact runtime map entry while its read guard
/// is held. This does not itself establish that the Store lacks the session.
#[derive(Clone)]
pub struct SelectedRuntimeSession {
    project: Uuid,
    session: Uuid,
    origin: SessionCandidateOrigin,
    candidate: RuntimeSessionCandidate,
    row: SessionRow,
    detail: SessionDetailSource,
    runtime_sequence: Option<SequenceObservationV1>,
    captured_at: DateTime<Utc>,
}

fn preview(raw: &str, cap: usize) -> BoundedText {
    let mut end = cap.min(raw.len());
    while !raw.is_char_boundary(end) {
        end -= 1;
    }
    BoundedText {
        text: raw[..end].to_owned(),
        observed_bytes: raw.len() as u64,
        truncated: end < raw.len(),
    }
}

impl SelectedRuntimeSession {
    /// Called under the selected runtime map guard. Copy only the fields that
    /// Remote V1 projects; never clone paths, events or the whole Session.
    pub(crate) fn capture(
        session: &Session,
        origin: SessionCandidateOrigin,
        spawn_generation: Option<u64>,
        observed_events: Option<&[ConversationEvent]>,
        captured_at: DateTime<Utc>,
    ) -> Result<Self> {
        let project = session.project_id.ok_or(ReadError::SourceUnavailable)?;
        if session.is_eval
            || matches!(
                session.status,
                SessionStatus::Archived | SessionStatus::Deleted
            )
        {
            return Err(ReadError::SourceUnavailable);
        }
        let own_title = session
            .title
            .as_deref()
            .filter(|title| !title.is_empty())
            .or_else(|| (!session.query.is_empty()).then_some(session.query.as_str()))
            .unwrap_or("Untitled session");
        let candidate = RuntimeSessionCandidate {
            id: session.id,
            project_id: Some(project),
            status: session.status,
            is_eval: session.is_eval,
            updated_at_seconds: session.updated_at.timestamp(),
            updated_at_nanosecond: session.updated_at.timestamp_subsec_nanos(),
            spawn_generation,
        };
        let runtime_sequence = observed_events
            .map(|events| {
                let last = events.last();
                if last.is_some_and(|event| event.session_id != session.id || event.id < 0) {
                    return Err(ReadError::InvalidSource);
                }
                Ok(SequenceObservationV1 {
                    source: match origin {
                        SessionCandidateOrigin::Active => SourceV1::ActiveSessions,
                        SessionCandidateOrigin::Completed => SourceV1::CompletedSessions,
                        SessionCandidateOrigin::Store => return Err(ReadError::InvalidSource),
                    },
                    sequence: last.map(|event| event.sequence),
                    event_id: last
                        .filter(|event| event.id > 0)
                        .map(|event| DecimalI64::new(event.id.to_string()))
                        .transpose()
                        .map_err(|_| ReadError::InvalidSource)?,
                })
            })
            .transpose()?;
        Ok(Self {
            project,
            session: session.id,
            origin,
            candidate,
            row: SessionRow {
                id: session.id,
                parent_id: session.parent_id,
                continued_from: session.continued_from,
                kind: format!("{:?}", session.session_kind),
                provider: format!("{:?}", session.provider),
                status: format!("{:?}", session.status),
                updated_at: session
                    .updated_at
                    .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                own_title: preview(own_title, 512),
            },
            detail: SessionDetailSource {
                own_title: preview(own_title, 4096),
                query: Some(preview(&session.query, 4096)),
                model: session.model.as_deref().map(|model| preview(model, 4096)),
                saved_history_head: None,
            },
            runtime_sequence,
            captured_at,
        })
    }

    pub(crate) fn matches_reread(&self, after: &Self) -> bool {
        self.project == after.project
            && self.session == after.session
            && self.origin == after.origin
            && self.candidate == after.candidate
            && self.runtime_sequence == after.runtime_sequence
    }

    pub(crate) fn runtime_sequence(&self) -> Option<SequenceObservationV1> {
        self.runtime_sequence.clone()
    }
}

/// Copy only the list-summary fields of one runtime entry under its map
/// guard. The earlier scalar witness must still match this exact entry.
pub fn runtime_summary_row(
    session: &Session,
    project: Uuid,
    expected: RuntimeSessionCandidate,
    spawn_generation: Option<u64>,
) -> Result<SessionRow> {
    if session.id != expected.id
        || session.project_id != Some(project)
        || expected.project_id != Some(project)
        || session.status != expected.status
        || session.is_eval != expected.is_eval
        || session.updated_at.timestamp() != expected.updated_at_seconds
        || session.updated_at.timestamp_subsec_nanos() != expected.updated_at_nanosecond
        || spawn_generation != expected.spawn_generation
        || session.is_eval
        || matches!(
            session.status,
            SessionStatus::Archived | SessionStatus::Deleted
        )
    {
        return Err(ReadError::SourceUnavailable);
    }
    let own_title = session
        .title
        .as_deref()
        .filter(|title| !title.is_empty())
        .or_else(|| (!session.query.is_empty()).then_some(session.query.as_str()))
        .unwrap_or("Untitled session");
    Ok(SessionRow {
        id: session.id,
        parent_id: session.parent_id,
        continued_from: session.continued_from,
        kind: format!("{:?}", session.session_kind),
        provider: format!("{:?}", session.provider),
        status: format!("{:?}", session.status),
        updated_at: session
            .updated_at
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
        own_title: preview(own_title, 512),
    })
}

/// A transaction-scoped exact miss, paired with a runtime witness. Any Store
/// row for the ID, including a foreign-project row, prevents this path.
pub struct SelectedRuntimeOnlySession {
    runtime: SelectedRuntimeSession,
    observed_at: DateTime<Utc>,
}

impl SelectedRuntimeOnlySession {
    pub(crate) const fn identity(&self) -> (Uuid, Uuid, DateTime<Utc>) {
        (self.runtime.project, self.runtime.session, self.observed_at)
    }

    /// A post-Store map copy can only continue the runtime-only decision read
    /// when the exact session, origin and scalar generation still match.
    pub(crate) fn matches_runtime_reread(&self, after: &SelectedRuntimeSession) -> bool {
        self.runtime.matches_reread(after) && after.captured_at >= self.observed_at
    }
}

pub fn selected_runtime_store_miss(
    conn: &Connection,
    project: Uuid,
    session: Uuid,
    runtime: SelectedRuntimeSession,
) -> Result<SelectedRuntimeOnlySession> {
    if conn.is_autocommit() || runtime.project != project || runtime.session != session {
        return Err(ReadError::InvalidSource);
    }
    let stored_row: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sessions WHERE id=?1",
            [session.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    let stored_event: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM conversation_events WHERE session_id=?1 LIMIT 1",
            [session.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    if stored_row.is_some() || stored_event.is_some() {
        return Err(ReadError::SourceUnavailable);
    }
    let observed_at = Utc::now();
    if runtime.captured_at > observed_at {
        return Err(ReadError::InvalidSource);
    }
    Ok(SelectedRuntimeOnlySession {
        runtime,
        observed_at,
    })
}

/// Read both saved projections within the caller's bounded Store transaction.
/// The caller releases that transaction before any runtime reread.
pub fn selected_saved_session(
    conn: &Connection,
    project: Uuid,
    session: Uuid,
) -> Result<SelectedSavedSession> {
    if conn.is_autocommit() {
        return Err(ReadError::InvalidSource);
    }
    let row = get_session(conn, project, session)?;
    let detail = session_detail_source(conn, project, session)?;
    Ok(SelectedSavedSession {
        project,
        session,
        row,
        detail,
        observed_at: Utc::now(),
    })
}

/// Runtime and pending observations supplied after the Store lock is released.
/// The caller captures runtime candidates before Store work and rereads them
/// afterward. Pending coverage must come from its eight bounded sources.
pub struct SelectedSessionSources<'a> {
    pub runtime_capture: RuntimeSessionObservation<'a, RuntimeSessionCandidateSnapshot>,
    pub runtime_reread: Option<RuntimeSessionObservation<'a, RuntimeSessionRecheckObservation>>,
    pub runtime_sequences: Vec<SequenceObservationV1>,
    pub pending_coverage: Vec<SourceCoverageV1>,
    pub live_signals_lower_bound: u64,
}

/// Finish an exact selected response without reopening Store or retaining its
/// lock across runtime work. Completeness describes bounded source attempts,
/// never a frozen cross-source snapshot or answer authority.
pub fn finish_selected_session_response(
    request: &RemoteGetSessionV1,
    saved: SelectedSavedSession,
    sources: SelectedSessionSources<'_>,
    daemon_epoch: Uuid,
    observed_at: DateTime<Utc>,
) -> Result<SessionResponseV1> {
    if request.project_id.as_str() != saved.project.to_string()
        || request.session_id.as_str() != saved.session.to_string()
        || saved.row.id != saved.session
        || observed_at < saved.observed_at
    {
        return Err(ReadError::InvalidSource);
    }
    let coverage = selected_session_coverage(
        saved.project,
        saved.session,
        sources.runtime_capture,
        sources.runtime_reread,
        saved.observed_at,
        true,
    )?;
    for row in coverage.iter().chain(&sources.pending_coverage) {
        let source_time = DateTime::parse_from_rfc3339(row.observed_at.as_str())
            .map_err(|_| ReadError::InvalidSource)?
            .with_timezone(&Utc);
        if source_time > observed_at {
            return Err(ReadError::InvalidSource);
        }
    }
    let provisional_attention = AttentionV1 {
        requires_local_action: true,
        incomplete: true,
        live_signals_lower_bound: DecimalU64::new("0".into())
            .map_err(|_| ReadError::InvalidSource)?,
    };
    let mut summary = session_summary(saved.row, saved.project, provisional_attention)?;
    summary.attention = session_attention(
        &summary.status,
        sources.live_signals_lower_bound,
        &sources.pending_coverage,
    )?;
    let detail = session_detail(
        saved.detail,
        summary,
        sources.runtime_sequences,
        sources.pending_coverage,
    )?;
    selected_session_response(request, detail, coverage, daemon_epoch, observed_at)
}

/// Finish a session observed only in runtime after an exact Store miss. The
/// caller must reread runtime scalars after releasing the Store transaction.
/// A changed or unavailable reread cannot prove this selected identity.
pub fn finish_runtime_only_session_response(
    request: &RemoteGetSessionV1,
    selected: SelectedRuntimeOnlySession,
    sources: SelectedSessionSources<'_>,
    daemon_epoch: Uuid,
    observed_at: DateTime<Utc>,
) -> Result<SessionResponseV1> {
    let runtime = selected.runtime;
    if request.project_id.as_str() != runtime.project.to_string()
        || request.session_id.as_str() != runtime.session.to_string()
        || observed_at < selected.observed_at
        || runtime.row.id != runtime.session
        || runtime.detail.saved_history_head.is_some()
    {
        return Err(ReadError::InvalidSource);
    }
    let (before, after) = match (&sources.runtime_capture, &sources.runtime_reread) {
        (
            RuntimeSessionObservation::Observed(before),
            Some(RuntimeSessionObservation::Observed(after)),
        ) if after.state == RuntimeSessionRecheck::NoObservedChange => (*before, *after),
        _ => return Err(ReadError::SourceUnavailable),
    };
    let (rows, captured_at) = match runtime.origin {
        SessionCandidateOrigin::Active => (&before.active, before.active_observed_at),
        SessionCandidateOrigin::Completed => (&before.completed, before.completed_observed_at),
        SessionCandidateOrigin::Store => return Err(ReadError::InvalidSource),
    };
    if captured_at < runtime.captured_at
        || rows.iter().filter(|row| row.id == runtime.session).count() != 1
        || rows.iter().find(|row| row.id == runtime.session) != Some(&runtime.candidate)
        || after.active_observed_at > observed_at
        || after.completed_observed_at > observed_at
    {
        return Err(ReadError::SourceUnavailable);
    }
    let coverage = selected_session_coverage(
        runtime.project,
        runtime.session,
        sources.runtime_capture,
        sources.runtime_reread,
        selected.observed_at,
        false,
    )?;
    let selected_index = usize::from(runtime.origin == SessionCandidateOrigin::Completed);
    if coverage[selected_index].state != rsi_common::remote_read::CoverageStateV1::Complete
        || coverage[selected_index].lower_bound.get() != 1
    {
        return Err(ReadError::SourceUnavailable);
    }
    for row in coverage.iter().chain(&sources.pending_coverage) {
        let source_time = DateTime::parse_from_rfc3339(row.observed_at.as_str())
            .map_err(|_| ReadError::InvalidSource)?
            .with_timezone(&Utc);
        if source_time > observed_at {
            return Err(ReadError::InvalidSource);
        }
    }
    let provisional_attention = AttentionV1 {
        requires_local_action: true,
        incomplete: true,
        live_signals_lower_bound: DecimalU64::new("0".into())
            .map_err(|_| ReadError::InvalidSource)?,
    };
    let mut summary = session_summary(runtime.row, runtime.project, provisional_attention)?;
    summary.attention = session_attention(
        &summary.status,
        sources.live_signals_lower_bound,
        &sources.pending_coverage,
    )?;
    let detail = session_detail(
        runtime.detail,
        summary,
        sources.runtime_sequences,
        sources.pending_coverage,
    )?;
    selected_session_response(request, detail, coverage, daemon_epoch, observed_at)
}

#[cfg(all(
    test,
    any(not(feature = "test-shard-mode"), feature = "test-shard-store-04")
))]
mod tests {
    use super::*;
    use crate::remote_read::{
        NativeRuntimeApprovalListRecheckObservation, NativeRuntimeApprovalListSnapshot,
        NativeRuntimeApprovalListState, NativeRuntimeApprovalRecheck,
        RuntimeQuestionSlotListRecheckObservation, RuntimeQuestionSlotListSnapshot,
        RuntimeQuestionSlotRecheck, RuntimeSessionCandidate, RuntimeSessionRecheck,
        capture_runtime_only_pending_sources, capture_saved_pending_runtime_sources,
        finish_runtime_only_pending_sources, finish_saved_pending_sources,
        selected_saved_pending_sources,
    };
    use chrono::{Duration, SecondsFormat};
    use rsi_common::remote_read::{
        CoverageStateV1, DecisionId, DegradationV1, SelectedDecisionV1, SourceV1, Timestamp,
        WireUuid,
    };
    use rsi_common::types::SessionStatus;
    use rusqlite::params;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn saved_pending_capture_store_and_reread_release_locks_in_order() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions(id TEXT PRIMARY KEY,project_id TEXT,status TEXT,is_eval INTEGER,
                parent_id TEXT,continued_from TEXT,session_kind TEXT,provider TEXT,updated_at TEXT,
                title TEXT,query TEXT,model TEXT,pending_question_json TEXT);
             CREATE TABLE conversation_events(id INTEGER PRIMARY KEY,session_id TEXT,sequence INTEGER);
             CREATE TABLE pending_question_publications(publication_id TEXT,session_id TEXT);
             CREATE TABLE appserver_approval_publications(publication_id TEXT,session_id TEXT);
             CREATE INDEX appserver_approval_publications_session ON appserver_approval_publications(session_id,publication_id);
             CREATE TABLE pending_appserver_approvals(publication_id TEXT,session_id TEXT);
             CREATE TABLE approvals(id TEXT,session_id TEXT);
             CREATE INDEX idx_approvals_session_id ON approvals(session_id);",
        )
        .unwrap();
        let project = Uuid::new_v4();
        let session = Uuid::new_v4();
        conn.execute(
            "INSERT INTO sessions(id,project_id,status,is_eval,session_kind,provider,updated_at,title)
             VALUES(?1,?2,'Running',0,'Standard','Codex',?3,'Saved')",
            params![session.to_string(), project.to_string(), Utc::now().to_rfc3339()],
        )
        .unwrap();
        let captured_at = Utc::now();
        assert!(matches!(
            selected_saved_pending_sources(&conn, project, session, [None; 4], 1),
            Err(ReadError::InvalidSource)
        ));
        let capture = capture_saved_pending_runtime_sources(
            project,
            session,
            |p, s| {
                assert!(conn.is_autocommit());
                Ok(RuntimeQuestionSlotListSnapshot {
                    project: p,
                    session: s,
                    active_observed_at: captured_at,
                    completed_observed_at: captured_at,
                    active_generation: None,
                    completed_found: false,
                    slots: Vec::new(),
                })
            },
            |s| {
                assert!(conn.is_autocommit());
                Ok(NativeRuntimeApprovalListSnapshot {
                    session: s,
                    observed_at: captured_at,
                    state: NativeRuntimeApprovalListState::Present(Vec::new()),
                })
            },
        )
        .unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        let (store, selected) =
            selected_saved_pending_sources(&tx, project, session, [None; 4], 1).unwrap();
        tx.rollback().unwrap();
        let result = finish_saved_pending_sources(
            &selected,
            capture,
            store,
            |_| {
                assert!(conn.is_autocommit());
                let at = Utc::now();
                Ok(RuntimeQuestionSlotListRecheckObservation {
                    state: RuntimeQuestionSlotRecheck::NoObservedChange,
                    active_observed_at: at,
                    completed_observed_at: at,
                })
            },
            |_| {
                assert!(conn.is_autocommit());
                Ok(NativeRuntimeApprovalListRecheckObservation {
                    state: NativeRuntimeApprovalRecheck::NoObservedChange,
                    observed_at: Utc::now(),
                })
            },
        )
        .unwrap();
        assert_eq!(result.coverage.len(), 8);
        assert!(
            result
                .coverage
                .iter()
                .all(|row| row.state == CoverageStateV1::Complete)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn selected_response_anchors_saved_row_then_reports_runtime_change() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions(id TEXT PRIMARY KEY,project_id TEXT,status TEXT,is_eval INTEGER,
                parent_id TEXT,continued_from TEXT,session_kind TEXT,provider TEXT,updated_at TEXT,
                title TEXT,query TEXT,model TEXT);
             CREATE TABLE conversation_events(id INTEGER PRIMARY KEY,session_id TEXT,sequence INTEGER);",
        )
        .unwrap();
        let project = Uuid::new_v4();
        let session = Uuid::new_v4();
        let now = Utc::now();
        conn.execute(
            "INSERT INTO sessions(id,project_id,status,is_eval,session_kind,provider,updated_at,title,query,model)
             VALUES(?1,?2,'Running',0,'Standard','Codex',?3,'Selected session','question','model')",
            params![
                session.to_string(),
                project.to_string(),
                now.to_rfc3339_opts(SecondsFormat::Nanos, true)
            ],
        )
        .unwrap();
        assert!(matches!(
            selected_saved_session(&conn, project, session),
            Err(ReadError::InvalidSource)
        ));
        let request = RemoteGetSessionV1 {
            project_id: WireUuid::new(project.to_string()).unwrap(),
            session_id: WireUuid::new(session.to_string()).unwrap(),
        };
        let capture_at = Utc::now();
        let candidate = RuntimeSessionCandidate {
            id: session,
            project_id: Some(project),
            status: SessionStatus::Running,
            is_eval: false,
            updated_at_seconds: capture_at.timestamp(),
            updated_at_nanosecond: capture_at.timestamp_subsec_nanos(),
            spawn_generation: Some(3),
        };
        let capture = RuntimeSessionCandidateSnapshot {
            active: vec![candidate],
            active_observed_at: capture_at,
            completed: vec![],
            completed_observed_at: capture_at,
        };
        let tx = conn.unchecked_transaction().unwrap();
        let saved = selected_saved_session(&tx, project, session).unwrap();
        tx.rollback().unwrap();
        let after_at = Utc::now();
        let recheck = RuntimeSessionRecheckObservation {
            state: RuntimeSessionRecheck::NoObservedChange,
            active_observed_at: after_at,
            completed_observed_at: after_at,
        };
        let pending: Vec<_> = [
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
        .map(|(index, source)| SourceCoverageV1 {
            source,
            state: CoverageStateV1::Complete,
            has_more: false,
            lower_bound: DecimalU64::new("0".into()).unwrap(),
            observed_at: Timestamp::new(after_at.to_rfc3339_opts(SecondsFormat::Nanos, true))
                .unwrap(),
            observation_order: index as u32 + 1,
        })
        .collect();
        let complete = finish_selected_session_response(
            &request,
            saved,
            SelectedSessionSources {
                runtime_capture: RuntimeSessionObservation::Observed(&capture),
                runtime_reread: Some(RuntimeSessionObservation::Observed(&recheck)),
                runtime_sequences: vec![],
                pending_coverage: pending.clone(),
                live_signals_lower_bound: 0,
            },
            Uuid::new_v4(),
            after_at,
        )
        .unwrap();
        assert!(complete.complete);
        assert_eq!(complete.coverage[0].lower_bound.get(), 1);
        assert_eq!(complete.item.summary.id, request.session_id);

        let tx = conn.unchecked_transaction().unwrap();
        let saved = selected_saved_session(&tx, project, session).unwrap();
        tx.rollback().unwrap();
        let changed_at = Utc::now();
        let changed = RuntimeSessionRecheckObservation {
            state: RuntimeSessionRecheck::Changed,
            active_observed_at: changed_at,
            completed_observed_at: changed_at,
        };
        let uncertain = finish_selected_session_response(
            &request,
            saved,
            SelectedSessionSources {
                runtime_capture: RuntimeSessionObservation::Observed(&capture),
                runtime_reread: Some(RuntimeSessionObservation::Observed(&changed)),
                runtime_sequences: vec![],
                pending_coverage: pending,
                live_signals_lower_bound: 0,
            },
            Uuid::new_v4(),
            changed_at,
        )
        .unwrap();
        assert!(!uncertain.complete);
        assert_eq!(uncertain.degraded, vec![DegradationV1::Unavailable]);
        assert_eq!(uncertain.coverage[0].lower_bound.get(), 0);

        let tx = conn.unchecked_transaction().unwrap();
        let saved = selected_saved_session(&tx, project, session).unwrap();
        tx.rollback().unwrap();
        let envelope_at = saved.observed_at;
        let later = RuntimeSessionRecheckObservation {
            state: RuntimeSessionRecheck::NoObservedChange,
            active_observed_at: envelope_at + Duration::milliseconds(1),
            completed_observed_at: envelope_at + Duration::milliseconds(1),
        };
        assert!(matches!(
            finish_selected_session_response(
                &request,
                saved,
                SelectedSessionSources {
                    runtime_capture: RuntimeSessionObservation::Observed(&capture),
                    runtime_reread: Some(RuntimeSessionObservation::Observed(&later)),
                    runtime_sequences: vec![],
                    pending_coverage: complete.item.pending_coverage.clone(),
                    live_signals_lower_bound: 0,
                },
                Uuid::new_v4(),
                envelope_at,
            ),
            Err(ReadError::InvalidSource)
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn runtime_only_detail_requires_exact_store_miss_and_unchanged_runtime() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions(id TEXT PRIMARY KEY,project_id TEXT);
             CREATE TABLE conversation_events(id INTEGER PRIMARY KEY,session_id TEXT,sequence INTEGER);
             CREATE INDEX idx_events_session_sequence ON conversation_events(session_id,sequence);",
        )
            .unwrap();
        let project = Uuid::new_v4();
        let session = Uuid::new_v4();
        let request = RemoteGetSessionV1 {
            project_id: WireUuid::new(project.to_string()).unwrap(),
            session_id: WireUuid::new(session.to_string()).unwrap(),
        };
        let captured_at = Utc::now();
        let candidate = RuntimeSessionCandidate {
            id: session,
            project_id: Some(project),
            status: SessionStatus::Running,
            is_eval: false,
            updated_at_seconds: captured_at.timestamp(),
            updated_at_nanosecond: captured_at.timestamp_subsec_nanos(),
            spawn_generation: Some(7),
        };
        let runtime = || SelectedRuntimeSession {
            project,
            session,
            origin: SessionCandidateOrigin::Active,
            candidate,
            row: SessionRow {
                id: session,
                parent_id: None,
                continued_from: None,
                kind: "Standard".into(),
                provider: "Codex".into(),
                status: "Running".into(),
                updated_at: captured_at.to_rfc3339_opts(SecondsFormat::Nanos, true),
                own_title: preview("Runtime session", 512),
            },
            detail: SessionDetailSource {
                own_title: preview("Runtime session", 4096),
                query: Some(preview("question", 4096)),
                model: None,
                saved_history_head: None,
            },
            runtime_sequence: None,
            captured_at,
        };
        let before_at = Utc::now();
        let before = RuntimeSessionCandidateSnapshot {
            active: vec![candidate],
            active_observed_at: before_at,
            completed: vec![],
            completed_observed_at: before_at,
        };
        let pending_capture = capture_runtime_only_pending_sources(
            project,
            session,
            |project, session| {
                let at = Utc::now();
                Ok(RuntimeQuestionSlotListSnapshot {
                    project,
                    session,
                    active_observed_at: at,
                    completed_observed_at: at,
                    active_generation: Some(7),
                    completed_found: false,
                    slots: vec![],
                })
            },
            |session| {
                Ok(NativeRuntimeApprovalListSnapshot {
                    session,
                    observed_at: Utc::now(),
                    state: NativeRuntimeApprovalListState::Missing,
                })
            },
        )
        .unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        let selected = selected_runtime_store_miss(&tx, project, session, runtime()).unwrap();
        tx.rollback().unwrap();
        let mut reread = runtime();
        reread.captured_at = Utc::now();
        assert!(selected.matches_runtime_reread(&reread));
        reread.candidate.spawn_generation = Some(8);
        assert!(!selected.matches_runtime_reread(&reread));
        reread.candidate.spawn_generation = Some(7);
        reread.runtime_sequence = Some(SequenceObservationV1 {
            source: SourceV1::ActiveSessions,
            sequence: Some(1),
            event_id: None,
        });
        assert!(!selected.matches_runtime_reread(&reread));
        let pending = finish_runtime_only_pending_sources(
            &selected,
            pending_capture,
            |_| {
                let at = Utc::now();
                Ok(RuntimeQuestionSlotListRecheckObservation {
                    state: RuntimeQuestionSlotRecheck::NoObservedChange,
                    active_observed_at: at,
                    completed_observed_at: at,
                })
            },
            |_| {
                Ok(NativeRuntimeApprovalListRecheckObservation {
                    state: NativeRuntimeApprovalRecheck::Changed,
                    observed_at: Utc::now(),
                })
            },
        )
        .unwrap()
        .coverage;
        assert_eq!(pending[0].state, CoverageStateV1::Unavailable);
        assert_eq!(pending[1].state, CoverageStateV1::Complete);
        assert_eq!(pending[2].state, CoverageStateV1::Complete);
        assert_eq!(pending[4].state, CoverageStateV1::Unavailable);
        let after_at = Utc::now();
        let after = RuntimeSessionRecheckObservation {
            state: RuntimeSessionRecheck::NoObservedChange,
            active_observed_at: after_at,
            completed_observed_at: after_at,
        };
        let response = finish_runtime_only_session_response(
            &request,
            selected,
            SelectedSessionSources {
                runtime_capture: RuntimeSessionObservation::Observed(&before),
                runtime_reread: Some(RuntimeSessionObservation::Observed(&after)),
                runtime_sequences: vec![],
                pending_coverage: pending.clone(),
                live_signals_lower_bound: 0,
            },
            Uuid::new_v4(),
            Utc::now(),
        )
        .unwrap();
        assert!(!response.complete);
        assert_eq!(response.coverage[0].lower_bound.get(), 1);
        assert_eq!(response.coverage[2].lower_bound.get(), 0);
        assert_eq!(response.item.sequences[0].source, SourceV1::StoreHistory);
        assert!(response.item.sequences[0].sequence.is_none());

        let native_id = Uuid::new_v4();
        let incarnation = Uuid::new_v4();
        let page_capture = capture_runtime_only_pending_sources(
            project,
            session,
            |project, session| {
                let at = Utc::now();
                Ok(RuntimeQuestionSlotListSnapshot {
                    project,
                    session,
                    active_observed_at: at,
                    completed_observed_at: at,
                    active_generation: Some(7),
                    completed_found: false,
                    slots: vec![],
                })
            },
            |session| {
                Ok(NativeRuntimeApprovalListSnapshot {
                    session,
                    observed_at: Utc::now(),
                    state: NativeRuntimeApprovalListState::Present(vec![
                        crate::remote_read::NativeRuntimeApprovalRow {
                            id: native_id,
                            incarnation_id: incarnation,
                            spawn_generation: 7,
                            method: Some(preview("approval", 512)),
                            description: None,
                            resolution_observed: false,
                            resolution_persisted: false,
                            writer_live: true,
                            writer_capacity: 1,
                        },
                    ]),
                })
            },
        )
        .unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        let page_selected = selected_runtime_store_miss(&tx, project, session, runtime()).unwrap();
        let prepared = page_capture
            .prepare_page(&tx, &page_selected, None, 1)
            .unwrap();
        tx.rollback().unwrap();
        let page_sources = finish_runtime_only_pending_sources(
            &page_selected,
            page_capture,
            |_| {
                let at = Utc::now();
                Ok(RuntimeQuestionSlotListRecheckObservation {
                    state: RuntimeQuestionSlotRecheck::NoObservedChange,
                    active_observed_at: at,
                    completed_observed_at: at,
                })
            },
            |_| {
                Ok(NativeRuntimeApprovalListRecheckObservation {
                    state: NativeRuntimeApprovalRecheck::NoObservedChange,
                    observed_at: Utc::now(),
                })
            },
        )
        .unwrap();
        let page = prepared.finish(&page_sources).unwrap();
        assert_eq!(page.projection.items.len(), 1);
        assert_eq!(
            page.projection.items[0].id.as_str(),
            format!("native:{native_id}")
        );
        assert_eq!(page_sources.coverage[0].state, CoverageStateV1::Unavailable);
        assert_eq!(page_sources.coverage[4].state, CoverageStateV1::Complete);
        let native_selected = DecisionId::new(format!("native:{native_id}")).unwrap();
        assert!(matches!(
            crate::remote_read::selected_runtime_only_decision(
                &native_selected, project, session, &page_sources,
            ),
            Ok(SelectedDecisionV1::Present { decision, stale: false }) if decision.id == native_selected
        ));
        let durable_selected = DecisionId::new(format!("question:{}", Uuid::new_v4())).unwrap();
        assert!(matches!(
            crate::remote_read::selected_runtime_only_decision(
                &durable_selected, project, session, &page_sources,
            ),
            Ok(SelectedDecisionV1::Unavailable { id, reason: DegradationV1::Unavailable, .. }) if id == durable_selected
        ));

        let tx = conn.unchecked_transaction().unwrap();
        let selected = selected_runtime_store_miss(&tx, project, session, runtime()).unwrap();
        tx.rollback().unwrap();
        let changed = RuntimeSessionRecheckObservation {
            state: RuntimeSessionRecheck::Changed,
            ..after
        };
        assert!(matches!(
            finish_runtime_only_session_response(
                &request,
                selected,
                SelectedSessionSources {
                    runtime_capture: RuntimeSessionObservation::Observed(&before),
                    runtime_reread: Some(RuntimeSessionObservation::Observed(&changed)),
                    runtime_sequences: vec![],
                    pending_coverage: pending,
                    live_signals_lower_bound: 0,
                },
                Uuid::new_v4(),
                Utc::now(),
            ),
            Err(ReadError::SourceUnavailable)
        ));
        conn.execute(
            "INSERT INTO conversation_events(id,session_id,sequence) VALUES(1,?1,1)",
            [session.to_string()],
        )
        .unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        assert!(matches!(
            selected_runtime_store_miss(&tx, project, session, runtime()),
            Err(ReadError::SourceUnavailable)
        ));
        let foreign_conn = Connection::open_in_memory().unwrap();
        foreign_conn
            .execute_batch(
                "CREATE TABLE sessions(id TEXT PRIMARY KEY,project_id TEXT);
                 CREATE TABLE conversation_events(id INTEGER PRIMARY KEY,session_id TEXT,sequence INTEGER);",
            )
            .unwrap();
        foreign_conn
            .execute(
                "INSERT INTO sessions(id,project_id) VALUES(?1,?2)",
                params![session.to_string(), Uuid::new_v4().to_string()],
            )
            .unwrap();
        let tx = foreign_conn.unchecked_transaction().unwrap();
        assert!(matches!(
            selected_runtime_store_miss(&tx, project, session, runtime()),
            Err(ReadError::SourceUnavailable)
        ));
    }
}
