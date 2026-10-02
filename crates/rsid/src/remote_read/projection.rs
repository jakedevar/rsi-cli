use super::pending::{parse_json_limited, valid_candidates};
use super::{
    BoundedText, DecisionsPositionV1, HistoryRow, NativeRuntimeApprovalListRecheckObservation,
    NativeRuntimeApprovalListSnapshot, NativeRuntimeApprovalListState,
    NativeRuntimeApprovalRecheck, NativeRuntimeApprovalSnapshot, NativeRuntimeApprovalState,
    PendingCandidate, PendingKey, PendingSource, PendingSourceRow, PendingUnionCandidate,
    PendingUnionSelection, ProjectRow, QuestionSlotGeneration, QuestionSlotMirror, ReadError,
    Result, SelectedPendingSource, SelectedRuntimeOnlySession, SelectedSavedSession,
    SessionDetailSource, SessionRow, SourcePage, durable_question_fallback,
    pending_candidate_hydrate, pending_source_key_page, require_session_project,
    select_pending_union_keys,
};
use chrono::{DateTime, Utc};
use rsi_common::remote_read::{
    AttentionV1, ClosureStateV1, ContentStateV1, CoverageStateV1, CursorV1, DecimalI64, DecimalU64,
    DecisionDisplayV1, DecisionId, DecisionKindV1, DecisionModeV1, DecisionSourceObservationV1,
    DecisionSummaryV1, DecisionsResponseV1, DegradationV1, DeliveryStateV1, DisplayFieldV1,
    DisplaySourceFieldV1, EventKindV1, HistoryEventV1, IdentityClassV1, KnownEventKindV1,
    KnownProviderV1, KnownPublicationStateV1, KnownRoleV1, KnownSessionKindV1,
    KnownSessionStatusV1, PairingStateV1, PreviewStateV1, ProjectV1, ProjectionLimitsV1,
    ProviderV1, PublicationStateV1, QuestionOptionV1, QuestionV1, ReadResponseV1,
    RetainedDecisionDisplayV1, RoleV1, SelectedDecisionV1, SequenceObservationV1, SessionDetailV1,
    SessionKindV1, SessionStatusV1, SessionSummaryV1, SourceCoverageV1, SourceExtentV1, SourceV1,
    Text, Timestamp, WireDocumentV1, WireUuid, unavailable_label,
};
use rsi_common::types::PendingQuestion;
use rusqlite::{Connection, ErrorCode};
use serde::de::DeserializeOwned;
use serde_json::Value;
use uuid::Uuid;

fn wire_uuid(id: Uuid) -> Result<WireUuid> {
    WireUuid::new(id.to_string()).map_err(|_| ReadError::InvalidSource)
}

fn known_label<T: DeserializeOwned>(value: &str) -> Option<T> {
    serde_json::from_value(Value::String(value.to_owned())).ok()
}

pub fn project(row: ProjectRow) -> Result<ProjectV1> {
    Ok(ProjectV1 {
        id: wire_uuid(row.id)?,
        name: Text::<512>::new(row.name.text).map_err(|_| ReadError::InvalidSource)?,
    })
}

const PENDING_SOURCES: [SourceV1; 8] = [
    SourceV1::QuestionPublications,
    SourceV1::TrackedQuestionSlot,
    SourceV1::SessionQuestionSlot,
    SourceV1::DurableQuestionFallback,
    SourceV1::NativeRuntime,
    SourceV1::NativePublications,
    SourceV1::NativeHistoricalFallback,
    SourceV1::LegacyApprovals,
];

/// A bounded source attempt. Refused reads carry no zero-result claim.
#[derive(Clone, Copy)]
pub enum PendingRead<T> {
    Observed { value: T, at: DateTime<Utc> },
    Busy { at: DateTime<Utc> },
    Unavailable { at: DateTime<Utc> },
}

type PendingKeyPage = SourcePage<PendingCandidate, PendingKey>;

/// Five bounded Store attempts. The two runtime captures supply the other
/// three coverage rows after the caller performs its post-Store rereads.
pub struct PendingStoreSources {
    pub questions: PendingRead<PendingKeyPage>,
    pub fallback: PendingRead<Option<PendingSourceRow>>,
    pub native_publications: PendingRead<PendingKeyPage>,
    pub native_historical: PendingRead<PendingKeyPage>,
    pub legacy: PendingRead<PendingKeyPage>,
}

fn pending_attempt<T>(read: impl FnOnce() -> Result<T>) -> Result<PendingRead<T>> {
    let result = read();
    let at = Utc::now();
    match result {
        Ok(value) => Ok(PendingRead::Observed { value, at }),
        Err(ReadError::Busy)
        | Err(ReadError::Sql(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked,
                ..
            },
            _,
        ))) => Ok(PendingRead::Busy { at }),
        Err(ReadError::SourceUnavailable) => Ok(PendingRead::Unavailable { at }),
        Err(error) => Err(error),
    }
}

/// Attempt all five Store pending sources under the caller's read context.
/// Scope is checked first; a later NotFound or malformed source remains an
/// error, never an empty Complete result. Runtime acquisition is separate.
pub fn pending_store_sources(
    conn: &Connection,
    project: Uuid,
    session: Uuid,
    after: [Option<PendingKey>; 4],
    limit: usize,
) -> Result<PendingStoreSources> {
    pending_store_sources_inner(conn, project, session, after, limit, None)
}

/// Observe the exact durable fallback before applying a cursor whose runtime
/// witness binds its presence. The owner keeps this value in the same Store
/// transaction and passes it to `pending_store_sources_with_fallback`.
pub(super) fn pending_store_fallback(
    conn: &Connection,
    project: Uuid,
    session: Uuid,
) -> Result<PendingRead<Option<PendingSourceRow>>> {
    require_session_project(conn, project, session)?;
    pending_attempt(|| durable_question_fallback(conn, project, session))
}

pub(super) fn pending_store_sources_with_fallback(
    conn: &Connection,
    project: Uuid,
    session: Uuid,
    after: [Option<PendingKey>; 4],
    limit: usize,
    fallback: PendingRead<Option<PendingSourceRow>>,
) -> Result<PendingStoreSources> {
    pending_store_sources_inner(conn, project, session, after, limit, Some(fallback))
}

fn pending_store_sources_inner(
    conn: &Connection,
    project: Uuid,
    session: Uuid,
    after: [Option<PendingKey>; 4],
    limit: usize,
    fallback: Option<PendingRead<Option<PendingSourceRow>>>,
) -> Result<PendingStoreSources> {
    if !(1..=32).contains(&limit) {
        return Err(ReadError::ResourceLimit);
    }
    require_session_project(conn, project, session)?;
    Ok(PendingStoreSources {
        questions: pending_attempt(|| {
            pending_source_key_page(
                conn,
                project,
                session,
                PendingSource::Questions,
                after[0],
                limit,
            )
        })?,
        fallback: match fallback {
            Some(value) => value,
            None => pending_attempt(|| durable_question_fallback(conn, project, session))?,
        },
        native_publications: pending_attempt(|| {
            pending_source_key_page(
                conn,
                project,
                session,
                PendingSource::NativePublications,
                after[1],
                limit,
            )
        })?,
        native_historical: pending_attempt(|| {
            pending_source_key_page(
                conn,
                project,
                session,
                PendingSource::NativeHistorical,
                after[2],
                limit,
            )
        })?,
        legacy: pending_attempt(|| {
            pending_source_key_page(
                conn,
                project,
                session,
                PendingSource::LegacyApprovals,
                after[3],
                limit,
            )
        })?,
    })
}

fn borrow_pending_read<T>(read: &PendingRead<T>) -> PendingRead<&T> {
    match read {
        PendingRead::Observed { value, at } => PendingRead::Observed { value, at: *at },
        PendingRead::Busy { at } => PendingRead::Busy { at: *at },
        PendingRead::Unavailable { at } => PendingRead::Unavailable { at: *at },
    }
}

fn pending_read_at<T>(read: &PendingRead<T>) -> DateTime<Utc> {
    match read {
        PendingRead::Observed { at, .. }
        | PendingRead::Busy { at }
        | PendingRead::Unavailable { at } => *at,
    }
}

impl PendingStoreSources {
    /// Produce all eight coverage rows from attempted Store reads and the
    /// caller's generation-fenced runtime observations. Equal rereads mean
    /// no observed scalar change, never a stable cross-source snapshot.
    pub fn coverage(
        &self,
        question_slots: PendingRead<(&RuntimeQuestionSlotListSnapshot, RuntimeQuestionSlotRecheck)>,
        native_runtime: PendingRead<(
            &NativeRuntimeApprovalListSnapshot,
            NativeRuntimeApprovalRecheck,
        )>,
    ) -> Result<Vec<SourceCoverageV1>> {
        let fallback = match &self.fallback {
            PendingRead::Observed { value, at } => PendingRead::Observed {
                value: value.as_ref(),
                at: *at,
            },
            PendingRead::Busy { at } => PendingRead::Busy { at: *at },
            PendingRead::Unavailable { at } => PendingRead::Unavailable { at: *at },
        };
        pending_source_coverage(PendingCoverageInputs {
            questions: borrow_pending_read(&self.questions),
            question_slots,
            fallback,
            native_runtime,
            native_publications: borrow_pending_read(&self.native_publications),
            native_historical: borrow_pending_read(&self.native_historical),
            legacy: borrow_pending_read(&self.legacy),
        })
    }
}

fn pending_runtime_recheck<T, R>(
    captured: PendingRead<T>,
    recheck: impl FnOnce(&T) -> Result<(R, DateTime<Utc>)>,
) -> Result<PendingRead<(T, R)>> {
    match captured {
        PendingRead::Observed { value, .. } => match pending_attempt(|| recheck(&value))? {
            PendingRead::Observed {
                value: (checked, at),
                ..
            } => Ok(PendingRead::Observed {
                value: (value, checked),
                at,
            }),
            PendingRead::Busy { at } => Ok(PendingRead::Busy { at }),
            PendingRead::Unavailable { at } => Ok(PendingRead::Unavailable { at }),
        },
        PendingRead::Busy { at } => Ok(PendingRead::Busy { at }),
        PendingRead::Unavailable { at } => Ok(PendingRead::Unavailable { at }),
    }
}

fn borrow_pending_rechecked<T, R: Copy>(read: &PendingRead<(T, R)>) -> PendingRead<(&T, R)> {
    match read {
        PendingRead::Observed {
            value: (snapshot, recheck),
            at,
        } => PendingRead::Observed {
            value: (snapshot, *recheck),
            at: *at,
        },
        PendingRead::Busy { at } => PendingRead::Busy { at: *at },
        PendingRead::Unavailable { at } => PendingRead::Unavailable { at: *at },
    }
}

pub struct PendingAcquiredSources {
    pub store: PendingStoreSources,
    pub question_slots: PendingRead<(RuntimeQuestionSlotListSnapshot, RuntimeQuestionSlotRecheck)>,
    pub native_runtime: PendingRead<(
        NativeRuntimeApprovalListSnapshot,
        NativeRuntimeApprovalRecheck,
    )>,
    pub coverage: Vec<SourceCoverageV1>,
}

/// Runtime pending observations captured before the exact Store miss. Neither
/// runtime source lock is held after this value is returned.
pub struct RuntimeOnlyPendingCapture {
    project: Uuid,
    session: Uuid,
    question_slots: PendingRead<RuntimeQuestionSlotListSnapshot>,
    native_runtime: PendingRead<NativeRuntimeApprovalListSnapshot>,
}

/// Owned runtime observations captured before the saved-session Store read.
/// No runtime guard or Store lock is retained in this value.
pub struct SavedPendingRuntimeCapture {
    inner: RuntimeOnlyPendingCapture,
}

impl SavedPendingRuntimeCapture {
    pub(crate) fn runtime_only(&self) -> &RuntimeOnlyPendingCapture {
        &self.inner
    }

    pub(crate) fn into_runtime_only(self) -> RuntimeOnlyPendingCapture {
        self.inner
    }

    pub fn prepare_page(
        &self,
        conn: &Connection,
        store: &PendingStoreSources,
        previous_after: [Option<PendingKey>; 4],
        start: PendingPagePosition,
        mode: DecisionModeV1,
        limit: usize,
    ) -> Result<PendingPreparedPage> {
        let native = required_observed(&self.inner.native_runtime)?;
        let slots = required_observed(&self.inner.question_slots)?;
        prepare_pending_page(
            conn,
            self.inner.project,
            self.inner.session,
            store,
            native,
            slots,
            previous_after,
            start,
            mode,
            limit,
        )
    }

    /// Resolve a verified signed cursor against the pre-Store runtime witness
    /// and a fallback presence observed in the same Store transaction that
    /// will read the durable key pages. Refused captures never imply empty
    /// runtime lists or a usable cursor.
    pub fn resume_position(
        &self,
        position: &DecisionsPositionV1,
        fallback_present: bool,
    ) -> Result<([Option<PendingKey>; 4], PendingPagePosition)> {
        let native = match &self.inner.native_runtime {
            PendingRead::Observed { value, .. } => value,
            PendingRead::Busy { .. } => return Err(ReadError::Busy),
            PendingRead::Unavailable { .. } => return Err(ReadError::SourceUnavailable),
        };
        let slots = match &self.inner.question_slots {
            PendingRead::Observed { value, .. } => value,
            PendingRead::Busy { .. } => return Err(ReadError::Busy),
            PendingRead::Unavailable { .. } => return Err(ReadError::SourceUnavailable),
        };
        position.resume(native, slots, fallback_present)
    }
}

impl RuntimeOnlyPendingCapture {
    /// Schedule only captured runtime candidates after an exact transactional
    /// Store miss. Empty durable key slices here are a scheduling input; the
    /// eventual coverage still marks all saved-session sources Unavailable.
    pub fn prepare_page(
        &self,
        conn: &Connection,
        selected: &SelectedRuntimeOnlySession,
        position: Option<DecisionsPositionV1>,
        limit: usize,
    ) -> Result<RuntimeOnlyPreparedPage> {
        let (project, session, store_at) = selected.identity();
        if conn.is_autocommit()
            || self.project != project
            || self.session != session
            || !(1..=32).contains(&limit)
            || pending_read_at(&self.question_slots) > store_at
            || pending_read_at(&self.native_runtime) > store_at
        {
            return Err(ReadError::InvalidSource);
        }
        let native = runtime_only_schedule_native(required_observed(&self.native_runtime)?);
        let slots = required_observed(&self.question_slots)?;
        let start = if let Some(position) = position {
            let (previous, start) = position.resume(&native, slots, false)?;
            if previous.iter().any(Option::is_some) {
                return Err(ReadError::StaleCursor);
            }
            start
        } else {
            PendingPagePosition {
                examined: [0; 5],
                key_bucket: 0,
                slot_offset: 0,
                page_bucket: 0,
            }
        };
        let empty: SourcePage<PendingCandidate, PendingKey> = SourcePage {
            items: Vec::new(),
            next: None,
            has_more: false,
        };
        let selection = select_pending_page_keys(
            PendingPageInputs {
                runtime: &native,
                runtime_recheck: NativeRuntimeApprovalRecheck::NoObservedChange,
                questions: &[],
                native_publications: &[],
                native_historical: &[],
                legacy: &[],
                question_slots: slots,
                question_slots_recheck: RuntimeQuestionSlotRecheck::NoObservedChange,
                fallback: None,
            },
            start,
            limit,
        )?;
        let next_position = DecisionsPositionV1::after_page(
            [None; 4],
            &selection,
            [&empty, &empty, &empty, &empty],
            &native,
            slots,
            false,
        )?;
        Ok(RuntimeOnlyPreparedPage {
            selection,
            next_position,
            store_observed_at: store_at,
        })
    }
}

/// Copy saved pending sources and the exact selected row in one bounded Store
/// transaction. The caller releases that transaction before finishing.
pub fn selected_saved_pending_sources(
    conn: &Connection,
    project: Uuid,
    session: Uuid,
    after: [Option<PendingKey>; 4],
    limit: usize,
) -> Result<(PendingStoreSources, SelectedSavedSession)> {
    if conn.is_autocommit() {
        return Err(ReadError::InvalidSource);
    }
    let store = pending_store_sources(conn, project, session, after, limit)?;
    let selected = super::selected_saved_session(conn, project, session)?;
    Ok((store, selected))
}

pub fn capture_saved_pending_runtime_sources(
    project: Uuid,
    session: Uuid,
    capture_question_slots: impl FnOnce(Uuid, Uuid) -> Result<RuntimeQuestionSlotListSnapshot>,
    capture_native: impl FnOnce(Uuid) -> Result<NativeRuntimeApprovalListSnapshot>,
) -> Result<SavedPendingRuntimeCapture> {
    Ok(SavedPendingRuntimeCapture {
        inner: capture_runtime_only_pending_sources(
            project,
            session,
            capture_question_slots,
            capture_native,
        )?,
    })
}

/// Called after the bounded Store transaction is released. `selected` and
/// `store` must have been copied inside that same transaction, with `selected`
/// last so its timestamp follows every Store pending attempt.
pub fn finish_saved_pending_sources(
    selected: &SelectedSavedSession,
    capture: SavedPendingRuntimeCapture,
    store: PendingStoreSources,
    recheck_question_slots: impl FnOnce(
        &RuntimeQuestionSlotListSnapshot,
    ) -> Result<RuntimeQuestionSlotListRecheckObservation>,
    recheck_native: impl FnOnce(
        &NativeRuntimeApprovalListSnapshot,
    ) -> Result<NativeRuntimeApprovalListRecheckObservation>,
) -> Result<PendingAcquiredSources> {
    let (project, session, store_at) = selected.identity();
    let RuntimeOnlyPendingCapture {
        project: captured_project,
        session: captured_session,
        question_slots,
        native_runtime,
    } = capture.inner;
    if project != captured_project || session != captured_session {
        return Err(ReadError::InvalidSource);
    }
    if pending_read_at(&question_slots) > store_at || pending_read_at(&native_runtime) > store_at {
        return Err(ReadError::InvalidSource);
    }
    for at in [
        pending_read_at(&store.questions),
        pending_read_at(&store.fallback),
        pending_read_at(&store.native_publications),
        pending_read_at(&store.native_historical),
        pending_read_at(&store.legacy),
    ] {
        if at > store_at {
            return Err(ReadError::InvalidSource);
        }
    }
    let question_slots = pending_runtime_recheck(question_slots, |before| {
        let observation = recheck_question_slots(before)?;
        if observation.active_observed_at < store_at || observation.completed_observed_at < store_at
        {
            return Err(ReadError::InvalidSource);
        }
        Ok((
            observation.state,
            observation
                .active_observed_at
                .max(observation.completed_observed_at),
        ))
    })?;
    let native_runtime = pending_runtime_recheck(native_runtime, |before| {
        let observation = recheck_native(before)?;
        if observation.observed_at < store_at {
            return Err(ReadError::InvalidSource);
        }
        Ok((observation.state, observation.observed_at))
    })?;
    let coverage = store.coverage(
        borrow_pending_rechecked(&question_slots),
        borrow_pending_rechecked(&native_runtime),
    )?;
    Ok(PendingAcquiredSources {
        store,
        question_slots,
        native_runtime,
        coverage,
    })
}

pub struct RuntimeOnlyPendingSources {
    pub question_slots: PendingRead<(RuntimeQuestionSlotListSnapshot, RuntimeQuestionSlotRecheck)>,
    pub native_runtime: PendingRead<(
        NativeRuntimeApprovalListSnapshot,
        NativeRuntimeApprovalRecheck,
    )>,
    pub coverage: Vec<SourceCoverageV1>,
}

pub fn capture_runtime_only_pending_sources(
    project: Uuid,
    session: Uuid,
    capture_question_slots: impl FnOnce(Uuid, Uuid) -> Result<RuntimeQuestionSlotListSnapshot>,
    capture_native: impl FnOnce(Uuid) -> Result<NativeRuntimeApprovalListSnapshot>,
) -> Result<RuntimeOnlyPendingCapture> {
    let question_slots = pending_attempt(|| capture_question_slots(project, session))?;
    if matches!(&question_slots, PendingRead::Observed { value, .. } if value.project != project || value.session != session)
    {
        return Err(ReadError::InvalidSource);
    }
    let native_runtime = pending_attempt(|| capture_native(session))?;
    if matches!(&native_runtime, PendingRead::Observed { value, .. } if value.session != session) {
        return Err(ReadError::InvalidSource);
    }
    Ok(RuntimeOnlyPendingCapture {
        project,
        session,
        question_slots,
        native_runtime,
    })
}

