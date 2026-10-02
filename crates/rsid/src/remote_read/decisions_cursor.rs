use super::{
    NativeRuntimeApprovalListSnapshot, NativeRuntimeApprovalListState, PendingCandidate,
    PendingKey, PendingPagePosition, PendingPageSelection, PendingSource, QuestionSlotGeneration,
    QuestionSlotMirror, ReadError, RemoteCursorSigner, Result, RuntimeQuestionSlotListSnapshot,
    SourcePage,
};
use rsi_common::remote_read::{CursorV1, ReadRequestV1};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Continuation over four durable source keys and two bounded runtime lists.
/// Names are short because this is inside the 384-byte signed cursor payload.
/// An offset is usable only while both runtime identity witnesses still match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionsPositionV1 {
    #[serde(rename = "q")]
    questions_after: Option<Uuid>,
    #[serde(rename = "p")]
    native_publications_after: Option<Uuid>,
    #[serde(rename = "h")]
    native_historical_after: Option<Uuid>,
    #[serde(rename = "l")]
    legacy_after: Option<i64>,
    #[serde(rename = "r")]
    runtime_offset: u8,
    #[serde(rename = "s")]
    slot_offset: u8,
    #[serde(rename = "k")]
    key_bucket: u8,
    #[serde(rename = "b")]
    page_bucket: u8,
    #[serde(rename = "nw")]
    native_witness: String,
    #[serde(rename = "qw")]
    question_witness: String,
}

impl DecisionsPositionV1 {
    /// Convert examined indexes into source keysets before the input slices
    /// disappear. Unexamined sources retain their previous `after` key; an
    /// exhausted input slice does not assert that a source is exhausted.
    pub fn after_page(
        previous_after: [Option<PendingKey>; 4],
        selection: &PendingPageSelection,
        pages: [&SourcePage<PendingCandidate, PendingKey>; 4],
        native: &NativeRuntimeApprovalListSnapshot,
        slots: &RuntimeQuestionSlotListSnapshot,
        fallback_present: bool,
    ) -> Result<Self> {
        if selection.items.len() > 32
            || selection.next.key_bucket > 2
            || selection.next.page_bucket > 1
            || selection.next.examined[1] > 64
            || slots.slots.len() > 3
            || selection.next.slot_offset > slots.slots.len() + usize::from(fallback_present)
        {
            return Err(ReadError::InvalidSource);
        }
        let NativeRuntimeApprovalListState::Present(rows) = &native.state else {
            return Err(ReadError::SourceUnavailable);
        };
        if native.session != slots.session || selection.next.examined[1] > rows.len() {
            return Err(ReadError::InvalidSource);
        }
        let source_types = [
            PendingSource::Questions,
            PendingSource::NativePublications,
            PendingSource::NativeHistorical,
            PendingSource::LegacyApprovals,
        ];
        let indexes = [0, 2, 3, 4];
        let mut after = previous_after;
        for index in 0..4 {
            let page = pages[index];
            let examined = selection.next.examined[indexes[index]];
            let expected_next = page.has_more.then(|| {
                page.items.last().map(|last| {
                    if index == 3 {
                        PendingKey::LegacyRowid(last.rowid)
                    } else {
                        PendingKey::Id(last.id)
                    }
                })
            });
            if examined > page.items.len()
                || (page.has_more && page.items.is_empty())
                || expected_next.flatten() != page.next
                || page
                    .items
                    .iter()
                    .any(|row| row.source != source_types[index] || row.rowid <= 0)
                || page.items.windows(2).any(|pair| match source_types[index] {
                    PendingSource::LegacyApprovals => pair[0].rowid >= pair[1].rowid,
                    _ => pair[0].id >= pair[1].id,
                })
            {
                return Err(ReadError::InvalidSource);
            }
            if let Some(first) = page.items.first() {
                let advances = match (index, after[index]) {
                    (3, Some(PendingKey::LegacyRowid(previous))) => first.rowid > previous,
                    (3, None) => true,
                    (_, Some(PendingKey::Id(previous))) => first.id > previous,
                    (_, None) => true,
                    _ => false,
                };
                if !advances {
                    return Err(ReadError::InvalidSource);
                }
            }
            if examined > 0 {
                let last = page.items[examined - 1];
                after[index] = Some(if index == 3 {
                    PendingKey::LegacyRowid(last.rowid)
                } else {
                    PendingKey::Id(last.id)
                });
            }
        }
        let position = Self {
            questions_after: id_key(after[0])?,
            native_publications_after: id_key(after[1])?,
            native_historical_after: id_key(after[2])?,
            legacy_after: legacy_key(after[3])?,
            runtime_offset: selection.next.examined[1] as u8,
            slot_offset: selection.next.slot_offset as u8,
            key_bucket: selection.next.key_bucket,
            page_bucket: selection.next.page_bucket,
            native_witness: native_witness(native)?,
            question_witness: question_witness(slots)?,
        };
        position.validate()?;
        Ok(position)
    }

