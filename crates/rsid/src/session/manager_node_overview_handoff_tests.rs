//! #2749: a seat handoff file edited after the appointment is flagged on the
//! successor's overview, so a request the retired seat appended after the
//! successor read the file is not lost.

use super::*;
use chrono::Duration;
use uuid::Uuid;

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
fn handoff(reference: Option<&str>, handed_at: DateTime<Utc>) -> ManagerNodeSeatHandoffV1 {
    ManagerNodeSeatHandoffV1 {
        appointment_id: Uuid::new_v4(),
        old_seat: Uuid::new_v4(),
        new_seat: Uuid::new_v4(),
        handoff: reference.map(str::to_string),
        handed_at,
        handoff_changed_after_appointment: false,
        handoff_modified_at: None,
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
fn set_modified(path: &std::path::Path, at: DateTime<Utc>) {
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(SystemTime::from(at))
        .unwrap();
}

/// The retired seat appends to the handoff file after the baton passed: the
/// successor's overview says the file changed since the appointment, and
/// when. A file written before the appointment is not flagged.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn a_handoff_edited_after_the_appointment_is_flagged_for_the_successor() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("handoff-gm.md");
    std::fs::write(&file, "# handoff\n").unwrap();
    let handed_at = Utc::now() - Duration::hours(1);
    let reference = file.to_str().unwrap();

    // Written before the baton passed: unchanged.
    set_modified(&file, handed_at - Duration::minutes(5));
    let shown = flag_handoff_changed(Some(handoff(Some(reference), handed_at)))
        .await
        .unwrap();
    assert!(!shown.handoff_changed_after_appointment);
    assert_eq!(shown.handoff_modified_at, None);

    // The old seat appends 11 s after the appointment.
    let edited = handed_at + Duration::seconds(11);
    set_modified(&file, edited);
    let shown = flag_handoff_changed(Some(handoff(Some(reference), handed_at)))
        .await
        .unwrap();
    assert!(shown.handoff_changed_after_appointment);
    assert_eq!(shown.handoff_modified_at, Some(edited));
    assert_eq!(shown.handoff.as_deref(), Some(reference));
    let wire = serde_json::to_value(&shown).unwrap();
    assert_eq!(wire["handoff_changed_after_appointment"], true);
}

/// A handoff that names a commit, a repository-relative path or a missing
/// file cannot be checked: never flagged, and no handoff stays none.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn an_unreadable_or_unnamed_handoff_is_not_flagged() {
    let handed_at = Utc::now();
    for reference in [
        None,
        Some("abc1234"),
        Some("thoughts/shared/handoffs/h.md"),
        Some("/nonexistent/rsi-2749/handoff.md"),
    ] {
        let shown = flag_handoff_changed(Some(handoff(reference, handed_at)))
            .await
            .unwrap();
        assert!(!shown.handoff_changed_after_appointment, "{reference:?}");
        assert_eq!(shown.handoff_modified_at, None);
    }
    assert!(flag_handoff_changed(None).await.is_none());
}

/// A handoff that predates the field parses with the flag off.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[test]
fn an_older_handoff_row_parses_with_the_flag_off() {
    let mut wire = serde_json::to_value(handoff(Some("h.md"), Utc::now())).unwrap();
    let object = wire.as_object_mut().unwrap();
    object.remove("handoff_changed_after_appointment");
    object.remove("handoff_modified_at");
    let parsed: ManagerNodeSeatHandoffV1 = serde_json::from_value(wire).unwrap();
    assert!(!parsed.handoff_changed_after_appointment);
    assert_eq!(parsed.handoff_modified_at, None);
}