/// After the Store transaction has been released, reread both runtime
/// sources. Durable pending sources remain Unavailable for a runtime-only
/// session: the saved-session-scoped readers cannot prove an empty set.
pub fn finish_runtime_only_pending_sources(
    selected: &SelectedRuntimeOnlySession,
    capture: RuntimeOnlyPendingCapture,
    recheck_question_slots: impl FnOnce(
        &RuntimeQuestionSlotListSnapshot,
    ) -> Result<RuntimeQuestionSlotListRecheckObservation>,
    recheck_native: impl FnOnce(
        &NativeRuntimeApprovalListSnapshot,
    ) -> Result<NativeRuntimeApprovalListRecheckObservation>,
) -> Result<RuntimeOnlyPendingSources> {
    let (project, session, store_at) = selected.identity();
    if capture.project != project || capture.session != session {
        return Err(ReadError::InvalidSource);
    }
    for at in [
        match &capture.question_slots {
            PendingRead::Observed { at, .. }
            | PendingRead::Busy { at }
            | PendingRead::Unavailable { at } => *at,
        },
        match &capture.native_runtime {
            PendingRead::Observed { at, .. }
            | PendingRead::Busy { at }
            | PendingRead::Unavailable { at } => *at,
        },
    ] {
        if at > store_at {
            return Err(ReadError::InvalidSource);
        }
    }
    let question_was_observed = matches!(&capture.question_slots, PendingRead::Observed { .. });
    let native_was_observed = matches!(&capture.native_runtime, PendingRead::Observed { .. });
    let question_slots = pending_runtime_recheck(capture.question_slots, |before| {
        let observation = recheck_question_slots(before)?;
        if observation.active_observed_at < store_at || observation.completed_observed_at < store_at
        {
            return Err(ReadError::InvalidSource);
        }
        Ok((
            observation.state,
            observation
                .active_observed_at
                .max(observation.completed_observed_at),
        ))
    })?;
    let native_runtime = pending_runtime_recheck(capture.native_runtime, |before| {
        let observation = recheck_native(before)?;
        if observation.observed_at < store_at {
            return Err(ReadError::InvalidSource);
        }
        Ok((observation.state, observation.observed_at))
    })?;
    for (was_observed, at) in [
        (
            question_was_observed,
            match &question_slots {
                PendingRead::Observed { at, .. }
                | PendingRead::Busy { at }
                | PendingRead::Unavailable { at } => *at,
            },
        ),
        (
            native_was_observed,
            match &native_runtime {
                PendingRead::Observed { at, .. }
                | PendingRead::Busy { at }
                | PendingRead::Unavailable { at } => *at,
            },
        ),
    ] {
        if was_observed && at < store_at {
            return Err(ReadError::InvalidSource);
        }
    }
    let coverage = pending_source_coverage(PendingCoverageInputs {
        questions: PendingRead::Unavailable { at: store_at },
        question_slots: borrow_pending_rechecked(&question_slots),
        fallback: PendingRead::Unavailable { at: store_at },
        native_runtime: borrow_pending_rechecked(&native_runtime),
        native_publications: PendingRead::Unavailable { at: store_at },
        native_historical: PendingRead::Unavailable { at: store_at },
        legacy: PendingRead::Unavailable { at: store_at },
    })?;
    Ok(RuntimeOnlyPendingSources {
        question_slots,
        native_runtime,
        coverage,
    })
}

/// Fixture helper for source coverage tests. Production owners use the three
/// separate capture, Store and finish calls so Store locks cannot span runtime
/// observations.
#[cfg(test)]
pub fn acquire_pending_sources(
    conn: &Connection,
    project: Uuid,
    session: Uuid,
    after: [Option<PendingKey>; 4],
    limit: usize,
    capture_question_slots: impl FnOnce(Uuid, Uuid) -> Result<RuntimeQuestionSlotListSnapshot>,
    capture_native: impl FnOnce(Uuid) -> Result<NativeRuntimeApprovalListSnapshot>,
    recheck_question_slots: impl FnOnce(
        &RuntimeQuestionSlotListSnapshot,
    ) -> Result<RuntimeQuestionSlotListRecheckObservation>,
    recheck_native: impl FnOnce(
        &NativeRuntimeApprovalListSnapshot,
    ) -> Result<NativeRuntimeApprovalListRecheckObservation>,
) -> Result<PendingAcquiredSources> {
    if !(1..=32).contains(&limit) {
        return Err(ReadError::ResourceLimit);
    }
    require_session_project(conn, project, session)?;
    let question_slots = pending_attempt(|| capture_question_slots(project, session))?;
    if let PendingRead::Observed { value, .. } = &question_slots {
        if value.project != project || value.session != session {
            return Err(ReadError::InvalidSource);
        }
    }
    let native_runtime = pending_attempt(|| capture_native(session))?;
    if let PendingRead::Observed { value, .. } = &native_runtime {
        if value.session != session {
            return Err(ReadError::InvalidSource);
        }
    }
    let store = pending_store_sources(conn, project, session, after, limit)?;
    let question_slots = pending_runtime_recheck(question_slots, |before| {
        recheck_question_slots(before).map(|observation| {
            (
                observation.state,
                observation
                    .active_observed_at
                    .max(observation.completed_observed_at),
            )
        })
    })?;
    let native_runtime = pending_runtime_recheck(native_runtime, |before| {
        recheck_native(before).map(|observation| (observation.state, observation.observed_at))
    })?;
    let coverage = store.coverage(
        borrow_pending_rechecked(&question_slots),
        borrow_pending_rechecked(&native_runtime),
    )?;
    Ok(PendingAcquiredSources {
        store,
        question_slots,
        native_runtime,
        coverage,
    })
}

/// Inputs already acquired under the caller's project and Store guards.
/// These are source observations, not a cross-source transaction.
pub struct PendingCoverageInputs<'a> {
    pub questions: PendingRead<&'a PendingKeyPage>,
    pub question_slots: PendingRead<(
        &'a RuntimeQuestionSlotListSnapshot,
        RuntimeQuestionSlotRecheck,
    )>,
    pub fallback: PendingRead<Option<&'a PendingSourceRow>>,
    pub native_runtime: PendingRead<(
        &'a NativeRuntimeApprovalListSnapshot,
        NativeRuntimeApprovalRecheck,
    )>,
    pub native_publications: PendingRead<&'a PendingKeyPage>,
    pub native_historical: PendingRead<&'a PendingKeyPage>,
    pub legacy: PendingRead<&'a PendingKeyPage>,
}

fn coverage_from_read<T>(
    source: SourceV1,
    order: u32,
    read: PendingRead<T>,
    inspect: impl FnOnce(T) -> Result<(usize, bool, bool)>,
) -> Result<SourceCoverageV1> {
    let (state, count, has_more, at) = match read {
        PendingRead::Observed { value, at } => {
            let (count, has_more, available) = inspect(value)?;
            (
                if !available {
                    CoverageStateV1::Unavailable
                } else if has_more {
                    CoverageStateV1::Limited
                } else {
                    CoverageStateV1::Complete
                },
                if available { count } else { 0 },
                if available { has_more } else { false },
                at,
            )
        }
        PendingRead::Busy { at } => (CoverageStateV1::Busy, 0, false, at),
        PendingRead::Unavailable { at } => (CoverageStateV1::Unavailable, 0, false, at),
    };
    Ok(SourceCoverageV1 {
        source,
        state,
        has_more,
        lower_bound: DecimalU64::new(count.to_string()).map_err(|_| ReadError::InvalidSource)?,
        observed_at: Timestamp::new(at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true))
            .map_err(|_| ReadError::InvalidSource)?,
        observation_order: order,
    })
}

fn coverage_from_key_page(
    source: SourceV1,
    expected: PendingSource,
    read: PendingRead<&PendingKeyPage>,
    order: u32,
) -> Result<SourceCoverageV1> {
    coverage_from_read(source, order, read, |page| {
        if !valid_candidates(&page.items, expected)
            || page.has_more != page.next.is_some()
            || (page.has_more && page.items.is_empty())
        {
            return Err(ReadError::InvalidSource);
        }
        Ok((page.items.len(), page.has_more, true))
    })
}

/// Derive all eight required coverage rows from attempted bounded reads.
/// Missing native writer or changed runtime rereads stay unavailable; a
/// supplied key page with a tail stays limited. Callers still own acquisition,
/// project authorization, response degradation and source-page cursors.
pub fn pending_source_coverage(inputs: PendingCoverageInputs<'_>) -> Result<Vec<SourceCoverageV1>> {
    let mut rows = Vec::with_capacity(8);
    rows.push(coverage_from_key_page(
        SourceV1::QuestionPublications,
        PendingSource::Questions,
        inputs.questions,
        1,
    )?);
    for (source, mirror, order) in [
        (
            SourceV1::TrackedQuestionSlot,
            QuestionSlotMirror::Tracked,
            2,
        ),
        (
            SourceV1::SessionQuestionSlot,
            QuestionSlotMirror::Session,
            3,
        ),
    ] {
        rows.push(coverage_from_read(
            source,
            order,
            inputs.question_slots,
            |(snapshot, recheck)| {
                if snapshot.slots.len() > 3
                    || snapshot.slots.iter().any(|slot| match slot.generation {
                        QuestionSlotGeneration::Spawn(generation) => {
                            snapshot.active_generation != Some(generation)
                        }
                        QuestionSlotGeneration::Completed => {
                            !snapshot.completed_found || slot.mirror == QuestionSlotMirror::Tracked
                        }
                    })
                    || snapshot.slots.iter().enumerate().any(|(index, slot)| {
                        snapshot.slots[index + 1..].iter().any(|other| {
                            slot.generation == other.generation && slot.mirror == other.mirror
                        })
                    })
                {
                    return Err(ReadError::InvalidSource);
                }
                Ok((
                    snapshot
                        .slots
                        .iter()
                        .filter(|slot| slot.mirror == mirror)
                        .count(),
                    false,
                    recheck == RuntimeQuestionSlotRecheck::NoObservedChange,
                ))
            },
        )?);
    }
    rows.push(coverage_from_read(
        SourceV1::DurableQuestionFallback,
        4,
        inputs.fallback,
        |row| {
            if row.is_some_and(|row| row.state != "slot") {
                return Err(ReadError::InvalidSource);
            }
            Ok((usize::from(row.is_some()), false, true))
        },
    )?);
    rows.push(coverage_from_read(
        SourceV1::NativeRuntime,
        5,
        inputs.native_runtime,
        |(snapshot, recheck)| match &snapshot.state {
            NativeRuntimeApprovalListState::Present(items) if items.len() <= 64 => {
                if items.windows(2).any(|pair| pair[0].id >= pair[1].id)
                    || items
                        .iter()
                        .any(|item| !item.writer_live || item.writer_capacity > 64)
                    || items.first().is_some_and(|first| {
                        items.iter().any(|item| {
                            item.incarnation_id != first.incarnation_id
                                || item.spawn_generation != first.spawn_generation
                                || item.writer_capacity != first.writer_capacity
                        })
                    })
                {
                    return Err(ReadError::InvalidSource);
                }
                Ok((
                    items.len(),
                    false,
                    recheck == NativeRuntimeApprovalRecheck::NoObservedChange,
                ))
            }
            NativeRuntimeApprovalListState::Present(_) => Err(ReadError::InvalidSource),
            NativeRuntimeApprovalListState::Missing
            | NativeRuntimeApprovalListState::SourceChanged => Ok((0, false, false)),
        },
    )?);
    for (source, expected, read, order) in [
        (
            SourceV1::NativePublications,
            PendingSource::NativePublications,
            inputs.native_publications,
            6,
        ),
        (
            SourceV1::NativeHistoricalFallback,
            PendingSource::NativeHistorical,
            inputs.native_historical,
            7,
        ),
        (
            SourceV1::LegacyApprovals,
            PendingSource::LegacyApprovals,
            inputs.legacy,
            8,
        ),
    ] {
        rows.push(coverage_from_key_page(source, expected, read, order)?);
    }
    Ok(rows)
}

fn pending_coverage_complete(pending_coverage: &[SourceCoverageV1]) -> Result<bool> {
    if pending_coverage.len() > PENDING_SOURCES.len() {
        return Err(ReadError::InvalidSource);
    }
    let mut seen = [false; PENDING_SOURCES.len()];
    let mut complete = true;
    for coverage in pending_coverage {
        let index = PENDING_SOURCES
            .iter()
            .position(|source| *source == coverage.source)
            .ok_or(ReadError::InvalidSource)?;
        if seen[index] {
            return Err(ReadError::InvalidSource);
        }
        seen[index] = true;
        complete &= coverage.state == CoverageStateV1::Complete && !coverage.has_more;
    }
    Ok(complete && seen.into_iter().all(|present| present))
}

/// Status for one bounded decision response. Complete means all eight source
/// attempts were exhausted and this projection found no uncertainty; it does
/// not assert a cross-source snapshot or confer answer authority.
pub struct PendingResponseStatus {
    pub complete: bool,
    pub degraded: Vec<DegradationV1>,
}

pub fn pending_response_status(
    coverage: &[SourceCoverageV1],
    items: &[DecisionSummaryV1],
    selected: &SelectedDecisionV1,
) -> Result<PendingResponseStatus> {
    if coverage.len() != PENDING_SOURCES.len() || items.len() > 32 {
        return Err(ReadError::InvalidSource);
    }
    let coverage_complete = pending_coverage_complete(coverage)?;
    let mut orders = Vec::with_capacity(coverage.len());
    let mut degraded = Vec::new();
    let mut mark = |reason| {
        if !degraded.contains(&reason) {
            degraded.push(reason);
        }
    };
    for row in coverage {
        if orders.contains(&row.observation_order) {
            return Err(ReadError::InvalidSource);
        }
        orders.push(row.observation_order);
        match row.state {
            CoverageStateV1::Complete if row.has_more => mark(DegradationV1::Limited),
            CoverageStateV1::Complete => {}
            CoverageStateV1::Busy => mark(DegradationV1::Busy),
            CoverageStateV1::Limited => mark(DegradationV1::Limited),
            CoverageStateV1::Unavailable => mark(DegradationV1::Unavailable),
        }
    }
    let mut ids = Vec::with_capacity(items.len());
    for item in items.iter().chain(match selected {
        SelectedDecisionV1::Present {
            decision,
            stale: false,
        } => Some(decision.as_ref()),
        _ => None,
    }) {
        if item.can_answer {
            return Err(ReadError::InvalidSource);
        }
        if item.disagreement {
            mark(DegradationV1::Disagreement);
        }
        match item.details_state {
            PreviewStateV1::Complete => {}
            PreviewStateV1::Truncated => mark(DegradationV1::Truncated),
            PreviewStateV1::Unavailable => mark(DegradationV1::Unavailable),
        }
        if item.omitted_source_observations.get() > 0 || item.omitted_questions.get() > 0 {
            mark(DegradationV1::Truncated);
        }
        for observation in &item.source_observations {
            match observation.state {
                CoverageStateV1::Complete => {}
                CoverageStateV1::Busy => mark(DegradationV1::Busy),
                CoverageStateV1::Limited => mark(DegradationV1::Limited),
                CoverageStateV1::Unavailable => mark(DegradationV1::Unavailable),
            }
        }
    }
    for item in items {
        if ids.contains(&item.id) {
            return Err(ReadError::InvalidSource);
        }
        ids.push(item.id.clone());
    }
    match selected {
        SelectedDecisionV1::None { .. } | SelectedDecisionV1::Present { stale: false, .. } => {}
        SelectedDecisionV1::Present { stale: true, .. } => mark(DegradationV1::Stale),
        SelectedDecisionV1::Tombstone { .. } if !coverage_complete => {
            return Err(ReadError::InvalidSource);
        }
        SelectedDecisionV1::Tombstone { .. } => {}
        SelectedDecisionV1::Unavailable { reason, .. } => {
            if !matches!(
                reason,
                DegradationV1::Busy
                    | DegradationV1::Limited
                    | DegradationV1::Unavailable
                    | DegradationV1::SourceChanged
            ) {
                return Err(ReadError::InvalidSource);
            }
            mark(*reason);
        }
    }
    Ok(PendingResponseStatus {
        complete: coverage_complete && degraded.is_empty(),
        degraded,
    })
}

pub struct PendingResponseInputs {
    pub project: Uuid,
    pub session: Uuid,
    pub daemon_epoch: Uuid,
    pub observed_at: DateTime<Utc>,
    pub mode: DecisionModeV1,
    pub page_limit: u32,
    pub items: Vec<DecisionSummaryV1>,
    pub selected: SelectedDecisionV1,
    pub coverage: Vec<SourceCoverageV1>,
    /// More scheduled candidates in the supplied slices, separate from source
    /// page tails reported by coverage. Neither proves a stable snapshot.
    pub page_remaining_in_inputs: bool,
    /// Signed by the RPC owner; this substrate forwards, never mints a token.
    pub next_cursor: Option<CursorV1>,
}

/// Assemble and wire-validate a bounded decisions response. Exhausted source
/// attempts and page positions can establish only observed completeness, not
/// a cross-source transaction or answer authority.
pub fn pending_decisions_response(inputs: PendingResponseInputs) -> Result<DecisionsResponseV1> {
    if !(1..=32).contains(&inputs.page_limit)
        || inputs.items.len() > inputs.page_limit as usize
        || inputs.coverage.iter().any(|row| {
            row.state == CoverageStateV1::Complete && row.has_more
                || matches!(
                    row.state,
                    CoverageStateV1::Busy | CoverageStateV1::Unavailable
                ) && row.has_more
        })
    {
        return Err(ReadError::InvalidSource);
    }
    let has_more =
        inputs.page_remaining_in_inputs || inputs.coverage.iter().any(|row| row.has_more);
    if inputs.next_cursor.is_some() != has_more
        || inputs
            .next_cursor
            .as_ref()
            .is_some_and(|cursor| !matches!(cursor, CursorV1::Decisions { .. }))
    {
        return Err(ReadError::InvalidSource);
    }
    let mut status = pending_response_status(&inputs.coverage, &inputs.items, &inputs.selected)?;
    if inputs.page_remaining_in_inputs {
        status.complete = false;
        if !status.degraded.contains(&DegradationV1::Limited) {
            status.degraded.push(DegradationV1::Limited);
        }
    }
    let response = DecisionsResponseV1 {
        version: Text::<3>::new(rsi_common::remote_read::VERSION.into())
            .map_err(|_| ReadError::InvalidSource)?,
        daemon_epoch: wire_uuid(inputs.daemon_epoch)?,
        observed_at: Timestamp::new(
            inputs
                .observed_at
                .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
        )
        .map_err(|_| ReadError::InvalidSource)?,
        next_cursor: inputs.next_cursor,
        complete: status.complete,
        projection_limits: ProjectionLimitsV1 {
            page_items: inputs.page_limit,
            name_bytes: 512,
            event_text_bytes: 8192,
            decision_text_bytes: 8192,
            item_bytes: 65_536,
            envelope_bytes: 524_288,
        },
        degraded: status.degraded,
        coverage: inputs.coverage,
        project_id: wire_uuid(inputs.project)?,
        session_id: wire_uuid(inputs.session)?,
        mode: inputs.mode,
        items: inputs.items,
        selected: inputs.selected,
    };
    rsi_common::remote_read::encode(&WireDocumentV1::Response(
        ReadResponseV1::RemoteGetDecisionsV1(response.clone()),
    ))
    .map_err(|_| ReadError::InvalidSource)?;
    Ok(response)
}

/// Hydrated rows correspond one-for-one with a bounded key selection. None
/// means a durable occurrence was filtered by the requested view, not that a
/// source or selected identity disappeared. Runtime witnesses remain visible
/// when their durable mirror was filtered. Slot and fallback sources are
/// assembled separately; this page cannot establish eight-source coverage.
pub struct PendingUnionProjection {
    pub items: Vec<DecisionSummaryV1>,
    pub filtered_durable: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendingPageCandidate {
    Key(PendingUnionCandidate),
    RuntimeSlot {
        generation: QuestionSlotGeneration,
        mirror: QuestionSlotMirror,
    },
    DurableFallback,
}

/// Runtime native position is absolute in the captured writer list. Durable
/// positions are relative to the caller's supplied bounded key slices.
/// The caller must sign these positions with its source-page positions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingPagePosition {
    pub examined: [usize; 5],
    pub key_bucket: u8,
    pub slot_offset: usize,
    pub page_bucket: u8,
}

pub struct PendingPageSelection {
    pub items: Vec<PendingPageCandidate>,
    pub next: PendingPagePosition,
    /// Only the supplied slices and captured slot set are exhausted here.
    pub remaining_in_inputs: bool,
}

/// Store-owned payloads copied for selected keys only. `selection` is
/// provisional until both runtime witnesses are reread after Store release.
pub struct PendingPreparedPage {
    selection: PendingPageSelection,
    hydrated: Vec<Option<PendingSourceRow>>,
    next_position: DecisionsPositionV1,
    durable_observed_at: DateTime<Utc>,
}

pub struct PendingFinishedPage {
    pub projection: PendingUnionProjection,
    pub next_position: DecisionsPositionV1,
    pub remaining_in_inputs: bool,
}

pub struct RuntimeOnlyPreparedPage {
    selection: PendingPageSelection,
    next_position: DecisionsPositionV1,
    store_observed_at: DateTime<Utc>,
}

/// An absent or replaced native writer contributes no schedulable runtime
/// candidates. This private copy is only for key selection and cursor offset
/// validation; coverage continues to report the actual source as Unavailable.
fn runtime_only_schedule_native(
    snapshot: &NativeRuntimeApprovalListSnapshot,
) -> NativeRuntimeApprovalListSnapshot {
    let mut scheduling = snapshot.clone();
    if !matches!(scheduling.state, NativeRuntimeApprovalListState::Present(_)) {
        scheduling.state = NativeRuntimeApprovalListState::Present(Vec::new());
    }
    scheduling
}