    /// Revalidate runtime identity before applying offsets to new captures.
    /// A change relocates the reader; it never promotes missing data to a
    /// tombstone or claims a stable snapshot across pages.
    pub fn resume(
        &self,
        native: &NativeRuntimeApprovalListSnapshot,
        slots: &RuntimeQuestionSlotListSnapshot,
        fallback_present: bool,
    ) -> Result<([Option<PendingKey>; 4], PendingPagePosition)> {
        self.validate().map_err(|_| ReadError::StaleCursor)?;
        if native_witness(native)? != self.native_witness
            || question_witness(slots)? != self.question_witness
        {
            return Err(ReadError::StaleCursor);
        }
        let NativeRuntimeApprovalListState::Present(rows) = &native.state else {
            return Err(ReadError::StaleCursor);
        };
        if native.session != slots.session
            || usize::from(self.runtime_offset) > rows.len()
            || usize::from(self.slot_offset) > slots.slots.len() + usize::from(fallback_present)
        {
            return Err(ReadError::StaleCursor);
        }
        Ok((
            [
                self.questions_after.map(PendingKey::Id),
                self.native_publications_after.map(PendingKey::Id),
                self.native_historical_after.map(PendingKey::Id),
                self.legacy_after.map(PendingKey::LegacyRowid),
            ],
            PendingPagePosition {
                examined: [0, usize::from(self.runtime_offset), 0, 0, 0],
                key_bucket: self.key_bucket,
                slot_offset: usize::from(self.slot_offset),
                page_bucket: self.page_bucket,
            },
        ))
    }

    fn validate(&self) -> Result<()> {
        if self.runtime_offset > 64
            || self.slot_offset > 4
            || self.key_bucket > 2
            || self.page_bucket > 1
            || self.legacy_after.is_some_and(|id| id <= 0)
            || !valid_digest(&self.native_witness)
            || !valid_digest(&self.question_witness)
        {
            return Err(ReadError::InvalidSource);
        }
        Ok(())
    }
}

impl RemoteCursorSigner {
    pub fn sign_decisions(
        &self,
        request: &ReadRequestV1,
        policy_scope: [u8; 32],
        position: &DecisionsPositionV1,
    ) -> Result<CursorV1> {
        if !matches!(request, ReadRequestV1::RemoteGetDecisionsV1(_)) {
            return Err(ReadError::InvalidSource);
        }
        position.validate()?;
        self.sign(request, policy_scope, position)
    }

    pub fn verify_decisions(
        &self,
        request: &ReadRequestV1,
        policy_scope: [u8; 32],
        cursor: &CursorV1,
    ) -> Result<DecisionsPositionV1> {
        if !matches!(request, ReadRequestV1::RemoteGetDecisionsV1(_)) {
            return Err(ReadError::StaleCursor);
        }
        let position: DecisionsPositionV1 = self.verify(request, policy_scope, cursor)?;
        position.validate().map_err(|_| ReadError::StaleCursor)?;
        Ok(position)
    }
}

fn id_key(key: Option<PendingKey>) -> Result<Option<Uuid>> {
    match key {
        Some(PendingKey::Id(id)) => Ok(Some(id)),
        None => Ok(None),
        _ => Err(ReadError::InvalidSource),
    }
}