impl RuntimeOnlyPreparedPage {
    /// Finish only after the runtime-only source rereads have completed with
    /// Store unlocked. Saved-session coverage stays Unavailable in `acquired`.
    pub fn finish(self, acquired: &RuntimeOnlyPendingSources) -> Result<PendingFinishedPage> {
        let (native, native_recheck) = required_observed(&acquired.native_runtime)?;
        let (slots, slots_recheck) = required_observed(&acquired.question_slots)?;
        let native_unavailable =
            !matches!(native.state, NativeRuntimeApprovalListState::Present(_));
        if (*native_recheck != NativeRuntimeApprovalRecheck::NoObservedChange
            && !native_unavailable)
            || *slots_recheck != RuntimeQuestionSlotRecheck::NoObservedChange
            || acquired.coverage.len() != 8
            || native.session != slots.session
        {
            return Err(ReadError::SourceUnavailable);
        }
        if native_unavailable
            && !acquired.coverage.iter().any(|row| {
                row.source == SourceV1::NativeRuntime && row.state == CoverageStateV1::Unavailable
            })
        {
            return Err(ReadError::InvalidSource);
        }
        let scheduling_native = runtime_only_schedule_native(native);
        self.next_position
            .resume(&scheduling_native, slots, false)?;
        let projection = project_pending_page_candidates(
            &self.selection,
            &vec![None; self.selection.items.len()],
            &scheduling_native,
            NativeRuntimeApprovalRecheck::NoObservedChange,
            slots,
            *slots_recheck,
            None,
            Timestamp::new(
                self.store_observed_at
                    .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            )
            .map_err(|_| ReadError::InvalidSource)?,
        )?;
        Ok(PendingFinishedPage {
            projection,
            next_position: self.next_position,
            remaining_in_inputs: self.selection.remaining_in_inputs,
        })
    }
}

impl PendingPreparedPage {
    /// Project only after the owner has released Store and reread both runtime
    /// witnesses. The signed position is also rebound to those captures.
    pub fn finish(self, acquired: &PendingAcquiredSources) -> Result<PendingFinishedPage> {
        let (native, native_recheck) = required_observed(&acquired.native_runtime)?;
        let (slots, slots_recheck) = required_observed(&acquired.question_slots)?;
        if *native_recheck != NativeRuntimeApprovalRecheck::NoObservedChange
            || *slots_recheck != RuntimeQuestionSlotRecheck::NoObservedChange
            || acquired.coverage.len() != 8
        {
            return Err(ReadError::SourceUnavailable);
        }
        let fallback = required_observed(&acquired.store.fallback)?.as_ref();
        self.next_position
            .resume(native, slots, fallback.is_some())?;
        let durable_at = Timestamp::new(
            self.durable_observed_at
                .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
        )
        .map_err(|_| ReadError::InvalidSource)?;
        let projection = project_pending_page_candidates(
            &self.selection,
            &self.hydrated,
            native,
            *native_recheck,
            slots,
            *slots_recheck,
            fallback,
            durable_at,
        )?;
        Ok(PendingFinishedPage {
            projection,
            next_position: self.next_position,
            remaining_in_inputs: self.selection.remaining_in_inputs,
        })
    }
}

fn required_observed<T>(read: &PendingRead<T>) -> Result<&T> {
    match read {
        PendingRead::Observed { value, .. } => Ok(value),
        PendingRead::Busy { .. } => Err(ReadError::Busy),
        PendingRead::Unavailable { .. } => Err(ReadError::SourceUnavailable),
    }
}

/// Schedule a bounded supplied source slice and hydrate only its chosen
/// durable candidates while the Store transaction is held. Passing the
/// `NoObservedChange` variants here is a scheduling precondition only: the
/// caller must compare both actual post-Store rereads before any projection.
#[allow(clippy::too_many_arguments)]
pub fn prepare_pending_page(
    conn: &Connection,
    project: Uuid,
    session: Uuid,
    store: &PendingStoreSources,
    native: &NativeRuntimeApprovalListSnapshot,
    slots: &RuntimeQuestionSlotListSnapshot,
    previous_after: [Option<PendingKey>; 4],
    start: PendingPagePosition,
    mode: DecisionModeV1,
    limit: usize,
) -> Result<PendingPreparedPage> {
    if conn.is_autocommit()
        || !(1..=32).contains(&limit)
        || slots.project != project
        || slots.session != session
        || native.session != session
    {
        return Err(ReadError::InvalidSource);
    }
    let pages = [
        required_observed(&store.questions)?,
        required_observed(&store.native_publications)?,
        required_observed(&store.native_historical)?,
        required_observed(&store.legacy)?,
    ];
    let fallback = required_observed(&store.fallback)?.as_ref();
    let selection = select_pending_page_keys(
        PendingPageInputs {
            runtime: native,
            runtime_recheck: NativeRuntimeApprovalRecheck::NoObservedChange,
            questions: &pages[0].items,
            native_publications: &pages[1].items,
            native_historical: &pages[2].items,
            legacy: &pages[3].items,
            question_slots: slots,
            question_slots_recheck: RuntimeQuestionSlotRecheck::NoObservedChange,
            fallback,
        },
        start,
        limit,
    )?;
    let retained = matches!(mode, DecisionModeV1::Retained);
    let mut hydrated = Vec::with_capacity(selection.items.len());
    for item in &selection.items {
        let candidate = match item {
            PendingPageCandidate::Key(
                PendingUnionCandidate::Question(row) | PendingUnionCandidate::Legacy(row),
            ) => Some(*row),
            PendingPageCandidate::Key(PendingUnionCandidate::Native { durable, .. }) => *durable,
            PendingPageCandidate::RuntimeSlot { .. } | PendingPageCandidate::DurableFallback => {
                None
            }
        };
        hydrated.push(
            candidate
                .map(|key| pending_candidate_hydrate(conn, project, session, key, retained))
                .transpose()?
                .flatten(),
        );
    }
    let next_position = DecisionsPositionV1::after_page(
        previous_after,
        &selection,
        pages,
        native,
        slots,
        fallback.is_some(),
    )?;
    Ok(PendingPreparedPage {
        selection,
        hydrated,
        next_position,
        durable_observed_at: Utc::now(),
    })
}

pub struct PendingPageInputs<'a> {
    pub runtime: &'a NativeRuntimeApprovalListSnapshot,
    pub runtime_recheck: NativeRuntimeApprovalRecheck,
    pub questions: &'a [PendingCandidate],
    pub native_publications: &'a [PendingCandidate],
    pub native_historical: &'a [PendingCandidate],
    pub legacy: &'a [PendingCandidate],
    pub question_slots: &'a RuntimeQuestionSlotListSnapshot,
    pub question_slots_recheck: RuntimeQuestionSlotRecheck,
    pub fallback: Option<&'a PendingSourceRow>,
}

/// Interleave one keyed candidate with one captured slot/fallback candidate.
/// This consumes no payload, makes no whole-source exhaustion claim and does
/// not treat an unavailable runtime source as an empty list.
pub fn select_pending_page_keys(
    inputs: PendingPageInputs<'_>,
    mut position: PendingPagePosition,
    limit: usize,
) -> Result<PendingPageSelection> {
    if inputs.question_slots_recheck != RuntimeQuestionSlotRecheck::NoObservedChange {
        return Err(ReadError::SourceUnavailable);
    }
    if !(1..=32).contains(&limit)
        || position.key_bucket > 2
        || position.page_bucket > 1
        || inputs.runtime.session != inputs.question_slots.session
        || inputs.question_slots.slots.len() > 3
        || inputs
            .question_slots
            .slots
            .iter()
            .any(|slot| match slot.generation {
                QuestionSlotGeneration::Spawn(generation) => {
                    inputs.question_slots.active_generation != Some(generation)
                }
                QuestionSlotGeneration::Completed => {
                    !inputs.question_slots.completed_found
                        || slot.mirror == QuestionSlotMirror::Tracked
                }
            })
        || inputs
            .question_slots
            .slots
            .iter()
            .enumerate()
            .any(|(index, slot)| {
                inputs.question_slots.slots[index + 1..]
                    .iter()
                    .any(|other| slot.generation == other.generation && slot.mirror == other.mirror)
            })
        || inputs
            .fallback
            .is_some_and(|row| row.id != inputs.question_slots.session || row.state != "slot")
        || position.examined[0] > inputs.questions.len()
        || position.examined[2] > inputs.native_publications.len()
        || position.examined[3] > inputs.native_historical.len()
        || position.examined[4] > inputs.legacy.len()
    {
        return Err(ReadError::InvalidSource);
    }
    let slot_count = inputs.question_slots.slots.len() + usize::from(inputs.fallback.is_some());
    if position.slot_offset > slot_count {
        return Err(ReadError::InvalidSource);
    }
    // Validate every key source even when this page's first item is a slot.
    select_pending_union_keys(
        inputs.runtime,
        inputs.runtime_recheck,
        position.examined[1],
        &inputs.questions[position.examined[0]..],
        &inputs.native_publications[position.examined[2]..],
        &inputs.native_historical[position.examined[3]..],
        &inputs.legacy[position.examined[4]..],
        position.key_bucket,
        1,
    )?;
    let mut items = Vec::with_capacity(limit);
    while items.len() < limit {
        let mut chosen = None;
        for _ in 0..2 {
            if position.page_bucket == 0 {
                let keyed = select_pending_union_keys(
                    inputs.runtime,
                    inputs.runtime_recheck,
                    position.examined[1],
                    &inputs.questions[position.examined[0]..],
                    &inputs.native_publications[position.examined[2]..],
                    &inputs.native_historical[position.examined[3]..],
                    &inputs.legacy[position.examined[4]..],
                    position.key_bucket,
                    1,
                )?;
                if let Some(candidate) = keyed.items.first().copied() {
                    position.examined[0] += keyed.examined[0];
                    position.examined[1] = keyed.examined[1];
                    position.examined[2] += keyed.examined[2];
                    position.examined[3] += keyed.examined[3];
                    position.examined[4] += keyed.examined[4];
                    position.key_bucket = keyed.next_bucket;
                    chosen = Some(PendingPageCandidate::Key(candidate));
                }
            } else if position.slot_offset < slot_count {
                chosen = Some(
                    if let Some(slot) = inputs.question_slots.slots.get(position.slot_offset) {
                        PendingPageCandidate::RuntimeSlot {
                            generation: slot.generation,
                            mirror: slot.mirror,
                        }
                    } else {
                        PendingPageCandidate::DurableFallback
                    },
                );
                position.slot_offset += 1;
            }
            position.page_bucket = (position.page_bucket + 1) % 2;
            if chosen.is_some() {
                break;
            }
        }
        let Some(candidate) = chosen else { break };
        items.push(candidate);
    }
    let NativeRuntimeApprovalListState::Present(runtime_rows) = &inputs.runtime.state else {
        return Err(ReadError::SourceUnavailable);
    };
    Ok(PendingPageSelection {
        items,
        remaining_in_inputs: position.examined[0] < inputs.questions.len()
            || position.examined[1] < runtime_rows.len()
            || position.examined[2] < inputs.native_publications.len()
            || position.examined[3] < inputs.native_historical.len()
            || position.examined[4] < inputs.legacy.len()
            || position.slot_offset < slot_count,
        next: position,
    })
}

pub fn project_pending_union_keys(
    selection: &PendingUnionSelection,
    hydrated: &[Option<PendingSourceRow>],
    runtime: &NativeRuntimeApprovalListSnapshot,
    runtime_recheck: NativeRuntimeApprovalRecheck,
    durable_observed_at: Timestamp,
) -> Result<PendingUnionProjection> {
    if selection.items.len() > 32 || selection.items.len() != hydrated.len() {
        return Err(ReadError::InvalidSource);
    }
    let NativeRuntimeApprovalListState::Present(runtime_rows) = &runtime.state else {
        return Err(ReadError::SourceUnavailable);
    };
    if runtime_recheck != NativeRuntimeApprovalRecheck::NoObservedChange {
        return Err(ReadError::SourceUnavailable);
    }
    if runtime_rows.len() > 64
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
    {
        return Err(ReadError::InvalidSource);
    }
    let mut items = Vec::with_capacity(selection.items.len());
    let mut filtered_durable = 0;
    for (candidate, row) in selection.items.iter().zip(hydrated) {
        let source = match candidate {
            PendingUnionCandidate::Question(key) if key.source == PendingSource::Questions => {
                Some(key.source)
            }
            PendingUnionCandidate::Legacy(key) if key.source == PendingSource::LegacyApprovals => {
                Some(key.source)
            }
            PendingUnionCandidate::Native { durable, .. } => {
                if let Some(key) = durable {
                    if !matches!(
                        key.source,
                        PendingSource::NativePublications | PendingSource::NativeHistorical
                    ) || key.id != candidate.id()
                    {
                        return Err(ReadError::InvalidSource);
                    }
                }
                durable.map(|key| key.source)
            }
            _ => return Err(ReadError::InvalidSource),
        };
        if row.as_ref().is_some_and(|row| row.id != candidate.id())
            || (row.is_some() && source.is_none())
            || matches!(
                candidate,
                PendingUnionCandidate::Native {
                    runtime: false,
                    durable: None,
                    ..
                }
            )
        {
            return Err(ReadError::InvalidSource);
        }
        filtered_durable += usize::from(source.is_some() && row.is_none());
        let summary = match (*candidate, row) {
            (PendingUnionCandidate::Question(_), Some(row)) => Some(durable_question_summary(
                row.clone(),
                false,
                durable_observed_at.clone(),
            )?),
            (PendingUnionCandidate::Legacy(_), Some(row)) => Some(durable_approval_summary(
                PendingSource::LegacyApprovals,
                row.clone(),
                durable_observed_at.clone(),
            )?),
            (
                PendingUnionCandidate::Native {
                    id,
                    runtime: witness,
                    durable,
                },
                row,
            ) => {
                let snapshot = if witness {
                    let index = runtime_rows
                        .binary_search_by_key(&id, |entry| entry.id)
                        .map_err(|_| ReadError::SourceUnavailable)?;
                    Some(NativeRuntimeApprovalSnapshot {
                        session: runtime.session,
                        selected: id,
                        observed_at: runtime.observed_at,
                        state: NativeRuntimeApprovalState::Present(runtime_rows[index].clone()),
                    })
                } else {
                    None
                };
                match (snapshot, durable, row) {
                    (Some(snapshot), Some(key), Some(row)) => Some(native_approval_union(
                        snapshot,
                        runtime_recheck,
                        key.source,
                        row.clone(),
                        durable_observed_at.clone(),
                    )?),
                    (Some(snapshot), _, _) => Some(runtime_native_approval_summary(snapshot)?),
                    (None, Some(key), Some(row)) => Some(durable_approval_summary(
                        key.source,
                        row.clone(),
                        durable_observed_at.clone(),
                    )?),
                    (None, _, None) => None,
                    (None, None, Some(_)) => return Err(ReadError::InvalidSource),
                }
            }
            (_, None) => None,
        };
        if let Some(summary) = summary {
            if items
                .iter()
                .any(|item: &DecisionSummaryV1| item.id == summary.id)
            {
                return Err(ReadError::InvalidSource);
            }
            items.push(summary);
        }
    }
    Ok(PendingUnionProjection {
        items,
        filtered_durable,
    })
}

/// Project one scheduled page without changing its key/slot order. Hydrated
/// rows align with candidates; only keyed candidates may carry a durable row.
/// A filtered keyed row is omitted from display but never made a tombstone.
/// Runtime rereads report only no observed bounded change, not stability.
pub fn project_pending_page_candidates(
    selection: &PendingPageSelection,
    hydrated: &[Option<PendingSourceRow>],
    native_runtime: &NativeRuntimeApprovalListSnapshot,
    native_recheck: NativeRuntimeApprovalRecheck,
    question_slots: &RuntimeQuestionSlotListSnapshot,
    question_slots_recheck: RuntimeQuestionSlotRecheck,
    fallback: Option<&PendingSourceRow>,
    durable_observed_at: Timestamp,
) -> Result<PendingUnionProjection> {
    if selection.items.len() > 32
        || selection.items.len() != hydrated.len()
        || native_runtime.session != question_slots.session
    {
        return Err(ReadError::InvalidSource);
    }
    if native_recheck != NativeRuntimeApprovalRecheck::NoObservedChange
        || question_slots_recheck != RuntimeQuestionSlotRecheck::NoObservedChange
    {
        return Err(ReadError::SourceUnavailable);
    }
    if question_slots.slots.len() > 3
        || question_slots
            .slots
            .iter()
            .enumerate()
            .any(|(index, slot)| {
                question_slots.slots[index + 1..]
                    .iter()
                    .any(|other| slot.generation == other.generation && slot.mirror == other.mirror)
            })
        || fallback.is_some_and(|row| row.id != question_slots.session || row.state != "slot")
    {
        return Err(ReadError::InvalidSource);
    }
    let mut items = Vec::with_capacity(selection.items.len());
    let mut filtered_durable = 0;
    for (candidate, row) in selection.items.iter().zip(hydrated) {
        let projected = match (*candidate, row) {
            (PendingPageCandidate::Key(key), row) => {
                let one = project_pending_union_keys(
                    &PendingUnionSelection {
                        items: vec![key],
                        examined: [0; 5],
                        next_bucket: 0,
                        remaining_in_inputs: false,
                    },
                    std::slice::from_ref(row),
                    native_runtime,
                    native_recheck,
                    durable_observed_at.clone(),
                )?;
                filtered_durable += one.filtered_durable;
                one.items.into_iter().next()
            }
            (PendingPageCandidate::RuntimeSlot { generation, mirror }, None) => {
                let slot = question_slots
                    .slots
                    .iter()
                    .find(|slot| slot.generation == generation && slot.mirror == mirror)
                    .ok_or(ReadError::SourceUnavailable)?;
                if match generation {
                    QuestionSlotGeneration::Spawn(value) => {
                        question_slots.active_generation != Some(value)
                    }
                    QuestionSlotGeneration::Completed => {
                        !question_slots.completed_found || mirror != QuestionSlotMirror::Session
                    }
                } {
                    return Err(ReadError::SourceUnavailable);
                }
                Some(runtime_question_slot_summary(
                    RuntimeQuestionSlotSnapshot {
                        project: question_slots.project,
                        session: question_slots.session,
                        generation,
                        mirror,
                        observed_at: match generation {
                            QuestionSlotGeneration::Spawn(_) => question_slots.active_observed_at,
                            QuestionSlotGeneration::Completed => {
                                question_slots.completed_observed_at
                            }
                        },
                        state: RuntimeQuestionSlotState::Present(slot.projection.clone()),
                    },
                )?)
            }
            (PendingPageCandidate::DurableFallback, None) => {
                let row = fallback.ok_or(ReadError::SourceUnavailable)?;
                Some(durable_question_summary(
                    row.clone(),
                    true,
                    durable_observed_at.clone(),
                )?)
            }
            (_, Some(_)) => return Err(ReadError::InvalidSource),
        };
        if let Some(summary) = projected {
            if items
                .iter()
                .any(|item: &DecisionSummaryV1| item.id == summary.id)
            {
                return Err(ReadError::InvalidSource);
            }
            items.push(summary);
        }
    }
    Ok(PendingUnionProjection {
        items,
        filtered_durable,
    })
}

/// Assemble a lower-bound attention hint from already de-duplicated live
/// signals. A page cannot claim no pending work until all eight sources have
/// been observed to exhaustion; source coverage is not a frozen snapshot.
pub fn session_attention(
    status: &SessionStatusV1,
    live_signals_lower_bound: u64,
    pending_coverage: &[SourceCoverageV1],
) -> Result<AttentionV1> {
    let complete = pending_coverage_complete(pending_coverage)?;
    let status_requires_inspection = matches!(
        status,
        SessionStatusV1::Known {
            value: KnownSessionStatusV1::WaitingApproval | KnownSessionStatusV1::Failed
        } | SessionStatusV1::Unknown { .. }
    );
    Ok(AttentionV1 {
        requires_local_action: status_requires_inspection
            || live_signals_lower_bound > 0
            || !complete,
        incomplete: !complete,
        live_signals_lower_bound: DecimalU64::new(live_signals_lower_bound.to_string())
            .map_err(|_| ReadError::InvalidSource)?,
    })
}

/// Project an exact selected durable-source miss after the caller has checked
/// runtime occurrence identity and released its source locks. Runtime slots
/// require a separate generation-fenced reread and cannot enter this path.
pub fn missing_selected_decision(
    selected: DecisionId,
    source: &SelectedPendingSource,
    pending_coverage: &[SourceCoverageV1],
    last_display: Option<RetainedDecisionDisplayV1>,
) -> Result<SelectedDecisionV1> {
    if !matches!(source, SelectedPendingSource::Missing) {
        return Err(ReadError::InvalidSource);
    }
    let identity_class = match selected.as_str().split(':').next() {
        Some("question" | "native") => IdentityClassV1::Publication,
        Some("legacy") => IdentityClassV1::Legacy,
        Some("question-fallback") => IdentityClassV1::Slot,
        _ => return Err(ReadError::InvalidSource),
    };
    let projected = if pending_coverage_complete(pending_coverage)? {
        SelectedDecisionV1::Tombstone {
            id: selected,
            identity_class,
            last_display,
            message: Text::<64>::new("Decision no longer available".into())
                .map_err(|_| ReadError::InvalidSource)?,
        }
    } else {
        let reason = if pending_coverage
            .iter()
            .any(|coverage| coverage.state == CoverageStateV1::Busy)
        {
            DegradationV1::Busy
        } else if pending_coverage
            .iter()
            .any(|coverage| coverage.state == CoverageStateV1::Unavailable)
        {
            DegradationV1::Unavailable
        } else {
            DegradationV1::Limited
        };
        SelectedDecisionV1::Unavailable {
            id: selected,
            last_display,
            reason,
        }
    };
    serde_json::from_value(serde_json::to_value(&projected).map_err(|_| ReadError::InvalidSource)?)
        .map_err(|_| ReadError::InvalidSource)
}