fn legacy_key(key: Option<PendingKey>) -> Result<Option<i64>> {
    match key {
        Some(PendingKey::LegacyRowid(id)) if id > 0 => Ok(Some(id)),
        None => Ok(None),
        _ => Err(ReadError::InvalidSource),
    }
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn native_witness(snapshot: &NativeRuntimeApprovalListSnapshot) -> Result<String> {
    let NativeRuntimeApprovalListState::Present(rows) = &snapshot.state else {
        return Err(ReadError::SourceUnavailable);
    };
    if rows.len() > 64
        || rows.windows(2).any(|pair| pair[0].id >= pair[1].id)
        || rows
            .iter()
            .any(|row| !row.writer_live || row.writer_capacity > 64)
        || rows.first().is_some_and(|first| {
            rows.iter().any(|row| {
                row.incarnation_id != first.incarnation_id
                    || row.spawn_generation != first.spawn_generation
                    || row.writer_capacity != first.writer_capacity
            })
        })
    {
        return Err(ReadError::InvalidSource);
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"rsi-remote-native-page-witness-v1");
    hasher.update(snapshot.session.as_bytes());
    for row in rows {
        hasher.update(row.id.as_bytes());
        hasher.update(row.incarnation_id.as_bytes());
        hasher.update(&row.spawn_generation.to_be_bytes());
        hasher.update(&row.writer_capacity.to_be_bytes());
        hasher.update(&[
            u8::from(row.resolution_observed),
            u8::from(row.resolution_persisted),
        ]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn question_witness(snapshot: &RuntimeQuestionSlotListSnapshot) -> Result<String> {
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
            snapshot.slots[index + 1..]
                .iter()
                .any(|other| slot.generation == other.generation && slot.mirror == other.mirror)
        })
    {
        return Err(ReadError::InvalidSource);
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"rsi-remote-question-page-witness-v1");
    hasher.update(snapshot.project.as_bytes());
    hasher.update(snapshot.session.as_bytes());
    hasher.update(&[u8::from(snapshot.completed_found)]);
    match snapshot.active_generation {
        Some(generation) => {
            hasher.update(&[1]);
            hasher.update(&generation.to_be_bytes());
        }
        None => {
            hasher.update(&[0]);
        }
    }
    for slot in &snapshot.slots {
        match slot.generation {
            QuestionSlotGeneration::Spawn(generation) => {
                hasher.update(&[1]);
                hasher.update(&generation.to_be_bytes());
            }
            QuestionSlotGeneration::Completed => {
                hasher.update(&[2]);
            }
        }
        hasher.update(&[match slot.mirror {
            QuestionSlotMirror::Tracked => 1,
            QuestionSlotMirror::Session => 2,
        }]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

#[cfg(all(
    test,
    any(not(feature = "test-shard-mode"), feature = "test-shard-store-04")
))]
mod tests {
    use super::*;
    use crate::remote_read::{
        PendingPageCandidate, PendingUnionCandidate, capture_saved_pending_runtime_sources,
    };
    use chrono::Utc;
    use serde_json::json;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn decision_position_advances_examined_keys_and_rejects_runtime_change() {
        let project = Uuid::new_v4();
        let session = Uuid::new_v4();
        let ids = [
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        ];
        let sources = [
            PendingSource::Questions,
            PendingSource::NativePublications,
            PendingSource::NativeHistorical,
            PendingSource::LegacyApprovals,
        ];
        let pages: Vec<_> = sources
            .into_iter()
            .enumerate()
            .map(|(index, source)| SourcePage {
                items: vec![PendingCandidate {
                    source,
                    id: ids[index],
                    rowid: (index + 1) as i64,
                }],
                next: None,
                has_more: false,
            })
            .collect();
        let native = NativeRuntimeApprovalListSnapshot {
            session,
            observed_at: Utc::now(),
            state: NativeRuntimeApprovalListState::Present(vec![]),
        };
        let slots = RuntimeQuestionSlotListSnapshot {
            project,
            session,
            active_observed_at: Utc::now(),
            completed_observed_at: Utc::now(),
            active_generation: None,
            completed_found: false,
            slots: vec![],
        };
        let selection = PendingPageSelection {
            items: vec![
                PendingPageCandidate::Key(PendingUnionCandidate::Question(pages[0].items[0])),
                PendingPageCandidate::Key(PendingUnionCandidate::Native {
                    id: ids[1],
                    runtime: false,
                    durable: Some(pages[1].items[0]),
                }),
                PendingPageCandidate::Key(PendingUnionCandidate::Native {
                    id: ids[2],
                    runtime: false,
                    durable: Some(pages[2].items[0]),
                }),
                PendingPageCandidate::Key(PendingUnionCandidate::Legacy(pages[3].items[0])),
            ],
            next: PendingPagePosition {
                examined: [1, 0, 1, 1, 1],
                key_bucket: 2,
                slot_offset: 0,
                page_bucket: 1,
            },
            remaining_in_inputs: false,
        };
        let position = DecisionsPositionV1::after_page(
            [None; 4],
            &selection,
            [&pages[0], &pages[1], &pages[2], &pages[3]],
            &native,
            &slots,
            false,
        )
        .unwrap();
        assert!(serde_json::to_vec(&position).unwrap().len() <= 384);
        let request: ReadRequestV1 = serde_json::from_value(json!({
            "method":"RemoteGetDecisionsV1",
            "params":{"project_id":project,"session_id":session,"limit":16,"mode":"attention"}
        }))
        .unwrap();
        let signer = RemoteCursorSigner::new(Uuid::new_v4());
        let cursor = signer.sign_decisions(&request, [9; 32], &position).unwrap();
        let decoded = signer.verify_decisions(&request, [9; 32], &cursor).unwrap();
        let (after, start) = decoded.resume(&native, &slots, false).unwrap();
        let capture = capture_saved_pending_runtime_sources(
            project,
            session,
            |_, _| {
                Ok(RuntimeQuestionSlotListSnapshot {
                    project: slots.project,
                    session: slots.session,
                    active_observed_at: slots.active_observed_at,
                    completed_observed_at: slots.completed_observed_at,
                    active_generation: slots.active_generation,
                    completed_found: slots.completed_found,
                    slots: slots.slots.clone(),
                })
            },
            |_| Ok(native.clone()),
        )
        .unwrap();
        let (captured_after, captured_start) = capture.resume_position(&decoded, false).unwrap();
        assert_eq!(captured_after, after);
        assert_eq!(captured_start.examined, start.examined);
        assert_eq!(after[0], Some(PendingKey::Id(ids[0])));
        assert_eq!(after[1], Some(PendingKey::Id(ids[1])));
        assert_eq!(after[2], Some(PendingKey::Id(ids[2])));
        assert_eq!(after[3], Some(PendingKey::LegacyRowid(4)));
        assert_eq!(start.examined, [0, 0, 0, 0, 0]);
        assert_eq!(start.key_bucket, 2);
        let changed_slots = RuntimeQuestionSlotListSnapshot {
            project,
            session,
            active_observed_at: Utc::now(),
            completed_observed_at: Utc::now(),
            active_generation: Some(1),
            completed_found: false,
            slots: vec![],
        };
        assert!(matches!(
            decoded.resume(&native, &changed_slots, false),
            Err(ReadError::StaleCursor)
        ));
        let changed_native = NativeRuntimeApprovalListSnapshot {
            session: Uuid::new_v4(),
            observed_at: Utc::now(),
            state: NativeRuntimeApprovalListState::Present(vec![]),
        };
        assert!(matches!(
            decoded.resume(&changed_native, &slots, false),
            Err(ReadError::StaleCursor)
        ));
        let bad_page = SourcePage {
            items: pages[0].items.clone(),
            next: Some(PendingKey::Id(ids[0])),
            has_more: false,
        };
        assert!(matches!(
            DecisionsPositionV1::after_page(
                [None; 4],
                &selection,
                [&bad_page, &pages[1], &pages[2], &pages[3]],
                &native,
                &slots,
                false,
            ),
            Err(ReadError::InvalidSource)
        ));
        assert!(matches!(
            DecisionsPositionV1::after_page(
                [Some(PendingKey::Id(ids[0])), None, None, None],
                &selection,
                [&pages[0], &pages[1], &pages[2], &pages[3]],
                &native,
                &slots,
                false,
            ),
            Err(ReadError::InvalidSource)
        ));
    }
}