/// Project an exact saved-session selection from the same Store transaction as
/// its page. The caller must have completed both runtime rereads after Store
/// unlock. An exact durable miss can still have a native runtime occurrence;
/// all other misses use coverage to distinguish tombstone from unavailable.
pub fn selected_saved_decision(
    selected: DecisionId,
    source: SelectedPendingSource,
    acquired: &PendingAcquiredSources,
    durable_observed_at: Timestamp,
) -> Result<SelectedDecisionV1> {
    let (native, native_recheck) = required_observed(&acquired.native_runtime)?;
    let (slots, slots_recheck) = required_observed(&acquired.question_slots)?;
    if *native_recheck != NativeRuntimeApprovalRecheck::NoObservedChange
        || *slots_recheck != RuntimeQuestionSlotRecheck::NoObservedChange
        || acquired.coverage.len() != 8
        || native.session != slots.session
    {
        return Err(ReadError::SourceUnavailable);
    }
    let native_rows = match &native.state {
        NativeRuntimeApprovalListState::Present(rows) => rows,
        NativeRuntimeApprovalListState::Missing | NativeRuntimeApprovalListState::SourceChanged => {
            return Err(ReadError::SourceUnavailable);
        }
    };
    let native_id = selected
        .as_str()
        .strip_prefix("native:")
        .and_then(|raw| Uuid::parse_str(raw).ok());
    let native_row = native_id.and_then(|id| native_rows.iter().find(|row| row.id == id));
    let decision = match source {
        SelectedPendingSource::Durable { source, row } => match source {
            PendingSource::Questions => durable_question_summary(row, false, durable_observed_at)?,
            PendingSource::LegacyApprovals => {
                durable_approval_summary(source, row, durable_observed_at)?
            }
            PendingSource::NativePublications | PendingSource::NativeHistorical => {
                if let Some(runtime) = native_row {
                    native_approval_union(
                        NativeRuntimeApprovalSnapshot {
                            session: native.session,
                            selected: runtime.id,
                            observed_at: native.observed_at,
                            state: NativeRuntimeApprovalState::Present(runtime.clone()),
                        },
                        *native_recheck,
                        source,
                        row,
                        durable_observed_at,
                    )?
                } else {
                    durable_approval_summary(source, row, durable_observed_at)?
                }
            }
        },
        SelectedPendingSource::QuestionFallback(row) => {
            durable_question_summary(row, true, durable_observed_at)?
        }
        SelectedPendingSource::RuntimeSlot { generation, mirror } => {
            let slot_id = DecisionId::new(format!(
                "question-slot:{}:{}:{}",
                slots.session,
                match generation {
                    QuestionSlotGeneration::Spawn(value) => value.to_string(),
                    QuestionSlotGeneration::Completed => "completed".to_owned(),
                },
                match mirror {
                    QuestionSlotMirror::Tracked => "tracked",
                    QuestionSlotMirror::Session => "session",
                }
            ))
            .map_err(|_| ReadError::InvalidSource)?;
            if slot_id != selected {
                return Err(ReadError::InvalidSource);
            }
            let Some(slot) = slots
                .slots
                .iter()
                .find(|slot| slot.generation == generation && slot.mirror == mirror)
            else {
                return Ok(SelectedDecisionV1::Unavailable {
                    id: selected,
                    last_display: None,
                    reason: DegradationV1::SourceChanged,
                });
            };
            runtime_question_slot_summary(RuntimeQuestionSlotSnapshot {
                project: slots.project,
                session: slots.session,
                generation,
                mirror,
                observed_at: match generation {
                    QuestionSlotGeneration::Spawn(value)
                        if slots.active_generation == Some(value) =>
                    {
                        slots.active_observed_at
                    }
                    QuestionSlotGeneration::Completed if slots.completed_found => {
                        slots.completed_observed_at
                    }
                    _ => return Err(ReadError::SourceUnavailable),
                },
                state: RuntimeQuestionSlotState::Present(slot.projection.clone()),
            })?
        }
        SelectedPendingSource::Missing => {
            if let Some(runtime) = native_row {
                runtime_native_approval_summary(NativeRuntimeApprovalSnapshot {
                    session: native.session,
                    selected: runtime.id,
                    observed_at: native.observed_at,
                    state: NativeRuntimeApprovalState::Present(runtime.clone()),
                })?
            } else {
                return missing_selected_decision(
                    selected,
                    &SelectedPendingSource::Missing,
                    &acquired.coverage,
                    None,
                );
            }
        }
    };
    if decision.id != selected {
        return Err(ReadError::InvalidSource);
    }
    Ok(SelectedDecisionV1::Present {
        decision: Box::new(decision),
        stale: false,
    })
}

/// Resolve one exact runtime-only identity after the session Store miss and
/// pending-source rereads. Durable-only kinds remain unavailable because the
/// five saved-session sources cannot establish absence for this session.
pub fn selected_runtime_only_decision(
    selected: &DecisionId,
    project: Uuid,
    session: Uuid,
    acquired: &RuntimeOnlyPendingSources,
) -> Result<SelectedDecisionV1> {
    if acquired.coverage.len() != 8 {
        return Err(ReadError::InvalidSource);
    }
    let unavailable = |reason| SelectedDecisionV1::Unavailable {
        id: selected.clone(),
        last_display: None,
        reason,
    };
    let parts: Vec<_> = selected.as_str().split(':').collect();
    let decision = match parts.as_slice() {
        ["native", raw] => {
            let id = Uuid::parse_str(raw).map_err(|_| ReadError::InvalidSource)?;
            let (native, recheck) = match &acquired.native_runtime {
                PendingRead::Observed { value, .. } => value,
                PendingRead::Busy { .. } => return Ok(unavailable(DegradationV1::Busy)),
                PendingRead::Unavailable { .. } => {
                    return Ok(unavailable(DegradationV1::Unavailable));
                }
            };
            if native.session != session
                || *recheck != NativeRuntimeApprovalRecheck::NoObservedChange
            {
                return Ok(unavailable(DegradationV1::SourceChanged));
            }
            let NativeRuntimeApprovalListState::Present(rows) = &native.state else {
                return Ok(unavailable(DegradationV1::Unavailable));
            };
            let Some(row) = rows.iter().find(|row| row.id == id) else {
                return Ok(unavailable(DegradationV1::Unavailable));
            };
            runtime_native_approval_summary(NativeRuntimeApprovalSnapshot {
                session,
                selected: id,
                observed_at: native.observed_at,
                state: NativeRuntimeApprovalState::Present(row.clone()),
            })?
        }
        ["question-slot", raw, _, _] if raw == &session.to_string() => {
            let (slots, recheck) = match &acquired.question_slots {
                PendingRead::Observed { value, .. } => value,
                PendingRead::Busy { .. } => return Ok(unavailable(DegradationV1::Busy)),
                PendingRead::Unavailable { .. } => {
                    return Ok(unavailable(DegradationV1::Unavailable));
                }
            };
            if slots.project != project
                || slots.session != session
                || *recheck != RuntimeQuestionSlotRecheck::NoObservedChange
            {
                return Ok(unavailable(DegradationV1::SourceChanged));
            }
            let Some(slot) = slots.slots.iter().find(|slot| {
                let generation = match slot.generation {
                    QuestionSlotGeneration::Spawn(value) => value.to_string(),
                    QuestionSlotGeneration::Completed => "completed".to_owned(),
                };
                let mirror = match slot.mirror {
                    QuestionSlotMirror::Tracked => "tracked",
                    QuestionSlotMirror::Session => "session",
                };
                selected.as_str() == format!("question-slot:{session}:{generation}:{mirror}")
            }) else {
                return Ok(unavailable(DegradationV1::SourceChanged));
            };
            runtime_question_slot_summary(RuntimeQuestionSlotSnapshot {
                project,
                session,
                generation: slot.generation,
                mirror: slot.mirror,
                observed_at: match slot.generation {
                    QuestionSlotGeneration::Spawn(value)
                        if slots.active_generation == Some(value) =>
                    {
                        slots.active_observed_at
                    }
                    QuestionSlotGeneration::Completed if slots.completed_found => {
                        slots.completed_observed_at
                    }
                    _ => return Ok(unavailable(DegradationV1::SourceChanged)),
                },
                state: RuntimeQuestionSlotState::Present(slot.projection.clone()),
            })?
        }
        ["question" | "legacy" | "question-fallback", ..] => {
            return Ok(unavailable(DegradationV1::Unavailable));
        }
        _ => return Err(ReadError::InvalidSource),
    };
    if decision.id.as_str() != selected.as_str() {
        return Err(ReadError::InvalidSource);
    }
    Ok(SelectedDecisionV1::Present {
        decision: Box::new(decision),
        stale: false,
    })
}

/// Project one persisted session without inventing runtime attention or
/// changing unknown provider, kind, or status labels. The caller supplies its
/// separately observed attention value before serializing the full response.
pub fn session_summary(
    row: SessionRow,
    project: Uuid,
    attention: AttentionV1,
) -> Result<SessionSummaryV1> {
    if row.parent_id == Some(row.id) || row.continued_from == Some(row.id) {
        return Err(ReadError::InvalidSource);
    }
    let kind = if let Some(value) = known_label::<KnownSessionKindV1>(&row.kind) {
        SessionKindV1::Known { value }
    } else {
        SessionKindV1::Unknown {
            label: Text::<128>::new(row.kind).map_err(|_| ReadError::InvalidSource)?,
        }
    };
    let provider = if let Some(value) = known_label::<KnownProviderV1>(&row.provider) {
        ProviderV1::Known { value }
    } else {
        ProviderV1::Unknown {
            label: Text::<128>::new(row.provider).map_err(|_| ReadError::InvalidSource)?,
        }
    };
    let status = if let Some(value) = known_label::<KnownSessionStatusV1>(&row.status) {
        SessionStatusV1::Known { value }
    } else {
        SessionStatusV1::Unknown {
            label: Text::<128>::new(row.status).map_err(|_| ReadError::InvalidSource)?,
        }
    };
    Ok(SessionSummaryV1 {
        id: wire_uuid(row.id)?,
        project_id: wire_uuid(project)?,
        parent_id: row.parent_id.map(wire_uuid).transpose()?,
        continued_from: row.continued_from.map(wire_uuid).transpose()?,
        kind,
        own_title: Text::<512>::new(row.own_title.text).map_err(|_| ReadError::InvalidSource)?,
        provider,
        status,
        updated_at: Timestamp::new(row.updated_at).map_err(|_| ReadError::InvalidSource)?,
        attention,
    })
}

fn display_field(
    value: Option<BoundedText>,
    field: DisplaySourceFieldV1,
) -> Result<DisplayFieldV1> {
    let Some(value) = value.filter(|value| !value.text.is_empty()) else {
        return Ok(DisplayFieldV1 {
            text: Text::<4096>::new(unavailable_label(field).into())
                .map_err(|_| ReadError::InvalidSource)?,
            state: PreviewStateV1::Unavailable,
            source_field: field,
            source_extent: SourceExtentV1::Unknown,
            observed_bytes: None,
        });
    };
    if value.observed_bytes < value.text.len() as u64
        || (!value.truncated && value.observed_bytes != value.text.len() as u64)
    {
        return Err(ReadError::InvalidSource);
    }
    Ok(DisplayFieldV1 {
        text: Text::<4096>::new(value.text).map_err(|_| ReadError::InvalidSource)?,
        state: if value.truncated {
            PreviewStateV1::Truncated
        } else {
            PreviewStateV1::Complete
        },
        source_field: field,
        source_extent: SourceExtentV1::FullField,
        observed_bytes: Some(
            DecimalU64::new(value.observed_bytes.to_string())
                .map_err(|_| ReadError::InvalidSource)?,
        ),
    })
}

fn approval_display_field(
    value: Option<BoundedText>,
    field: DisplaySourceFieldV1,
    extent: SourceExtentV1,
    cap: usize,
) -> Result<DisplayFieldV1> {
    let Some(value) = value.filter(|value| !value.text.is_empty()) else {
        return Ok(DisplayFieldV1 {
            text: Text::<4096>::new(unavailable_label(field).into())
                .map_err(|_| ReadError::InvalidSource)?,
            state: PreviewStateV1::Unavailable,
            source_field: field,
            source_extent: if extent == SourceExtentV1::BoundedSnapshot {
                extent
            } else {
                SourceExtentV1::Unknown
            },
            observed_bytes: None,
        });
    };
    if value.text.len() > cap
        || value.observed_bytes < value.text.len() as u64
        || (!value.truncated && value.observed_bytes != value.text.len() as u64)
    {
        return Err(ReadError::InvalidSource);
    }
    Ok(DisplayFieldV1 {
        text: Text::<4096>::new(value.text).map_err(|_| ReadError::InvalidSource)?,
        state: if value.truncated {
            PreviewStateV1::Truncated
        } else {
            PreviewStateV1::Complete
        },
        source_field: field,
        source_extent: extent,
        observed_bytes: Some(
            DecimalU64::new(value.observed_bytes.to_string())
                .map_err(|_| ReadError::InvalidSource)?,
        ),
    })
}

/// Preserve named approval fields. Native target JSON may already be short,
/// so complete reads are labelled as bounded snapshots. Legacy tool names
/// come from their own full column, with no conversation-history dependency.
pub fn approval_display(source: PendingSource, row: PendingSourceRow) -> Result<DecisionDisplayV1> {
    match source {
        PendingSource::NativePublications | PendingSource::NativeHistorical => {
            if row.tool_name.is_some() {
                return Err(ReadError::InvalidSource);
            }
            Ok(DecisionDisplayV1::NativeApproval {
                method: approval_display_field(
                    row.method,
                    DisplaySourceFieldV1::Method,
                    SourceExtentV1::BoundedSnapshot,
                    512,
                )?,
                description: approval_display_field(
                    row.description,
                    DisplaySourceFieldV1::Description,
                    SourceExtentV1::BoundedSnapshot,
                    2048,
                )?,
            })
        }
        PendingSource::LegacyApprovals => {
            if row.method.is_some() || row.description.is_some() {
                return Err(ReadError::InvalidSource);
            }
            Ok(DecisionDisplayV1::LegacyApproval {
                tool_name: approval_display_field(
                    row.tool_name,
                    DisplaySourceFieldV1::ToolName,
                    SourceExtentV1::FullField,
                    512,
                )?,
            })
        }
        PendingSource::Questions => Err(ReadError::InvalidSource),
    }
}

/// Project one durable approval occurrence without inventing a runtime writer
/// or combining mirrors. The union owner supplies a bounded observation time
/// and later merges exact same-ID native provenance before response delivery.
pub fn durable_approval_summary(
    source: PendingSource,
    row: PendingSourceRow,
    observed_at: Timestamp,
) -> Result<DecisionSummaryV1> {
    let (prefix, identity_class, kind, observation_source) = match source {
        PendingSource::NativePublications => (
            "native",
            IdentityClassV1::Publication,
            DecisionKindV1::NativeApproval,
            SourceV1::NativePublications,
        ),
        PendingSource::NativeHistorical => (
            "native",
            IdentityClassV1::Publication,
            DecisionKindV1::NativeApproval,
            SourceV1::NativeHistoricalFallback,
        ),
        PendingSource::LegacyApprovals => (
            "legacy",
            IdentityClassV1::Legacy,
            DecisionKindV1::LegacyApproval,
            SourceV1::LegacyApprovals,
        ),
        PendingSource::Questions => return Err(ReadError::InvalidSource),
    };
    let state = row.state.clone();
    let closure_raw = row.closure_state.clone();
    let incarnation = row.incarnation_id.map(wire_uuid).transpose()?;
    let id =
        DecisionId::new(format!("{prefix}:{}", row.id)).map_err(|_| ReadError::InvalidSource)?;
    let publication_state = if let Some(value) = known_label::<KnownPublicationStateV1>(&state) {
        PublicationStateV1::Known { value }
    } else {
        PublicationStateV1::Unknown {
            label: Text::<128>::new(state.clone()).map_err(|_| ReadError::InvalidSource)?,
        }
    };
    let closure_state = if source == PendingSource::LegacyApprovals {
        match state.as_str() {
            "Pending" => ClosureStateV1::Open,
            "Approved" | "Denied" => ClosureStateV1::Closed,
            _ => ClosureStateV1::Unknown,
        }
    } else if state == "superseded" {
        ClosureStateV1::Closed
    } else {
        closure_raw
            .as_deref()
            .and_then(known_label::<ClosureStateV1>)
            .unwrap_or(ClosureStateV1::Unknown)
    };
    let delivery_state = if state == "enqueued" {
        DeliveryStateV1::Enqueued
    } else {
        DeliveryStateV1::Unknown
    };
    let display = approval_display(source, row)?;
    let field_states = match &display {
        DecisionDisplayV1::NativeApproval {
            method,
            description,
        } => [method.state, description.state],
        DecisionDisplayV1::LegacyApproval { tool_name } => {
            [tool_name.state, PreviewStateV1::Complete]
        }
        DecisionDisplayV1::GenericQuestions { .. } => return Err(ReadError::InvalidSource),
    };
    let details_state = if field_states.contains(&PreviewStateV1::Unavailable) {
        PreviewStateV1::Unavailable
    } else if field_states.contains(&PreviewStateV1::Truncated) {
        PreviewStateV1::Truncated
    } else {
        PreviewStateV1::Complete
    };
    let requires_local_action = closure_state != ClosureStateV1::Closed
        || details_state != PreviewStateV1::Complete
        || matches!(state.as_str(), "expired" | "enqueued");
    let summary = DecisionSummaryV1 {
        id,
        identity_class,
        kind,
        publication_state: Some(publication_state),
        closure_state,
        delivery_state,
        source_observations: vec![DecisionSourceObservationV1 {
            source: observation_source,
            observed_at,
            state: CoverageStateV1::Complete,
            incarnation,
            spawn_generation: None,
            witness_present: None,
            resolution_observed: None,
            resolution_persisted: None,
            writer_live: None,
            writer_capacity: None,
            display_alternative: None,
        }],
        omitted_source_observations: DecimalU64::new("0".into())
            .map_err(|_| ReadError::InvalidSource)?,
        disagreement: false,
        display,
        questions: vec![],
        omitted_questions: DecimalU64::new("0".into()).map_err(|_| ReadError::InvalidSource)?,
        details_state,
        requires_local_action,
        can_answer: false,
    };
    serde_json::from_value(serde_json::to_value(&summary).map_err(|_| ReadError::InvalidSource)?)
        .map_err(|_| ReadError::InvalidSource)
}

/// Project one durable generic-question source without treating a fallback
/// slot as a publication occurrence. No answer or runtime writer is inferred.
pub fn durable_question_summary(
    row: PendingSourceRow,
    fallback: bool,
    observed_at: Timestamp,
) -> Result<DecisionSummaryV1> {
    let state = row.state.clone();
    if fallback != (state == "slot") {
        return Err(ReadError::InvalidSource);
    }
    let id = DecisionId::new(format!(
        "{}:{}",
        if fallback {
            "question-fallback"
        } else {
            "question"
        },
        row.id
    ))
    .map_err(|_| ReadError::InvalidSource)?;
    let publication_state = if fallback {
        None
    } else if matches!(state.as_str(), "unresolved" | "published" | "cleared") {
        let value =
            known_label::<KnownPublicationStateV1>(&state).ok_or(ReadError::InvalidSource)?;
        Some(PublicationStateV1::Known { value })
    } else {
        Some(PublicationStateV1::Unknown {
            label: Text::<128>::new(state.clone()).map_err(|_| ReadError::InvalidSource)?,
        })
    };
    let closure_state = match state.as_str() {
        "unresolved" | "published" => ClosureStateV1::Open,
        "cleared" => ClosureStateV1::Closed,
        _ => ClosureStateV1::Unknown,
    };
    let question_source = if row.details_unavailable {
        None
    } else {
        row.question_json
            .as_deref()
            .filter(|raw| raw.len() <= 65_536)
            .and_then(parse_json_limited)
            .and_then(|value| serde_json::from_value::<PendingQuestion>(value).ok())
    };
    let question_projection = project_questions(question_source.as_ref())?;
    let summary = DecisionSummaryV1 {
        id,
        identity_class: if fallback {
            IdentityClassV1::Slot
        } else {
            IdentityClassV1::Publication
        },
        kind: DecisionKindV1::GenericQuestions,
        publication_state,
        closure_state,
        delivery_state: DeliveryStateV1::Unknown,
        source_observations: vec![DecisionSourceObservationV1 {
            source: if fallback {
                SourceV1::DurableQuestionFallback
            } else {
                SourceV1::QuestionPublications
            },
            observed_at,
            state: CoverageStateV1::Complete,
            incarnation: None,
            spawn_generation: None,
            witness_present: None,
            resolution_observed: None,
            resolution_persisted: None,
            writer_live: None,
            writer_capacity: None,
            display_alternative: None,
        }],
        omitted_source_observations: DecimalU64::new("0".into())
            .map_err(|_| ReadError::InvalidSource)?,
        disagreement: false,
        display: DecisionDisplayV1::GenericQuestions {},
        questions: question_projection.questions,
        omitted_questions: question_projection.omitted_questions,
        details_state: question_projection.details_state,
        requires_local_action: closure_state != ClosureStateV1::Closed
            || question_projection.details_state != PreviewStateV1::Complete,
        can_answer: false,
    };
    serde_json::from_value(serde_json::to_value(&summary).map_err(|_| ReadError::InvalidSource)?)
        .map_err(|_| ReadError::InvalidSource)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionProjection {
    pub questions: Vec<QuestionV1>,
    pub omitted_questions: DecimalU64,
    pub details_state: PreviewStateV1,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeQuestionSlotState {
    Present(QuestionProjection),
    Missing,
    SourceChanged,
}

#[derive(Debug)]
pub struct RuntimeQuestionSlotSnapshot {
    pub project: Uuid,
    pub session: Uuid,
    pub generation: QuestionSlotGeneration,
    pub mirror: QuestionSlotMirror,
    pub observed_at: DateTime<Utc>,
    pub state: RuntimeQuestionSlotState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeQuestionSlotRecheck {
    NoObservedChange,
    Changed,
}

#[derive(Debug)]
pub struct RuntimeQuestionSlotRecheckObservation {
    pub state: RuntimeQuestionSlotRecheck,
    pub observed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeQuestionSlotListEntry {
    pub generation: QuestionSlotGeneration,
    pub mirror: QuestionSlotMirror,
    pub projection: QuestionProjection,
}

#[derive(Debug)]
pub struct RuntimeQuestionSlotListSnapshot {
    pub project: Uuid,
    pub session: Uuid,
    pub active_observed_at: DateTime<Utc>,
    pub completed_observed_at: DateTime<Utc>,
    /// Presence and generation of the active cache entry, even with no slot.
    pub active_generation: Option<u64>,
    /// Presence of the completed cache entry, even with no slot.
    pub completed_found: bool,
    /// At most tracked active, session active, and session completed.
    pub slots: Vec<RuntimeQuestionSlotListEntry>,
}

#[derive(Debug)]
pub struct RuntimeQuestionSlotListRecheckObservation {
    pub state: RuntimeQuestionSlotRecheck,
    pub active_observed_at: DateTime<Utc>,
    pub completed_observed_at: DateTime<Utc>,
}

/// A runtime slot has no publication occurrence identity. Only a present,
/// generation-fenced observation can be displayed; a missing or changed slot
/// is source-unavailable and must never be converted into a tombstone here.
pub fn runtime_question_slot_summary(
    snapshot: RuntimeQuestionSlotSnapshot,
) -> Result<DecisionSummaryV1> {
    let RuntimeQuestionSlotState::Present(projection) = snapshot.state else {
        return Err(ReadError::SourceUnavailable);
    };
    let generation = match snapshot.generation {
        QuestionSlotGeneration::Spawn(value) => value.to_string(),
        QuestionSlotGeneration::Completed => "completed".into(),
    };
    let (mirror, source) = match snapshot.mirror {
        QuestionSlotMirror::Tracked => ("tracked", SourceV1::TrackedQuestionSlot),
        QuestionSlotMirror::Session => ("session", SourceV1::SessionQuestionSlot),
    };
    let id = DecisionId::new(format!(
        "question-slot:{}:{generation}:{mirror}",
        snapshot.session
    ))
    .map_err(|_| ReadError::InvalidSource)?;
    let observed_at = Timestamp::new(
        snapshot
            .observed_at
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
    )
    .map_err(|_| ReadError::InvalidSource)?;
    let summary = DecisionSummaryV1 {
        id,
        identity_class: IdentityClassV1::Slot,
        kind: DecisionKindV1::GenericQuestions,
        publication_state: None,
        closure_state: ClosureStateV1::Unknown,
        delivery_state: DeliveryStateV1::Unknown,
        source_observations: vec![DecisionSourceObservationV1 {
            source,
            observed_at,
            state: CoverageStateV1::Complete,
            incarnation: None,
            spawn_generation: match snapshot.generation {
                QuestionSlotGeneration::Spawn(value) => {
                    Some(DecimalU64::new(value.to_string()).map_err(|_| ReadError::InvalidSource)?)
                }
                QuestionSlotGeneration::Completed => None,
            },
            witness_present: None,
            resolution_observed: None,
            resolution_persisted: None,
            writer_live: None,
            writer_capacity: None,
            display_alternative: None,
        }],
        omitted_source_observations: DecimalU64::new("0".into())
            .map_err(|_| ReadError::InvalidSource)?,
        disagreement: false,
        display: DecisionDisplayV1::GenericQuestions {},
        questions: projection.questions,
        omitted_questions: projection.omitted_questions,
        details_state: projection.details_state,
        requires_local_action: true,
        can_answer: false,
    };
    serde_json::from_value(serde_json::to_value(&summary).map_err(|_| ReadError::InvalidSource)?)
        .map_err(|_| ReadError::InvalidSource)
}

/// Display one exact runtime native approval witness. Its presence is not a
/// durable publication, closure, delivery, or answer-authority observation.
/// A missing or replaced witness remains unavailable, never a tombstone.
pub fn runtime_native_approval_summary(
    snapshot: NativeRuntimeApprovalSnapshot,
) -> Result<DecisionSummaryV1> {
    let NativeRuntimeApprovalState::Present(row) = snapshot.state else {
        return Err(ReadError::SourceUnavailable);
    };
    if row.id != snapshot.selected || !row.writer_live || row.writer_capacity > 64 {
        return Err(ReadError::InvalidSource);
    }
    let method = approval_display_field(
        row.method,
        DisplaySourceFieldV1::Method,
        SourceExtentV1::BoundedSnapshot,
        512,
    )?;
    let description = approval_display_field(
        row.description,
        DisplaySourceFieldV1::Description,
        SourceExtentV1::BoundedSnapshot,
        2048,
    )?;
    let details_state = if [method.state, description.state].contains(&PreviewStateV1::Unavailable)
    {
        PreviewStateV1::Unavailable
    } else if [method.state, description.state].contains(&PreviewStateV1::Truncated) {
        PreviewStateV1::Truncated
    } else {
        PreviewStateV1::Complete
    };
    let observed_at = Timestamp::new(
        snapshot
            .observed_at
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
    )
    .map_err(|_| ReadError::InvalidSource)?;
    let summary = DecisionSummaryV1 {
        id: DecisionId::new(format!("native:{}", row.id)).map_err(|_| ReadError::InvalidSource)?,
        identity_class: IdentityClassV1::Publication,
        kind: DecisionKindV1::NativeApproval,
        publication_state: None,
        closure_state: ClosureStateV1::Unknown,
        delivery_state: DeliveryStateV1::Unknown,
        source_observations: vec![DecisionSourceObservationV1 {
            source: SourceV1::NativeRuntime,
            observed_at,
            state: CoverageStateV1::Complete,
            incarnation: Some(wire_uuid(row.incarnation_id)?),
            spawn_generation: Some(
                DecimalU64::new(row.spawn_generation.to_string())
                    .map_err(|_| ReadError::InvalidSource)?,
            ),
            witness_present: Some(true),
            resolution_observed: Some(row.resolution_observed),
            resolution_persisted: Some(row.resolution_persisted),
            writer_live: Some(row.writer_live),
            writer_capacity: Some(row.writer_capacity),
            display_alternative: None,
        }],
        omitted_source_observations: DecimalU64::new("0".into())
            .map_err(|_| ReadError::InvalidSource)?,
        disagreement: false,
        display: DecisionDisplayV1::NativeApproval {
            method,
            description,
        },
        questions: vec![],
        omitted_questions: DecimalU64::new("0".into()).map_err(|_| ReadError::InvalidSource)?,
        details_state,
        requires_local_action: true,
        can_answer: false,
    };
    serde_json::from_value(serde_json::to_value(&summary).map_err(|_| ReadError::InvalidSource)?)
        .map_err(|_| ReadError::InvalidSource)
}

/// Join one selected native runtime witness to its exact durable publication
/// after a post-Store reread. The reread reports only no observed bounded
/// change; it does not make the sources a stable snapshot. Conflicting source
/// identity or display remains visible and requires local inspection.
pub fn native_approval_union(
    runtime: NativeRuntimeApprovalSnapshot,
    recheck: NativeRuntimeApprovalRecheck,
    durable_source: PendingSource,
    durable_row: PendingSourceRow,
    durable_observed_at: Timestamp,
) -> Result<DecisionSummaryV1> {
    if recheck != NativeRuntimeApprovalRecheck::NoObservedChange
        || !matches!(
            durable_source,
            PendingSource::NativePublications | PendingSource::NativeHistorical
        )
        || durable_row.id != runtime.selected
    {
        return Err(ReadError::SourceUnavailable);
    }
    let mut runtime = runtime_native_approval_summary(runtime)?;
    let mut durable = durable_approval_summary(durable_source, durable_row, durable_observed_at)?;
    if runtime.id != durable.id
        || runtime.kind != DecisionKindV1::NativeApproval
        || durable.kind != DecisionKindV1::NativeApproval
        || runtime.source_observations.len() != 1
        || durable.source_observations.len() != 1
    {
        return Err(ReadError::InvalidSource);
    }
    let mut witness = runtime.source_observations.remove(0);
    let recorded = &durable.source_observations[0];
    let identity_conflict = recorded
        .incarnation
        .as_ref()
        .is_some_and(|incarnation| Some(incarnation) != witness.incarnation.as_ref());
    let display_conflict = runtime.display != durable.display;
    if display_conflict {
        witness.display_alternative = Some(runtime.display);
    }
    durable.source_observations.insert(0, witness);
    durable.disagreement = identity_conflict || display_conflict;
    if identity_conflict {
        durable.publication_state = None;
        durable.closure_state = ClosureStateV1::Ambiguous;
        durable.delivery_state = DeliveryStateV1::Unknown;
    }
    if durable.disagreement {
        durable.details_state = PreviewStateV1::Unavailable;
    }
    durable.requires_local_action = true;
    durable.can_answer = false;
    serde_json::from_value(serde_json::to_value(&durable).map_err(|_| ReadError::InvalidSource)?)
        .map_err(|_| ReadError::InvalidSource)
}

fn question_unavailable() -> Result<QuestionProjection> {
    Ok(QuestionProjection {
        questions: vec![],
        omitted_questions: DecimalU64::new("0".into()).map_err(|_| ReadError::InvalidSource)?,
        details_state: PreviewStateV1::Unavailable,
    })
}

fn utf8_prefix(value: &str, cap: usize) -> (&str, bool) {
    let mut end = value.len().min(cap);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    (&value[..end], end < value.len())
}

/// Copy only bounded generic-question fields. This also accepts a borrowed
/// runtime question without cloning its potentially large strings or walking
/// omitted items. Missing or unusable source details stay unavailable.
pub fn project_questions(source: Option<&PendingQuestion>) -> Result<QuestionProjection> {
    let Some(source) = source.filter(|source| !source.questions.is_empty()) else {
        return question_unavailable();
    };
    let mut truncated = source.questions.len() > 8;
    let mut omitted_questions = u64::try_from(source.questions.len().saturating_sub(8))
        .map_err(|_| ReadError::ResourceLimit)?;
    let mut questions = Vec::with_capacity(source.questions.len().min(8));
    for item in source.questions.iter().take(8) {
        let (question, cut_question) = utf8_prefix(&item.question, 2048);
        if question.is_empty() {
            return question_unavailable();
        }
        let (header, cut_header) = utf8_prefix(&item.header, 128);
        truncated |= cut_question || cut_header || item.options.len() > 8;
        let mut options = Vec::with_capacity(item.options.len().min(8));
        for option in item.options.iter().take(8) {
            let (label, cut_label) = utf8_prefix(&option.label, 128);
            if label.is_empty() {
                return question_unavailable();
            }
            let (description, cut_description) = utf8_prefix(&option.description, 512);
            truncated |= cut_label || cut_description;
            options.push(QuestionOptionV1 {
                label: Text::<128>::new(label.into()).map_err(|_| ReadError::InvalidSource)?,
                description: Text::<512, false>::new(description.into())
                    .map_err(|_| ReadError::InvalidSource)?,
            });
        }
        questions.push(QuestionV1 {
            header: Text::<128, false>::new(header.into()).map_err(|_| ReadError::InvalidSource)?,
            question: Text::<2048>::new(question.into()).map_err(|_| ReadError::InvalidSource)?,
            options,
            multi_select: item.multi_select,
            omitted_options: DecimalU64::new(item.options.len().saturating_sub(8).to_string())
                .map_err(|_| ReadError::InvalidSource)?,
        });
    }
    // Leave headroom for identity, provenance and field names within the
    // decision's 8 KiB wire cap. Tail options/questions become explicit
    // omissions instead of an oversized or silently shortened response.
    while serde_json::to_vec(&questions)
        .map_err(|_| ReadError::InvalidSource)?
        .len()
        > 5_000
    {
        let last = questions.last_mut().ok_or(ReadError::InvalidSource)?;
        if last.options.pop().is_some() {
            let omitted = last.omitted_options.get().saturating_add(1);
            last.omitted_options =
                DecimalU64::new(omitted.to_string()).map_err(|_| ReadError::InvalidSource)?;
        } else if questions.len() > 1 {
            questions.pop();
            omitted_questions = omitted_questions.saturating_add(1);
        } else {
            return question_unavailable();
        }
        truncated = true;
    }
    Ok(QuestionProjection {
        questions,
        omitted_questions: DecimalU64::new(omitted_questions.to_string())
            .map_err(|_| ReadError::InvalidSource)?,
        details_state: if truncated {
            PreviewStateV1::Truncated
        } else {
            PreviewStateV1::Complete
        },
    })
}

/// Assemble saved detail with separately observed runtime and pending source
/// coverage. The Store history position is always represented, including an
/// observed empty history. The caller retains its read snapshot and permit.
pub fn session_detail(
    source: SessionDetailSource,
    summary: SessionSummaryV1,
    runtime_sequences: Vec<SequenceObservationV1>,
    pending_coverage: Vec<SourceCoverageV1>,
) -> Result<SessionDetailV1> {
    if runtime_sequences.len() > 2
        || runtime_sequences.iter().any(|observation| {
            !matches!(
                observation.source,
                SourceV1::ActiveSessions | SourceV1::CompletedSessions
            )
        })
    {
        return Err(ReadError::InvalidSource);
    }
    let (sequence, event_id) = match source.saved_history_head {
        Some((sequence, id)) if id > 0 => (
            Some(sequence),
            Some(DecimalI64::new(id.to_string()).map_err(|_| ReadError::InvalidSource)?),
        ),
        Some(_) => return Err(ReadError::InvalidSource),
        None => (None, None),
    };
    let mut sequences = runtime_sequences;
    sequences.push(SequenceObservationV1 {
        source: SourceV1::StoreHistory,
        sequence,
        event_id,
    });
    let detail = SessionDetailV1 {
        summary,
        own_title: Text::<4096>::new(source.own_title.text)
            .map_err(|_| ReadError::InvalidSource)?,
        query: display_field(source.query, DisplaySourceFieldV1::Query)?,
        model: display_field(source.model, DisplaySourceFieldV1::Model)?,
        sequences,
        pending_coverage,
    };
    // The generated wire validator enforces the title-prefix, sequence and
    // coverage invariants before this DTO can reach a transport adapter.
    serde_json::from_value(serde_json::to_value(&detail).map_err(|_| ReadError::InvalidSource)?)
        .map_err(|_| ReadError::InvalidSource)
}

/// Project one saved event. An exact tool key only proves the full source ID
/// fits; `ambiguous_key` reports a separately observed reused occurrence.
/// The caller must preserve its source coverage when that check is incomplete.
pub fn history_event(row: HistoryRow, ambiguous_key: bool) -> Result<HistoryEventV1> {
    if row.id <= 0 || (ambiguous_key && row.tool_pair_key.is_none()) {
        return Err(ReadError::InvalidSource);
    }
    let complete_tool_id = row
        .tool_id_display
        .as_ref()
        .is_some_and(|id| !id.text.is_empty() && !id.truncated);
    if row.tool_pair_key.is_some() != complete_tool_id {
        return Err(ReadError::InvalidSource);
    }
    let kind = if let Some(value) = known_label::<KnownEventKindV1>(&row.event_type) {
        EventKindV1::Known { value }
    } else {
        EventKindV1::Unknown {
            label: Text::<128>::new(row.event_type).map_err(|_| ReadError::InvalidSource)?,
        }
    };
    let role = row
        .role
        .map(|role| -> Result<RoleV1> {
            if let Some(value) = known_label::<KnownRoleV1>(&role) {
                Ok(RoleV1::Known { value })
            } else {
                Ok(RoleV1::Unknown {
                    label: Text::<128>::new(role).map_err(|_| ReadError::InvalidSource)?,
                })
            }
        })
        .transpose()?;
    let tool_name = row
        .tool_name
        .filter(|name| !name.text.is_empty())
        .map(|name| Text::<512>::new(name.text).map_err(|_| ReadError::InvalidSource))
        .transpose()?;
    let tool_id_display = row
        .tool_id_display
        .filter(|id| !id.text.is_empty())
        .map(|id| Text::<256>::new(id.text).map_err(|_| ReadError::InvalidSource))
        .transpose()?;
    let tool_pair_key = row
        .tool_pair_key
        .map(|key| Text::<256>::new(key).map_err(|_| ReadError::InvalidSource))
        .transpose()?;
    let pairing_state = match (&tool_pair_key, &tool_id_display) {
        (Some(key), Some(display)) if key == display => {
            if ambiguous_key {
                PairingStateV1::Ambiguous
            } else {
                PairingStateV1::Exact
            }
        }
        (None, Some(_)) => PairingStateV1::Oversized,
        (None, None) => PairingStateV1::Missing,
        _ => return Err(ReadError::InvalidSource),
    };
    let content_state = if row.offloaded {
        ContentStateV1::Offloaded
    } else if row.content.truncated {
        ContentStateV1::Preview
    } else {
        ContentStateV1::Complete
    };
    if row.content.observed_bytes < row.content.text.len() as u64
        || (!row.offloaded
            && !row.content.truncated
            && row.content.observed_bytes != row.content.text.len() as u64)
    {
        return Err(ReadError::InvalidSource);
    }
    Ok(HistoryEventV1 {
        id: DecimalI64::new(row.id.to_string()).map_err(|_| ReadError::InvalidSource)?,
        sequence: row.sequence,
        kind,
        role,
        created_at: Timestamp::new(row.created_at).map_err(|_| ReadError::InvalidSource)?,
        text: Text::<8192, false>::new(row.content.text).map_err(|_| ReadError::InvalidSource)?,
        content_bytes: DecimalU64::new(row.content.observed_bytes.to_string())
            .map_err(|_| ReadError::InvalidSource)?,
        truncated: row.content.truncated,
        tool_name,
        tool_pair_key,
        tool_id_display,
        pairing_state,
        content_state,
    })
}

#[cfg(all(
    test,
    any(not(feature = "test-shard-mode"), feature = "test-shard-store-04")
))]
mod tests {
    use super::*;
    use crate::remote_read::BoundedText;
    use crate::remote_read::NativeRuntimeApprovalRow;
    use rsi_common::remote_read::DecimalU64;
    use rsi_common::types::{QuestionItem, QuestionOption};

    fn row() -> SessionRow {
        SessionRow {
            id: Uuid::new_v4(),
            parent_id: None,
            continued_from: None,
            kind: "Standard".into(),
            provider: "OpenRouter".into(),
            status: "Running".into(),
            updated_at: "2026-09-27T00:00:00.000000000Z".into(),
            own_title: BoundedText {
                text: "A session".into(),
                observed_bytes: 9,
                truncated: false,
            },
        }
    }

    fn attention() -> AttentionV1 {
        AttentionV1 {
            requires_local_action: true,
            incomplete: true,
            live_signals_lower_bound: DecimalU64::new("0".into()).unwrap(),
        }
    }

    fn complete_pending_coverage() -> Vec<SourceCoverageV1> {
        PENDING_SOURCES
            .into_iter()
            .enumerate()
            .map(|(index, source)| SourceCoverageV1 {
                source,
                state: CoverageStateV1::Complete,
                has_more: false,
                lower_bound: DecimalU64::new("0".into()).unwrap(),
                observed_at: Timestamp::new("2026-09-27T00:00:00.000000000Z".into()).unwrap(),
                observation_order: (index + 1) as u32,
            })
            .collect()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn pending_response_status_preserves_source_and_selected_uncertainty() {
        let mut coverage = complete_pending_coverage();
        let none = SelectedDecisionV1::None {};
        let status = pending_response_status(&coverage, &[], &none).unwrap();
        assert!(status.complete);
        assert!(status.degraded.is_empty());

        coverage[0].state = CoverageStateV1::Limited;
        coverage[0].has_more = true;
        coverage[1].state = CoverageStateV1::Busy;
        coverage[4].state = CoverageStateV1::Unavailable;
        let status = pending_response_status(&coverage, &[], &none).unwrap();
        assert!(!status.complete);
        assert_eq!(
            status.degraded,
            vec![
                DegradationV1::Limited,
                DegradationV1::Busy,
                DegradationV1::Unavailable
            ]
        );
        let id = DecisionId::new(format!("native:{}", Uuid::new_v4())).unwrap();
        let unavailable = SelectedDecisionV1::Unavailable {
            id: id.clone(),
            last_display: None,
            reason: DegradationV1::SourceChanged,
        };
        let status = pending_response_status(&coverage, &[], &unavailable).unwrap();
        assert!(!status.complete);
        assert!(status.degraded.contains(&DegradationV1::SourceChanged));
        let tombstone = SelectedDecisionV1::Tombstone {
            id,
            identity_class: IdentityClassV1::Publication,
            last_display: None,
            message: Text::<64>::new("Decision no longer available".into()).unwrap(),
        };
        assert!(matches!(
            pending_response_status(&coverage, &[], &tombstone),
            Err(ReadError::InvalidSource)
        ));

        coverage = complete_pending_coverage();
        let mut row = approval_row();
        row.state = "Pending".into();
        row.tool_name = Some(BoundedText {
            text: "tool".into(),
            observed_bytes: 4,
            truncated: false,
        });
        let mut item = durable_approval_summary(
            PendingSource::LegacyApprovals,
            row,
            Timestamp::new("2026-09-27T00:00:00.000000000Z".into()).unwrap(),
        )
        .unwrap();
        item.disagreement = true;
        item.details_state = PreviewStateV1::Truncated;
        let status = pending_response_status(&coverage, &[item], &none).unwrap();
        assert!(!status.complete);
        assert_eq!(
            status.degraded,
            vec![DegradationV1::Disagreement, DegradationV1::Truncated]
        );
        coverage[2].observation_order = coverage[0].observation_order;
        assert!(matches!(
            pending_response_status(&coverage, &[], &none),
            Err(ReadError::InvalidSource)
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn pending_response_envelope_requires_truthful_cursor_and_degradation() {
        use rsi_common::remote_read::OpaqueToken;

        let project = Uuid::new_v4();
        let session = Uuid::new_v4();
        let epoch = Uuid::new_v4();
        let at = chrono::DateTime::parse_from_rfc3339("2026-09-27T00:00:00.000000000Z")
            .unwrap()
            .with_timezone(&Utc);
        let make = |coverage, items, selected, remaining, next_cursor| PendingResponseInputs {
            project,
            session,
            daemon_epoch: epoch,
            observed_at: at,
            mode: DecisionModeV1::Attention,
            page_limit: 16,
            items,
            selected,
            coverage,
            page_remaining_in_inputs: remaining,
            next_cursor,
        };
        let mut coverage = complete_pending_coverage();
        let mut row = approval_row();
        row.state = "Pending".into();
        row.tool_name = Some(BoundedText {
            text: "tool".into(),
            observed_bytes: 4,
            truncated: false,
        });
        let item = durable_approval_summary(
            PendingSource::LegacyApprovals,
            row,
            Timestamp::new("2026-09-27T00:00:00.000000000Z".into()).unwrap(),
        )
        .unwrap();
        coverage[7].lower_bound = DecimalU64::new("1".into()).unwrap();
        let response = pending_decisions_response(make(
            coverage.clone(),
            vec![item.clone()],
            SelectedDecisionV1::None {},
            false,
            None,
        ))
        .unwrap();
        assert!(response.complete);
        assert!(response.degraded.is_empty());
        assert_eq!(response.items[0].id, item.id);
        assert!(!response.items[0].can_answer);
        assert!(response.next_cursor.is_none());

        let cursor = || CursorV1::Decisions {
            token: OpaqueToken::new("opaque_test_token".into()).unwrap(),
        };
        assert!(matches!(
            pending_decisions_response(make(
                coverage.clone(),
                vec![item.clone()],
                SelectedDecisionV1::None {},
                true,
                None,
            )),
            Err(ReadError::InvalidSource)
        ));
        let continued = pending_decisions_response(make(
            coverage.clone(),
            vec![item],
            SelectedDecisionV1::None {},
            true,
            Some(cursor()),
        ))
        .unwrap();
        assert!(!continued.complete);
        assert_eq!(continued.degraded, vec![DegradationV1::Limited]);

        coverage[4].state = CoverageStateV1::Busy;
        let selected = SelectedDecisionV1::Unavailable {
            id: DecisionId::new(format!("native:{}", Uuid::new_v4())).unwrap(),
            last_display: None,
            reason: DegradationV1::Busy,
        };
        let degraded =
            pending_decisions_response(make(coverage, Vec::new(), selected, false, None)).unwrap();
        assert!(!degraded.complete);
        assert_eq!(degraded.degraded, vec![DegradationV1::Busy]);

        let mut paged = complete_pending_coverage();
        paged[0].state = CoverageStateV1::Limited;
        paged[0].has_more = true;
        assert!(matches!(
            pending_decisions_response(make(
                paged.clone(),
                Vec::new(),
                SelectedDecisionV1::None {},
                false,
                None,
            )),
            Err(ReadError::InvalidSource)
        ));
        let paged = pending_decisions_response(make(
            paged,
            Vec::new(),
            SelectedDecisionV1::None {},
            false,
            Some(cursor()),
        ))
        .unwrap();
        assert!(!paged.complete);
        assert_eq!(paged.degraded, vec![DegradationV1::Limited]);

        let mut busy = complete_pending_coverage();
        busy[4].state = CoverageStateV1::Busy;
        let foreign = SelectedDecisionV1::Unavailable {
            id: DecisionId::new(format!("question-slot:{}:7:tracked", Uuid::new_v4())).unwrap(),
            last_display: None,
            reason: DegradationV1::Busy,
        };
        assert!(matches!(
            pending_decisions_response(make(busy, Vec::new(), foreign, false, None)),
            Err(ReadError::InvalidSource)
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn pending_key_projection_joins_native_witness_and_preserves_filtered_rows() {
        use crate::remote_read::{NativeRuntimeApprovalRow, select_pending_union_keys};

        let id = |n: u8| Uuid::parse_str(&format!("00000000-0000-0000-0000-{n:012x}")).unwrap();
        let incarnation = Uuid::new_v4();
        let at = chrono::DateTime::parse_from_rfc3339("2026-09-27T00:00:00.000000000Z")
            .unwrap()
            .with_timezone(&Utc);
        let bounded = |text: &str| BoundedText {
            text: text.into(),
            observed_bytes: text.len() as u64,
            truncated: false,
        };
        let runtime = NativeRuntimeApprovalListSnapshot {
            session: Uuid::new_v4(),
            observed_at: at,
            state: NativeRuntimeApprovalListState::Present(vec![
                NativeRuntimeApprovalRow {
                    id: id(2),
                    incarnation_id: incarnation,
                    spawn_generation: 1,
                    method: Some(bounded("method")),
                    description: Some(bounded("description")),
                    resolution_observed: false,
                    resolution_persisted: false,
                    writer_live: true,
                    writer_capacity: 64,
                },
                NativeRuntimeApprovalRow {
                    id: id(3),
                    incarnation_id: incarnation,
                    spawn_generation: 1,
                    method: Some(bounded("method")),
                    description: Some(bounded("description")),
                    resolution_observed: false,
                    resolution_persisted: false,
                    writer_live: true,
                    writer_capacity: 64,
                },
            ]),
        };
        let key = |source, n, rowid| PendingCandidate {
            source,
            id: id(n),
            rowid,
        };
        let selection = select_pending_union_keys(
            &runtime,
            NativeRuntimeApprovalRecheck::NoObservedChange,
            0,
            &[
                key(PendingSource::Questions, 1, 1),
                key(PendingSource::Questions, 5, 5),
            ],
            &[key(PendingSource::NativePublications, 2, 2)],
            &[],
            &[key(PendingSource::LegacyApprovals, 4, 4)],
            0,
            5,
        )
        .unwrap();
        let mut rows = Vec::new();
        for candidate in &selection.items {
            let row = match *candidate {
                PendingUnionCandidate::Question(key) if key.id == id(5) => None,
                PendingUnionCandidate::Question(key) => {
                    let mut row = approval_row();
                    row.id = key.id;
                    row.details_unavailable = true;
                    Some(row)
                }
                PendingUnionCandidate::Native {
                    id,
                    durable: Some(_),
                    ..
                } => {
                    let mut row = approval_row();
                    row.id = id;
                    row.state = "enqueued".into();
                    row.closure_state = Some("open".into());
                    row.incarnation_id = Some(incarnation);
                    row.method = Some(bounded("method"));
                    row.description = Some(bounded("description"));
                    Some(row)
                }
                PendingUnionCandidate::Native { durable: None, .. } => None,
                PendingUnionCandidate::Legacy(key) => {
                    let mut row = approval_row();
                    row.id = key.id;
                    row.state = "Pending".into();
                    row.tool_name = Some(bounded("tool"));
                    Some(row)
                }
            };
            rows.push(row);
        }
        let projected = project_pending_union_keys(
            &selection,
            &rows,
            &runtime,
            NativeRuntimeApprovalRecheck::NoObservedChange,
            Timestamp::new("2026-09-27T00:00:00.000000000Z".into()).unwrap(),
        )
        .unwrap();
        assert_eq!(projected.items.len(), 4);
        assert_eq!(projected.filtered_durable, 1);
        let joined = projected
            .items
            .iter()
            .find(|item| item.id.as_str() == format!("native:{}", id(2)))
            .unwrap();
        assert_eq!(joined.source_observations.len(), 2);
        assert!(!joined.can_answer);
        assert!(
            projected
                .items
                .iter()
                .any(|item| item.id.as_str() == format!("native:{}", id(3)))
        );
        let native_position = selection
            .items
            .iter()
            .position(|candidate| candidate.id() == id(2))
            .unwrap();
        let mut filtered_mirror = rows.clone();
        filtered_mirror[native_position] = None;
        let runtime_only = project_pending_union_keys(
            &selection,
            &filtered_mirror,
            &runtime,
            NativeRuntimeApprovalRecheck::NoObservedChange,
            Timestamp::new("2026-09-27T00:00:00.000000000Z".into()).unwrap(),
        )
        .unwrap();
        assert_eq!(runtime_only.filtered_durable, 2);
        assert_eq!(runtime_only.items.len(), 4);
        assert_eq!(
            runtime_only
                .items
                .iter()
                .find(|item| item.id.as_str() == format!("native:{}", id(2)))
                .unwrap()
                .source_observations
                .len(),
            1
        );
        assert!(matches!(
            project_pending_union_keys(
                &selection,
                &rows,
                &runtime,
                NativeRuntimeApprovalRecheck::Changed,
                Timestamp::new("2026-09-27T00:00:00.000000000Z".into()).unwrap(),
            ),
            Err(ReadError::SourceUnavailable)
        ));
        rows[0].as_mut().unwrap().id = Uuid::new_v4();
        assert!(matches!(
            project_pending_union_keys(
                &selection,
                &rows,
                &runtime,
                NativeRuntimeApprovalRecheck::NoObservedChange,
                Timestamp::new("2026-09-27T00:00:00.000000000Z".into()).unwrap(),
            ),
            Err(ReadError::InvalidSource)
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn pending_page_scheduler_interleaves_all_captured_slots_without_losing_key_positions() {
        use crate::remote_read::NativeRuntimeApprovalRow;

        let id = |n: u8| Uuid::parse_str(&format!("00000000-0000-0000-0000-{n:012x}")).unwrap();
        let session = Uuid::new_v4();
        let at = chrono::DateTime::parse_from_rfc3339("2026-09-27T00:00:00.000000000Z")
            .unwrap()
            .with_timezone(&Utc);
        let incarnation = Uuid::new_v4();
        let native = |n| NativeRuntimeApprovalRow {
            id: id(n),
            incarnation_id: incarnation,
            spawn_generation: 7,
            method: None,
            description: None,
            resolution_observed: false,
            resolution_persisted: false,
            writer_live: true,
            writer_capacity: 64,
        };
        let runtime = NativeRuntimeApprovalListSnapshot {
            session,
            observed_at: at,
            state: NativeRuntimeApprovalListState::Present(vec![native(2), native(3)]),
        };
        let projection = question_unavailable().unwrap();
        let slots = RuntimeQuestionSlotListSnapshot {
            project: Uuid::new_v4(),
            session,
            active_observed_at: at,
            completed_observed_at: at,
            active_generation: Some(7),
            completed_found: true,
            slots: vec![
                RuntimeQuestionSlotListEntry {
                    generation: QuestionSlotGeneration::Spawn(7),
                    mirror: QuestionSlotMirror::Tracked,
                    projection: projection.clone(),
                },
                RuntimeQuestionSlotListEntry {
                    generation: QuestionSlotGeneration::Spawn(7),
                    mirror: QuestionSlotMirror::Session,
                    projection: projection.clone(),
                },
                RuntimeQuestionSlotListEntry {
                    generation: QuestionSlotGeneration::Completed,
                    mirror: QuestionSlotMirror::Session,
                    projection,
                },
            ],
        };
        let mut fallback = approval_row();
        fallback.id = session;
        fallback.state = "slot".into();
        let key = |source, n| PendingCandidate {
            source,
            id: id(n),
            rowid: n as i64,
        };
        let questions = [
            key(PendingSource::Questions, 1),
            key(PendingSource::Questions, 5),
        ];
        let native_publications = [key(PendingSource::NativePublications, 2)];
        let legacy = [key(PendingSource::LegacyApprovals, 4)];
        let inputs = || PendingPageInputs {
            runtime: &runtime,
            runtime_recheck: NativeRuntimeApprovalRecheck::NoObservedChange,
            questions: &questions,
            native_publications: &native_publications,
            native_historical: &[],
            legacy: &legacy,
            question_slots: &slots,
            question_slots_recheck: RuntimeQuestionSlotRecheck::NoObservedChange,
            fallback: Some(&fallback),
        };
        let start = PendingPagePosition {
            examined: [0; 5],
            key_bucket: 0,
            slot_offset: 0,
            page_bucket: 0,
        };
        let first = select_pending_page_keys(inputs(), start, 4).unwrap();
        assert_eq!(first.items.len(), 4);
        assert_eq!(
            first.items[0],
            PendingPageCandidate::Key(PendingUnionCandidate::Question(questions[0]))
        );
        assert_eq!(
            first.items[1],
            PendingPageCandidate::RuntimeSlot {
                generation: QuestionSlotGeneration::Spawn(7),
                mirror: QuestionSlotMirror::Tracked,
            }
        );
        assert_eq!(first.next.examined, [1, 1, 1, 0, 0]);
        assert_eq!(first.next.slot_offset, 2);
        assert!(first.remaining_in_inputs);

        let second = select_pending_page_keys(inputs(), first.next, 4).unwrap();
        assert_eq!(second.items.len(), 4);
        assert_eq!(
            second.items[1],
            PendingPageCandidate::RuntimeSlot {
                generation: QuestionSlotGeneration::Completed,
                mirror: QuestionSlotMirror::Session,
            }
        );
        assert_eq!(second.items[3], PendingPageCandidate::DurableFallback);
        assert_eq!(second.next.examined, [2, 1, 1, 0, 1]);
        assert_eq!(second.next.slot_offset, 4);
        assert!(second.remaining_in_inputs);

        let third = select_pending_page_keys(inputs(), second.next, 4).unwrap();
        assert_eq!(
            third.items,
            vec![PendingPageCandidate::Key(PendingUnionCandidate::Native {
                id: id(3),
                runtime: true,
                durable: None,
            })]
        );
        assert_eq!(third.next.examined, [2, 2, 1, 0, 1]);
        assert!(!third.remaining_in_inputs);
        assert!(
            matches!(select_pending_page_keys(inputs(), third.next, 4), Ok(empty) if empty.items.is_empty())
        );

        let mut changed = inputs();
        changed.question_slots_recheck = RuntimeQuestionSlotRecheck::Changed;
        assert!(matches!(
            select_pending_page_keys(changed, start, 1),
            Err(ReadError::SourceUnavailable)
        ));
        let mut changed = inputs();
        changed.runtime_recheck = NativeRuntimeApprovalRecheck::Changed;
        let slot_first = PendingPagePosition {
            page_bucket: 1,
            ..start
        };
        assert!(matches!(
            select_pending_page_keys(changed, slot_first, 1),
            Err(ReadError::SourceUnavailable)
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn pending_page_projection_retains_slot_and_fallback_identity_after_key_filtering() {
        let session = Uuid::new_v4();
        let question = Uuid::new_v4();
        let at = chrono::DateTime::parse_from_rfc3339("2026-09-27T00:00:00.000000000Z")
            .unwrap()
            .with_timezone(&Utc);
        let runtime = NativeRuntimeApprovalListSnapshot {
            session,
            observed_at: at,
            state: NativeRuntimeApprovalListState::Present(Vec::new()),
        };
        let slots = RuntimeQuestionSlotListSnapshot {
            project: Uuid::new_v4(),
            session,
            active_observed_at: at,
            completed_observed_at: at,
            active_generation: Some(7),
            completed_found: false,
            slots: vec![RuntimeQuestionSlotListEntry {
                generation: QuestionSlotGeneration::Spawn(7),
                mirror: QuestionSlotMirror::Tracked,
                projection: question_unavailable().unwrap(),
            }],
        };
        let mut question_row = approval_row();
        question_row.id = question;
        question_row.details_unavailable = true;
        let mut fallback = approval_row();
        fallback.id = session;
        fallback.state = "slot".into();
        fallback.details_unavailable = true;
        let selection = PendingPageSelection {
            items: vec![
                PendingPageCandidate::Key(PendingUnionCandidate::Question(PendingCandidate {
                    source: PendingSource::Questions,
                    id: question,
                    rowid: 1,
                })),
                PendingPageCandidate::RuntimeSlot {
                    generation: QuestionSlotGeneration::Spawn(7),
                    mirror: QuestionSlotMirror::Tracked,
                },
                PendingPageCandidate::DurableFallback,
            ],
            next: PendingPagePosition {
                examined: [1, 0, 0, 0, 0],
                key_bucket: 1,
                slot_offset: 2,
                page_bucket: 0,
            },
            remaining_in_inputs: false,
        };
        let observed_at = || Timestamp::new("2026-09-27T00:00:00.000000000Z".into()).unwrap();
        let project = |rows: &[Option<PendingSourceRow>],
                       slots: &RuntimeQuestionSlotListSnapshot,
                       fallback: &PendingSourceRow,
                       slot_recheck| {
            project_pending_page_candidates(
                &selection,
                rows,
                &runtime,
                NativeRuntimeApprovalRecheck::NoObservedChange,
                slots,
                slot_recheck,
                Some(fallback),
                observed_at(),
            )
        };
        let rows = [Some(question_row), None, None];
        let complete = project(
            &rows,
            &slots,
            &fallback,
            RuntimeQuestionSlotRecheck::NoObservedChange,
        )
        .unwrap();
        assert_eq!(complete.items.len(), 3);
        assert_eq!(complete.filtered_durable, 0);
        assert_eq!(
            complete.items[0].id.as_str(),
            format!("question:{question}")
        );
        assert_eq!(
            complete.items[1].id.as_str(),
            format!("question-slot:{session}:7:tracked")
        );
        assert_eq!(
            complete.items[2].id.as_str(),
            format!("question-fallback:{session}")
        );
        assert!(complete.items.iter().all(|item| !item.can_answer));
        let filtered = project(
            &[None, None, None],
            &slots,
            &fallback,
            RuntimeQuestionSlotRecheck::NoObservedChange,
        )
        .unwrap();
        assert_eq!(filtered.items.len(), 2);
        assert_eq!(filtered.filtered_durable, 1);
        let mut missing = slots;
        missing.slots.clear();
        assert!(matches!(
            project(
                &rows,
                &missing,
                &fallback,
                RuntimeQuestionSlotRecheck::NoObservedChange
            ),
            Err(ReadError::SourceUnavailable)
        ));
        assert!(matches!(
            project(
                &rows,
                &missing,
                &fallback,
                RuntimeQuestionSlotRecheck::Changed
            ),
            Err(ReadError::SourceUnavailable)
        ));
        fallback.id = Uuid::new_v4();
        assert!(matches!(
            project(
                &rows,
                &missing,
                &fallback,
                RuntimeQuestionSlotRecheck::NoObservedChange
            ),
            Err(ReadError::InvalidSource)
        ));
    }

    fn approval_row() -> PendingSourceRow {
        PendingSourceRow {
            id: Uuid::new_v4(),
            state: "unresolved".into(),
            closure_state: None,
            incarnation_id: None,
            epoch: None,
            method: None,
            description: None,
            tool_name: None,
            question_json: None,
            details_unavailable: false,
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn generic_questions_preserve_order_and_selection_with_explicit_omissions() {
        let source = PendingQuestion {
            questions: (0..9)
                .map(|index| QuestionItem {
                    question: format!("Choose {index}?"),
                    header: format!("Group {index}"),
                    options: (0..9)
                        .map(|option| QuestionOption {
                            label: format!("Option {option}"),
                            description: "説明".into(),
                        })
                        .collect(),
                    multi_select: index == 0,
                })
                .collect(),
        };
        let projection = project_questions(Some(&source)).unwrap();
        assert_eq!(projection.details_state, PreviewStateV1::Truncated);
        assert_eq!(projection.questions.len(), 8);
        assert_eq!(projection.omitted_questions.get(), 1);
        assert_eq!(projection.questions[0].question.as_str(), "Choose 0?");
        assert!(projection.questions[0].multi_select);
        assert_eq!(
            projection.questions[0].options[0].label.as_str(),
            "Option 0"
        );
        assert_eq!(projection.questions[0].omitted_options.get(), 1);
        assert!(serde_json::to_vec(&projection.questions).unwrap().len() <= 5_000);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn generic_questions_bound_large_unicode_runtime_text_and_reject_empty_required_fields() {
        let mut source = PendingQuestion {
            questions: vec![QuestionItem {
                question: "界".repeat(11_000_000),
                header: "見出し".repeat(100),
                options: vec![QuestionOption {
                    label: "選択".repeat(100),
                    description: "説明".repeat(1_000),
                }],
                multi_select: false,
            }],
        };
        let projection = project_questions(Some(&source)).unwrap();
        assert_eq!(projection.details_state, PreviewStateV1::Truncated);
        assert_eq!(projection.questions.len(), 1);
        assert_eq!(projection.questions[0].question.as_str().len(), 2046);
        assert!(projection.questions[0].question.as_str().ends_with('界'));
        assert!(serde_json::to_vec(&projection.questions).unwrap().len() <= 5_000);

        source.questions[0].options[0].label.clear();
        let unavailable = project_questions(Some(&source)).unwrap();
        assert_eq!(unavailable.details_state, PreviewStateV1::Unavailable);
        assert!(unavailable.questions.is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn durable_questions_keep_publication_and_fallback_identity_distinct() {
        let at = Timestamp::new("2026-09-27T00:00:00.000000000Z".into()).unwrap();
        let questions = PendingQuestion {
            questions: vec![QuestionItem {
                question: "Which checks?".into(),
                header: "Tests".into(),
                options: vec![QuestionOption {
                    label: "Unit".into(),
                    description: "Run unit checks".into(),
                }],
                multi_select: true,
            }],
        };
        let mut publication = approval_row();
        publication.state = "published".into();
        publication.question_json = Some(serde_json::to_string(&questions).unwrap());
        let summary = durable_question_summary(publication.clone(), false, at.clone()).unwrap();
        assert_eq!(summary.id.as_str(), format!("question:{}", publication.id));
        assert_eq!(summary.identity_class, IdentityClassV1::Publication);
        assert_eq!(summary.closure_state, ClosureStateV1::Open);
        assert_eq!(summary.questions[0].options[0].label.as_str(), "Unit");
        assert!(summary.questions[0].multi_select);
        assert!(summary.requires_local_action);
        assert!(!summary.can_answer);
        assert_eq!(
            summary.source_observations[0].source,
            SourceV1::QuestionPublications
        );

        let mut fallback = publication.clone();
        fallback.state = "slot".into();
        let slot = durable_question_summary(fallback, true, at.clone()).unwrap();
        assert_eq!(
            slot.id.as_str(),
            format!("question-fallback:{}", publication.id)
        );
        assert_eq!(slot.identity_class, IdentityClassV1::Slot);
        assert_eq!(slot.closure_state, ClosureStateV1::Unknown);
        assert!(slot.publication_state.is_none());
        assert_eq!(
            slot.source_observations[0].source,
            SourceV1::DurableQuestionFallback
        );

        publication.state = "cleared".into();
        let cleared = durable_question_summary(publication.clone(), false, at.clone()).unwrap();
        assert_eq!(cleared.closure_state, ClosureStateV1::Closed);
        assert!(!cleared.requires_local_action);
        publication.question_json = Some("{broken".into());
        let unreadable = durable_question_summary(publication, false, at).unwrap();
        assert_eq!(unreadable.details_state, PreviewStateV1::Unavailable);
        assert!(unreadable.questions.is_empty());
        assert!(unreadable.requires_local_action);

        let mut future = approval_row();
        future.state = "enqueued".into();
        let unknown = durable_question_summary(
            future,
            false,
            Timestamp::new("2026-09-27T00:00:00.000000000Z".into()).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            unknown.publication_state,
            Some(PublicationStateV1::Unknown { .. })
        ));
        assert_eq!(unknown.closure_state, ClosureStateV1::Unknown);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn runtime_question_slot_summary_keeps_generation_and_missing_source_unavailable() {
        let project = Uuid::new_v4();
        let session = Uuid::new_v4();
        let observed_at = chrono::DateTime::parse_from_rfc3339("2026-09-27T00:00:00.000000000Z")
            .unwrap()
            .with_timezone(&Utc);
        let question = PendingQuestion {
            questions: vec![QuestionItem {
                question: "Continue?".into(),
                header: "Choice".into(),
                options: Vec::new(),
                multi_select: false,
            }],
        };
        let snapshot = |generation, mirror, state| RuntimeQuestionSlotSnapshot {
            project,
            session,
            generation,
            mirror,
            observed_at,
            state,
        };
        let active = runtime_question_slot_summary(snapshot(
            QuestionSlotGeneration::Spawn(42),
            QuestionSlotMirror::Tracked,
            RuntimeQuestionSlotState::Present(project_questions(Some(&question)).unwrap()),
        ))
        .unwrap();
        assert_eq!(
            active.id.as_str(),
            format!("question-slot:{session}:42:tracked")
        );
        assert_eq!(active.identity_class, IdentityClassV1::Slot);
        assert_eq!(active.closure_state, ClosureStateV1::Unknown);
        assert_eq!(
            active.source_observations[0].source,
            SourceV1::TrackedQuestionSlot
        );
        assert_eq!(
            active.source_observations[0]
                .spawn_generation
                .as_ref()
                .unwrap()
                .get(),
            42
        );
        assert!(active.requires_local_action);
        assert!(!active.can_answer);
        let completed = runtime_question_slot_summary(snapshot(
            QuestionSlotGeneration::Completed,
            QuestionSlotMirror::Session,
            RuntimeQuestionSlotState::Present(project_questions(Some(&question)).unwrap()),
        ))
        .unwrap();
        assert_eq!(
            completed.id.as_str(),
            format!("question-slot:{session}:completed:session")
        );
        assert!(completed.source_observations[0].spawn_generation.is_none());
        assert!(matches!(
            runtime_question_slot_summary(snapshot(
                QuestionSlotGeneration::Spawn(42),
                QuestionSlotMirror::Tracked,
                RuntimeQuestionSlotState::Missing,
            )),
            Err(ReadError::SourceUnavailable)
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn runtime_native_summary_reports_witness_without_inventing_publication_or_authority() {
        let session = Uuid::new_v4();
        let selected = Uuid::new_v4();
        let incarnation = Uuid::new_v4();
        let observed_at = chrono::DateTime::parse_from_rfc3339("2026-09-27T00:00:00.000000000Z")
            .unwrap()
            .with_timezone(&Utc);
        let row = NativeRuntimeApprovalRow {
            id: selected,
            incarnation_id: incarnation,
            spawn_generation: 73,
            method: Some(BoundedText {
                text: "execute".into(),
                observed_bytes: 7,
                truncated: false,
            }),
            description: None,
            resolution_observed: true,
            resolution_persisted: false,
            writer_live: true,
            writer_capacity: 63,
        };
        let snapshot = |state| NativeRuntimeApprovalSnapshot {
            session,
            selected,
            observed_at,
            state,
        };
        let summary = runtime_native_approval_summary(snapshot(
            NativeRuntimeApprovalState::Present(row.clone()),
        ))
        .unwrap();
        assert_eq!(summary.id.as_str(), format!("native:{selected}"));
        assert_eq!(summary.identity_class, IdentityClassV1::Publication);
        assert_eq!(summary.kind, DecisionKindV1::NativeApproval);
        assert!(summary.publication_state.is_none());
        assert_eq!(summary.closure_state, ClosureStateV1::Unknown);
        assert_eq!(summary.delivery_state, DeliveryStateV1::Unknown);
        assert_eq!(
            summary.source_observations[0].source,
            SourceV1::NativeRuntime
        );
        assert_eq!(summary.source_observations[0].writer_capacity, Some(63));
        assert_eq!(
            summary.source_observations[0].resolution_observed,
            Some(true)
        );
        assert_eq!(
            summary.source_observations[0].resolution_persisted,
            Some(false)
        );
        assert_eq!(summary.details_state, PreviewStateV1::Unavailable);
        assert!(summary.requires_local_action);
        assert!(!summary.can_answer);
        assert!(matches!(
            runtime_native_approval_summary(snapshot(NativeRuntimeApprovalState::Missing)),
            Err(ReadError::SourceUnavailable)
        ));
        assert!(matches!(
            runtime_native_approval_summary(snapshot(NativeRuntimeApprovalState::SourceChanged)),
            Err(ReadError::SourceUnavailable)
        ));
        let mut wrong = row;
        wrong.id = Uuid::new_v4();
        assert!(matches!(
            runtime_native_approval_summary(snapshot(NativeRuntimeApprovalState::Present(wrong))),
            Err(ReadError::InvalidSource)
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn native_union_keeps_both_sources_and_exposes_conflicts() {
        let session = Uuid::new_v4();
        let selected = Uuid::new_v4();
        let incarnation = Uuid::new_v4();
        let observed_at = chrono::DateTime::parse_from_rfc3339("2026-09-27T00:00:00.000000000Z")
            .unwrap()
            .with_timezone(&Utc);
        let at = Timestamp::new("2026-09-27T00:00:00.000000000Z".into()).unwrap();
        let field = BoundedText {
            text: "execute".into(),
            observed_bytes: 7,
            truncated: false,
        };
        let witness = NativeRuntimeApprovalRow {
            id: selected,
            incarnation_id: incarnation,
            spawn_generation: 73,
            method: Some(field.clone()),
            description: Some(field.clone()),
            resolution_observed: false,
            resolution_persisted: false,
            writer_live: true,
            writer_capacity: 63,
        };
        let snapshot = |row| NativeRuntimeApprovalSnapshot {
            session,
            selected,
            observed_at,
            state: NativeRuntimeApprovalState::Present(row),
        };
        let mut durable = approval_row();
        durable.id = selected;
        durable.state = "published".into();
        durable.incarnation_id = Some(incarnation);
        durable.method = Some(field.clone());
        durable.description = Some(field);
        let joined = native_approval_union(
            snapshot(witness.clone()),
            NativeRuntimeApprovalRecheck::NoObservedChange,
            PendingSource::NativePublications,
            durable.clone(),
            at.clone(),
        )
        .unwrap();
        assert_eq!(joined.source_observations.len(), 2);
        assert_eq!(
            joined.source_observations[0].source,
            SourceV1::NativeRuntime
        );
        assert_eq!(
            joined.source_observations[1].source,
            SourceV1::NativePublications
        );
        assert!(!joined.disagreement);
        assert_eq!(joined.details_state, PreviewStateV1::Complete);
        assert!(joined.publication_state.is_some());
        assert!(joined.requires_local_action);
        assert!(!joined.can_answer);

        let mut conflict = durable.clone();
        conflict.incarnation_id = Some(Uuid::new_v4());
        conflict.method = None;
        let ambiguous = native_approval_union(
            snapshot(witness.clone()),
            NativeRuntimeApprovalRecheck::NoObservedChange,
            PendingSource::NativePublications,
            conflict,
            at.clone(),
        )
        .unwrap();
        assert!(ambiguous.disagreement);
        assert_eq!(ambiguous.closure_state, ClosureStateV1::Ambiguous);
        assert!(ambiguous.publication_state.is_none());
        assert_eq!(ambiguous.details_state, PreviewStateV1::Unavailable);
        assert!(
            ambiguous.source_observations[0]
                .display_alternative
                .is_some()
        );

        assert!(matches!(
            native_approval_union(
                snapshot(witness.clone()),
                NativeRuntimeApprovalRecheck::Changed,
                PendingSource::NativePublications,
                durable.clone(),
                at.clone(),
            ),
            Err(ReadError::SourceUnavailable)
        ));
        durable.id = Uuid::new_v4();
        assert!(matches!(
            native_approval_union(
                snapshot(witness),
                NativeRuntimeApprovalRecheck::NoObservedChange,
                PendingSource::NativePublications,
                durable,
                at,
            ),
            Err(ReadError::SourceUnavailable)
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn native_and_legacy_approval_display_preserve_distinct_named_fields() {
        let mut native = approval_row();
        native.method = Some(BoundedText {
            text: "commandExecution/approval".into(),
            observed_bytes: 25,
            truncated: false,
        });
        native.description = Some(BoundedText {
            text: "Run tests".into(),
            observed_bytes: 9,
            truncated: false,
        });
        let projected = approval_display(PendingSource::NativePublications, native).unwrap();
        let DecisionDisplayV1::NativeApproval {
            method,
            description,
        } = projected
        else {
            panic!("expected native approval display")
        };
        assert_eq!(method.text.as_str(), "commandExecution/approval");
        assert_eq!(description.text.as_str(), "Run tests");
        assert_eq!(method.source_extent, SourceExtentV1::BoundedSnapshot);
        assert_eq!(description.source_extent, SourceExtentV1::BoundedSnapshot);

        let missing = approval_display(PendingSource::NativeHistorical, approval_row()).unwrap();
        let DecisionDisplayV1::NativeApproval {
            method,
            description,
        } = missing
        else {
            panic!("expected native fallback display")
        };
        assert_eq!(method.text.as_str(), "Native approval — method unavailable");
        assert_eq!(description.text.as_str(), "Description unavailable");
        assert_eq!(method.state, PreviewStateV1::Unavailable);
        assert_eq!(method.source_extent, SourceExtentV1::BoundedSnapshot);

        let mut legacy = approval_row();
        legacy.tool_name = Some(BoundedText {
            text: "Read".into(),
            observed_bytes: 4,
            truncated: false,
        });
        let DecisionDisplayV1::LegacyApproval { tool_name } =
            approval_display(PendingSource::LegacyApprovals, legacy).unwrap()
        else {
            panic!("expected legacy approval display")
        };
        assert_eq!(tool_name.text.as_str(), "Read");
        assert_eq!(tool_name.source_extent, SourceExtentV1::FullField);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn approval_display_keeps_unicode_preview_and_rejects_bad_extent_metadata() {
        let mut native = approval_row();
        native.method = Some(crate::remote_read::bounded_text("界".repeat(300), 900, 512).unwrap());
        let DecisionDisplayV1::NativeApproval { method, .. } =
            approval_display(PendingSource::NativePublications, native).unwrap()
        else {
            panic!("expected native approval display")
        };
        assert_eq!(method.state, PreviewStateV1::Truncated);
        assert_eq!(method.text.as_str().len(), 510);
        assert_eq!(method.observed_bytes.as_ref().unwrap().get(), 900);

        let mut invalid = approval_row();
        invalid.tool_name = Some(BoundedText {
            text: "Read".into(),
            observed_bytes: 3,
            truncated: false,
        });
        assert!(matches!(
            approval_display(PendingSource::LegacyApprovals, invalid),
            Err(ReadError::InvalidSource)
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn durable_approval_summary_keeps_closure_delivery_and_local_inspection_distinct() {
        let at = Timestamp::new("2026-09-27T00:00:00.000000000Z".into()).unwrap();
        let mut native = approval_row();
        native.state = "enqueued".into();
        native.closure_state = Some("open".into());
        native.incarnation_id = Some(Uuid::new_v4());
        native.method = Some(BoundedText {
            text: "commandExecution/approval".into(),
            observed_bytes: 25,
            truncated: false,
        });
        native.description = Some(BoundedText {
            text: "Run tests".into(),
            observed_bytes: 9,
            truncated: false,
        });
        let summary = durable_approval_summary(
            PendingSource::NativePublications,
            native.clone(),
            at.clone(),
        )
        .unwrap();
        assert_eq!(summary.kind, DecisionKindV1::NativeApproval);
        assert_eq!(summary.delivery_state, DeliveryStateV1::Enqueued);
        assert_eq!(summary.closure_state, ClosureStateV1::Open);
        assert!(summary.requires_local_action);
        assert!(!summary.can_answer);
        assert_eq!(
            summary.source_observations[0].source,
            SourceV1::NativePublications
        );
        assert!(summary.source_observations[0].writer_live.is_none());

        native.state = "superseded".into();
        native.closure_state = None;
        let historical =
            durable_approval_summary(PendingSource::NativeHistorical, native, at.clone()).unwrap();
        assert_eq!(historical.closure_state, ClosureStateV1::Closed);
        assert!(!historical.requires_local_action);
        assert_eq!(
            historical.source_observations[0].source,
            SourceV1::NativeHistoricalFallback
        );

        let mut legacy = approval_row();
        legacy.state = "Pending".into();
        legacy.tool_name = Some(BoundedText {
            text: "Read".into(),
            observed_bytes: 4,
            truncated: false,
        });
        let waiting =
            durable_approval_summary(PendingSource::LegacyApprovals, legacy.clone(), at.clone())
                .unwrap();
        assert_eq!(waiting.closure_state, ClosureStateV1::Open);
        assert!(waiting.requires_local_action);
        legacy.state = "Approved".into();
        let approved =
            durable_approval_summary(PendingSource::LegacyApprovals, legacy, at).unwrap();
        assert_eq!(approved.closure_state, ClosureStateV1::Closed);
        assert!(!approved.requires_local_action);

        let mut future = approval_row();
        future.state = "FutureState".into();
        future.tool_name = Some(BoundedText {
            text: "Read".into(),
            observed_bytes: 4,
            truncated: false,
        });
        let unknown = durable_approval_summary(
            PendingSource::LegacyApprovals,
            future,
            Timestamp::new("2026-09-27T00:00:00.000000000Z".into()).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            unknown.publication_state,
            Some(PublicationStateV1::Unknown { .. })
        ));
        assert_eq!(unknown.closure_state, ClosureStateV1::Unknown);
        assert!(unknown.requires_local_action);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn pending_coverage_uses_eight_bounded_sources_without_false_empty_or_tombstone() {
        let at = chrono::DateTime::parse_from_rfc3339("2026-09-27T00:00:00.000000000Z")
            .unwrap()
            .with_timezone(&Utc);
        let empty = PendingKeyPage {
            items: Vec::new(),
            next: None,
            has_more: false,
        };
        let question_slots = RuntimeQuestionSlotListSnapshot {
            project: Uuid::new_v4(),
            session: Uuid::new_v4(),
            active_observed_at: at,
            completed_observed_at: at,
            active_generation: Some(0),
            completed_found: false,
            slots: Vec::new(),
        };
        let native = NativeRuntimeApprovalListSnapshot {
            session: question_slots.session,
            observed_at: at,
            state: NativeRuntimeApprovalListState::Present(Vec::new()),
        };
        let complete = pending_source_coverage(PendingCoverageInputs {
            questions: PendingRead::Observed { value: &empty, at },
            question_slots: PendingRead::Observed {
                value: (
                    &question_slots,
                    RuntimeQuestionSlotRecheck::NoObservedChange,
                ),
                at,
            },
            fallback: PendingRead::Observed { value: None, at },
            native_runtime: PendingRead::Observed {
                value: (&native, NativeRuntimeApprovalRecheck::NoObservedChange),
                at,
            },
            native_publications: PendingRead::Observed { value: &empty, at },
            native_historical: PendingRead::Observed { value: &empty, at },
            legacy: PendingRead::Observed { value: &empty, at },
        })
        .unwrap();
        assert_eq!(complete.len(), 8);
        assert!(complete.iter().all(|row| {
            row.state == CoverageStateV1::Complete && !row.has_more && row.lower_bound.get() == 0
        }));
        let running = SessionStatusV1::Known {
            value: KnownSessionStatusV1::Running,
        };
        assert!(
            !session_attention(&running, 0, &complete)
                .unwrap()
                .incomplete
        );

        let question_id = Uuid::new_v4();
        let partial = PendingKeyPage {
            items: vec![PendingCandidate {
                source: PendingSource::Questions,
                id: question_id,
                rowid: 1,
            }],
            next: Some(PendingKey::Id(question_id)),
            has_more: true,
        };
        let missing_native = NativeRuntimeApprovalListSnapshot {
            session: question_slots.session,
            observed_at: at,
            state: NativeRuntimeApprovalListState::Missing,
        };
        let incomplete = pending_source_coverage(PendingCoverageInputs {
            questions: PendingRead::Observed {
                value: &partial,
                at,
            },
            question_slots: PendingRead::Busy { at },
            fallback: PendingRead::Observed { value: None, at },
            native_runtime: PendingRead::Observed {
                value: (&missing_native, NativeRuntimeApprovalRecheck::Changed),
                at,
            },
            native_publications: PendingRead::Observed { value: &empty, at },
            native_historical: PendingRead::Unavailable { at },
            legacy: PendingRead::Observed { value: &empty, at },
        })
        .unwrap();
        assert_eq!(incomplete[0].state, CoverageStateV1::Limited);
        assert!(incomplete[0].has_more);
        assert_eq!(incomplete[0].lower_bound.get(), 1);
        assert_eq!(incomplete[1].state, CoverageStateV1::Busy);
        assert_eq!(incomplete[2].state, CoverageStateV1::Busy);
        assert_eq!(incomplete[4].state, CoverageStateV1::Unavailable);
        assert_eq!(incomplete[6].state, CoverageStateV1::Unavailable);
        assert!(
            session_attention(&running, 0, &incomplete)
                .unwrap()
                .incomplete
        );
        let selected = DecisionId::new(format!("question:{question_id}")).unwrap();
        assert!(matches!(
            missing_selected_decision(selected, &SelectedPendingSource::Missing, &incomplete, None),
            Ok(SelectedDecisionV1::Unavailable { .. })
        ));
        let malformed = PendingKeyPage {
            items: Vec::new(),
            next: Some(PendingKey::Id(question_id)),
            has_more: true,
        };
        assert!(matches!(
            coverage_from_key_page(
                SourceV1::QuestionPublications,
                PendingSource::Questions,
                PendingRead::Observed {
                    value: &malformed,
                    at
                },
                1,
            ),
            Err(ReadError::InvalidSource)
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn pending_store_attempts_feed_eight_coverage_rows_and_preserve_source_failures() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions(id TEXT PRIMARY KEY,project_id TEXT,status TEXT,is_eval INTEGER,pending_question_json TEXT);
             CREATE TABLE pending_question_publications(publication_id TEXT,session_id TEXT,state TEXT,epoch INTEGER,question_json TEXT);
             CREATE TABLE appserver_approval_publications(publication_id TEXT,session_id TEXT);
             CREATE INDEX appserver_approval_publications_session ON appserver_approval_publications(session_id,publication_id);
             CREATE TABLE pending_appserver_approvals(publication_id TEXT,session_id TEXT);
             CREATE TABLE approvals(id TEXT,session_id TEXT);
             CREATE INDEX idx_approvals_session_id ON approvals(session_id);",
        )
        .unwrap();
        let project = Uuid::new_v4();
        let session = Uuid::new_v4();
        let q1 = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let q2 = Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap();
        conn.execute(
            "INSERT INTO sessions(id,project_id,status,is_eval) VALUES(?1,?2,'Running',0)",
            rusqlite::params![session.to_string(), project.to_string()],
        )
        .unwrap();
        for id in [q1, q2] {
            conn.execute(
                "INSERT INTO pending_question_publications(publication_id,session_id,state,epoch) VALUES(?1,?2,'open',1)",
                rusqlite::params![id.to_string(), session.to_string()],
            )
            .unwrap();
        }
        let sources = pending_store_sources(&conn, project, session, [None; 4], 1).unwrap();
        assert!(matches!(
            &sources.questions,
            PendingRead::Observed { value, .. } if value.items.len() == 1 && value.has_more && value.next == Some(PendingKey::Id(q1))
        ));
        let at = chrono::DateTime::parse_from_rfc3339("2026-09-27T00:00:00.000000000Z")
            .unwrap()
            .with_timezone(&Utc);
        let slots = RuntimeQuestionSlotListSnapshot {
            project,
            session,
            active_observed_at: at,
            completed_observed_at: at,
            active_generation: None,
            completed_found: false,
            slots: Vec::new(),
        };
        let native = NativeRuntimeApprovalListSnapshot {
            session,
            observed_at: at,
            state: NativeRuntimeApprovalListState::Present(Vec::new()),
        };
        let coverage = sources
            .coverage(
                PendingRead::Observed {
                    value: (&slots, RuntimeQuestionSlotRecheck::NoObservedChange),
                    at,
                },
                PendingRead::Observed {
                    value: (&native, NativeRuntimeApprovalRecheck::NoObservedChange),
                    at,
                },
            )
            .unwrap();
        assert_eq!(coverage.len(), 8);
        assert_eq!(coverage[0].state, CoverageStateV1::Limited);
        assert!(coverage[0].has_more);
        assert_eq!(coverage[0].lower_bound.get(), 1);
        assert!(
            coverage[1..]
                .iter()
                .all(|row| row.state == CoverageStateV1::Complete)
        );
        let tx = conn.unchecked_transaction().unwrap();
        let in_tx = pending_store_sources(&tx, project, session, [None; 4], 1).unwrap();
        let prepared = prepare_pending_page(
            &tx,
            project,
            session,
            &in_tx,
            &native,
            &slots,
            [None; 4],
            PendingPagePosition {
                examined: [0; 5],
                key_bucket: 0,
                slot_offset: 0,
                page_bucket: 0,
            },
            DecisionModeV1::Retained,
            1,
        )
        .unwrap();
        assert_eq!(prepared.selection.items.len(), 1);
        assert_eq!(prepared.hydrated[0].as_ref().unwrap().id, q1);
        assert!(prepared.durable_observed_at >= at);
        let (prepared_after, _) = prepared
            .next_position
            .resume(&native, &slots, false)
            .unwrap();
        assert_eq!(prepared_after[0], Some(PendingKey::Id(q1)));
        let prepared_coverage = in_tx
            .coverage(
                PendingRead::Observed {
                    value: (&slots, RuntimeQuestionSlotRecheck::NoObservedChange),
                    at,
                },
                PendingRead::Observed {
                    value: (&native, NativeRuntimeApprovalRecheck::NoObservedChange),
                    at,
                },
            )
            .unwrap();
        tx.rollback().unwrap();
        let mut acquired = PendingAcquiredSources {
            store: in_tx,
            question_slots: PendingRead::Observed {
                value: (slots, RuntimeQuestionSlotRecheck::NoObservedChange),
                at,
            },
            native_runtime: PendingRead::Observed {
                value: (native, NativeRuntimeApprovalRecheck::NoObservedChange),
                at,
            },
            coverage: prepared_coverage,
        };
        let finished = prepared.finish(&acquired).unwrap();
        assert_eq!(finished.projection.items.len(), 1);
        assert!(!finished.remaining_in_inputs);
        let selected = DecisionId::new(format!("question:{q1}")).unwrap();
        let exact = super::selected_saved_decision(
            selected.clone(),
            super::super::selected_pending_source(&conn, project, session, &selected).unwrap(),
            &acquired,
            Timestamp::new(Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            exact,
            SelectedDecisionV1::Present { decision, stale: false } if decision.id == selected
        ));
        let missing = DecisionId::new(format!("native:{}", Uuid::new_v4())).unwrap();
        assert!(matches!(
            super::selected_saved_decision(
                missing.clone(),
                SelectedPendingSource::Missing,
                &acquired,
                Timestamp::new(Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)).unwrap(),
            ),
            Ok(SelectedDecisionV1::Unavailable { id, .. }) if id == missing
        ));
        let slot_id = DecisionId::new(format!("question-slot:{session}:7:tracked")).unwrap();
        assert!(matches!(
            super::selected_saved_decision(
                slot_id.clone(),
                SelectedPendingSource::RuntimeSlot {
                    generation: QuestionSlotGeneration::Spawn(7),
                    mirror: QuestionSlotMirror::Tracked,
                },
                &acquired,
                Timestamp::new(Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)).unwrap(),
            ),
            Ok(SelectedDecisionV1::Unavailable { id, reason: DegradationV1::SourceChanged, .. }) if id == slot_id
        ));
        if let PendingRead::Observed {
            value: (_, recheck),
            ..
        } = &mut acquired.native_runtime
        {
            *recheck = NativeRuntimeApprovalRecheck::Changed;
        }
        let stale = PendingPreparedPage {
            selection: PendingPageSelection {
                items: Vec::new(),
                next: PendingPagePosition {
                    examined: [0; 5],
                    key_bucket: 0,
                    slot_offset: 0,
                    page_bucket: 0,
                },
                remaining_in_inputs: false,
            },
            hydrated: Vec::new(),
            next_position: finished.next_position,
            durable_observed_at: Utc::now(),
        };
        assert!(matches!(
            stale.finish(&acquired),
            Err(ReadError::SourceUnavailable)
        ));
        let next = pending_store_sources(
            &conn,
            project,
            session,
            [Some(PendingKey::Id(q1)), None, None, None],
            1,
        )
        .unwrap();
        assert!(matches!(
            &next.questions,
            PendingRead::Observed { value, .. } if value.items.len() == 1 && value.items[0].id == q2 && !value.has_more
        ));
        let fallback = pending_store_fallback(&conn, project, session).unwrap();
        let fallback_at = pending_read_at(&fallback);
        let resumed = pending_store_sources_with_fallback(
            &conn,
            project,
            session,
            [Some(PendingKey::Id(q1)), None, None, None],
            1,
            fallback,
        )
        .unwrap();
        assert_eq!(pending_read_at(&resumed.fallback), fallback_at);
        assert!(matches!(
            &resumed.questions,
            PendingRead::Observed { value, .. } if value.items.len() == 1 && value.items[0].id == q2
        ));
        assert!(matches!(
            pending_store_sources(&conn, Uuid::new_v4(), session, [None; 4], 1),
            Err(ReadError::NotFound)
        ));
        conn.execute(
            "UPDATE pending_question_publications SET publication_id='!' WHERE publication_id=?1",
            [q1.to_string()],
        )
        .unwrap();
        assert!(matches!(
            pending_store_sources(&conn, project, session, [None; 4], 1),
            Err(ReadError::InvalidSource)
        ));
        assert!(matches!(
            pending_attempt::<()>(|| Err(ReadError::Busy)),
            Ok(PendingRead::Busy { .. })
        ));
        assert!(matches!(
            pending_attempt::<()>(|| Err(ReadError::SourceUnavailable)),
            Ok(PendingRead::Unavailable { .. })
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn pending_acquisition_orders_runtime_store_rereads_and_keeps_busy_distinct() {
        use std::cell::RefCell;

        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions(id TEXT PRIMARY KEY,project_id TEXT,status TEXT,is_eval INTEGER,pending_question_json TEXT);
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
            "INSERT INTO sessions(id,project_id,status,is_eval) VALUES(?1,?2,'Running',0)",
            rusqlite::params![session.to_string(), project.to_string()],
        )
        .unwrap();
        let at = chrono::DateTime::parse_from_rfc3339("2026-09-27T00:00:00.000000000Z")
            .unwrap()
            .with_timezone(&Utc);
        let slots = |project, session| RuntimeQuestionSlotListSnapshot {
            project,
            session,
            active_observed_at: at,
            completed_observed_at: at,
            active_generation: None,
            completed_found: false,
            slots: Vec::new(),
        };
        let native = |session| NativeRuntimeApprovalListSnapshot {
            session,
            observed_at: at,
            state: NativeRuntimeApprovalListState::Present(Vec::new()),
        };
        let calls = RefCell::new(Vec::new());
        let acquired = acquire_pending_sources(
            &conn,
            project,
            session,
            [None; 4],
            1,
            |p, s| {
                calls.borrow_mut().push("capture_slots");
                Ok(slots(p, s))
            },
            |s| {
                calls.borrow_mut().push("capture_native");
                Ok(native(s))
            },
            |_| {
                calls.borrow_mut().push("recheck_slots");
                Ok(RuntimeQuestionSlotListRecheckObservation {
                    state: RuntimeQuestionSlotRecheck::NoObservedChange,
                    active_observed_at: at,
                    completed_observed_at: at,
                })
            },
            |_| {
                calls.borrow_mut().push("recheck_native");
                Ok(NativeRuntimeApprovalListRecheckObservation {
                    state: NativeRuntimeApprovalRecheck::NoObservedChange,
                    observed_at: at,
                })
            },
        )
        .unwrap();
        assert_eq!(
            *calls.borrow(),
            [
                "capture_slots",
                "capture_native",
                "recheck_slots",
                "recheck_native"
            ]
        );
        assert_eq!(acquired.coverage.len(), 8);
        assert!(
            acquired
                .coverage
                .iter()
                .all(|row| row.state == CoverageStateV1::Complete)
        );
        assert_eq!(
            acquired.coverage[1].observed_at.as_str(),
            "2026-09-27T00:00:00.000000000Z"
        );
        assert_eq!(
            acquired.coverage[4].observed_at.as_str(),
            "2026-09-27T00:00:00.000000000Z"
        );
        assert!(matches!(
            acquired.question_slots,
            PendingRead::Observed { .. }
        ));

        calls.borrow_mut().clear();
        let degraded = acquire_pending_sources(
            &conn,
            project,
            session,
            [None; 4],
            1,
            |_, _| {
                calls.borrow_mut().push("capture_slots");
                Err(ReadError::Busy)
            },
            |s| {
                calls.borrow_mut().push("capture_native");
                Ok(native(s))
            },
            |_| panic!("busy capture must not be reread"),
            |_| {
                calls.borrow_mut().push("recheck_native");
                Ok(NativeRuntimeApprovalListRecheckObservation {
                    state: NativeRuntimeApprovalRecheck::Changed,
                    observed_at: at,
                })
            },
        )
        .unwrap();
        assert_eq!(
            *calls.borrow(),
            ["capture_slots", "capture_native", "recheck_native"]
        );
        assert_eq!(degraded.coverage[1].state, CoverageStateV1::Busy);
        assert_eq!(degraded.coverage[2].state, CoverageStateV1::Busy);
        assert_eq!(degraded.coverage[4].state, CoverageStateV1::Unavailable);
        assert!(matches!(degraded.question_slots, PendingRead::Busy { .. }));
        assert!(matches!(
            acquire_pending_sources(
                &conn,
                Uuid::new_v4(),
                session,
                [None; 4],
                1,
                |_, _| panic!("scope denial must precede capture"),
                |_| panic!("scope denial must precede capture"),
                |_| panic!("scope denial must precede reread"),
                |_| panic!("scope denial must precede reread"),
            ),
            Err(ReadError::NotFound)
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn attention_keeps_partial_pending_visible_without_inventing_live_count() {
        let running = SessionStatusV1::Known {
            value: KnownSessionStatusV1::Running,
        };
        let mut coverage = complete_pending_coverage();
        assert!(
            !session_attention(&running, 0, &coverage)
                .unwrap()
                .requires_local_action
        );
        assert!(
            !session_attention(&running, 0, &coverage)
                .unwrap()
                .incomplete
        );
        let known_live = session_attention(&running, 2, &coverage).unwrap();
        assert!(known_live.requires_local_action);
        assert!(!known_live.incomplete);
        assert_eq!(known_live.live_signals_lower_bound.get(), 2);

        coverage[4].has_more = true;
        let paged = session_attention(&running, 0, &coverage).unwrap();
        assert!(paged.requires_local_action && paged.incomplete);
        assert_eq!(paged.live_signals_lower_bound.get(), 0);
        coverage[4].has_more = false;
        coverage[4].state = CoverageStateV1::Busy;
        let busy = session_attention(&running, 0, &coverage).unwrap();
        assert!(busy.requires_local_action && busy.incomplete);
        coverage.pop();
        let missing = session_attention(&running, 0, &coverage).unwrap();
        assert!(missing.requires_local_action && missing.incomplete);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn attention_keeps_failed_unknown_status_and_rejects_duplicate_sources() {
        let coverage = complete_pending_coverage();
        let failed = SessionStatusV1::Known {
            value: KnownSessionStatusV1::Failed,
        };
        assert!(
            session_attention(&failed, 0, &coverage)
                .unwrap()
                .requires_local_action
        );
        let unknown = SessionStatusV1::Unknown {
            label: Text::<128>::new("FutureStatus".into()).unwrap(),
        };
        assert!(
            session_attention(&unknown, 0, &coverage)
                .unwrap()
                .requires_local_action
        );
        let mut duplicate = coverage;
        duplicate[1].source = duplicate[0].source;
        assert!(matches!(
            session_attention(&failed, 0, &duplicate),
            Err(ReadError::InvalidSource)
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn selected_miss_needs_exhausted_coverage_before_tombstone() {
        let id = Uuid::new_v4();
        let selected = DecisionId::new(format!("native:{id}")).unwrap();
        let mut coverage = complete_pending_coverage();
        let tombstone = missing_selected_decision(
            selected.clone(),
            &SelectedPendingSource::Missing,
            &coverage,
            None,
        )
        .unwrap();
        assert!(matches!(
            tombstone,
            SelectedDecisionV1::Tombstone {
                identity_class: IdentityClassV1::Publication,
                ..
            }
        ));

        coverage[4].state = CoverageStateV1::Busy;
        let busy = missing_selected_decision(
            selected.clone(),
            &SelectedPendingSource::Missing,
            &coverage,
            None,
        )
        .unwrap();
        assert!(matches!(
            busy,
            SelectedDecisionV1::Unavailable {
                reason: DegradationV1::Busy,
                ..
            }
        ));
        coverage[4].state = CoverageStateV1::Complete;
        coverage[4].has_more = true;
        let paged = missing_selected_decision(
            selected.clone(),
            &SelectedPendingSource::Missing,
            &coverage,
            None,
        )
        .unwrap();
        assert!(matches!(
            paged,
            SelectedDecisionV1::Unavailable {
                reason: DegradationV1::Limited,
                ..
            }
        ));
        coverage[4].has_more = false;
        coverage.pop();
        assert!(matches!(
            missing_selected_decision(
                selected.clone(),
                &SelectedPendingSource::Missing,
                &coverage,
                None,
            ),
            Ok(SelectedDecisionV1::Unavailable { .. })
        ));
        assert!(matches!(
            missing_selected_decision(
                selected,
                &SelectedPendingSource::RuntimeSlot {
                    generation: crate::remote_read::QuestionSlotGeneration::Completed,
                    mirror: crate::remote_read::QuestionSlotMirror::Session,
                },
                &coverage,
                None,
            ),
            Err(ReadError::InvalidSource)
        ));
    }

    fn history_row() -> HistoryRow {
        HistoryRow {
            id: 42,
            sequence: 7,
            event_type: "ToolUse".into(),
            role: Some("Assistant".into()),
            created_at: "2026-09-27T00:00:00.000000000Z".into(),
            content: BoundedText {
                text: "run".into(),
                observed_bytes: 3,
                truncated: false,
            },
            tool_name: Some(BoundedText {
                text: "shell".into(),
                observed_bytes: 5,
                truncated: false,
            }),
            tool_pair_key: Some("call-42".into()),
            tool_id_display: Some(BoundedText {
                text: "call-42".into(),
                observed_bytes: 7,
                truncated: false,
            }),
            offloaded: false,
        }
    }

    fn detail_source() -> SessionDetailSource {
        SessionDetailSource {
            own_title: BoundedText {
                text: "A session with detail".into(),
                observed_bytes: 21,
                truncated: false,
            },
            query: Some(BoundedText {
                text: "🙂".into(),
                observed_bytes: 8000,
                truncated: true,
            }),
            model: None,
            saved_history_head: Some((7, i64::MAX)),
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn project_projection_keeps_canonical_id_and_rejects_empty_name() {
        let id = Uuid::new_v4();
        let row = ProjectRow {
            id,
            name: BoundedText {
                text: "My project".into(),
                observed_bytes: 10,
                truncated: false,
            },
        };
        let projected = project(row.clone()).unwrap();
        assert_eq!(projected.id.as_str(), id.to_string());
        assert!(
            serde_json::from_value::<ProjectV1>(serde_json::to_value(projected).unwrap()).is_ok()
        );
        let mut invalid = row;
        invalid.name.text.clear();
        assert!(project(invalid).is_err());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn session_projection_preserves_known_and_unknown_labels() {
        let project = Uuid::new_v4();
        let summary = session_summary(row(), project, attention()).unwrap();
        assert!(matches!(
            &summary.provider,
            ProviderV1::Known {
                value: KnownProviderV1::OpenRouter
            }
        ));
        let round_trip = serde_json::to_value(&summary).unwrap();
        assert!(serde_json::from_value::<SessionSummaryV1>(round_trip).is_ok());

        let mut unknown = row();
        unknown.provider = "NewProvider".into();
        let summary = session_summary(unknown, project, attention()).unwrap();
        assert!(matches!(summary.provider, ProviderV1::Unknown { .. }));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn session_projection_rejects_invalid_identity_and_unbounded_label() {
        let project = Uuid::new_v4();
        let mut self_parent = row();
        self_parent.parent_id = Some(self_parent.id);
        assert!(session_summary(self_parent, project, attention()).is_err());

        let mut oversized = row();
        oversized.provider = "x".repeat(129);
        assert!(session_summary(oversized, project, attention()).is_err());

        let mut bad_time = row();
        bad_time.updated_at = "yesterday".into();
        assert!(session_summary(bad_time, project, attention()).is_err());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn history_projection_preserves_exact_and_limited_content() {
        let exact = history_event(history_row(), false).unwrap();
        assert_eq!(exact.pairing_state, PairingStateV1::Exact);
        assert_eq!(exact.content_state, ContentStateV1::Complete);
        assert!(
            serde_json::from_value::<HistoryEventV1>(serde_json::to_value(&exact).unwrap()).is_ok()
        );

        let mut preview = history_row();
        preview.content.observed_bytes = 9000;
        preview.content.truncated = true;
        preview.tool_pair_key = None;
        preview.tool_id_display.as_mut().unwrap().truncated = true;
        let projected = history_event(preview, false).unwrap();
        assert_eq!(projected.content_state, ContentStateV1::Preview);
        assert_eq!(projected.pairing_state, PairingStateV1::Oversized);
        assert!(
            serde_json::from_value::<HistoryEventV1>(serde_json::to_value(projected).unwrap())
                .is_ok()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn history_projection_keeps_offload_and_rejects_invalid_pairing() {
        let mut offloaded = history_row();
        offloaded.offloaded = true;
        let projected = history_event(offloaded, true).unwrap();
        assert_eq!(projected.content_state, ContentStateV1::Offloaded);
        assert_eq!(projected.pairing_state, PairingStateV1::Ambiguous);

        let mut invalid = history_row();
        invalid.tool_pair_key = None;
        assert!(history_event(invalid, true).is_err());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn detail_projection_labels_previews_and_saved_head() {
        let project = Uuid::new_v4();
        let summary = session_summary(row(), project, attention()).unwrap();
        let detail = session_detail(detail_source(), summary, vec![], vec![]).unwrap();
        assert_eq!(detail.query.state, PreviewStateV1::Truncated);
        assert_eq!(detail.model.state, PreviewStateV1::Unavailable);
        assert_eq!(detail.sequences.len(), 1);
        assert_eq!(detail.sequences[0].source, SourceV1::StoreHistory);
        assert_eq!(
            detail.sequences[0].event_id.as_ref().unwrap().get(),
            i64::MAX
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn detail_projection_rejects_title_mismatch_and_runtime_source_spoofing() {
        let project = Uuid::new_v4();
        let summary = session_summary(row(), project, attention()).unwrap();
        let mut source = detail_source();
        source.own_title.text = "Other session".into();
        assert!(session_detail(source, summary.clone(), vec![], vec![]).is_err());

        let source = detail_source();
        let spoofed = SequenceObservationV1 {
            source: SourceV1::StoreHistory,
            sequence: None,
            event_id: None,
        };
        assert!(session_detail(source, summary, vec![spoofed], vec![]).is_err());
    }
}
