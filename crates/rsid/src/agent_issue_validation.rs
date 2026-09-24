//! Bounded malformed-request classification for guarded Issue verbs.
//!
//! This module deliberately exposes only typed, allowlisted evidence.  It
//! never returns serde text, JSON paths, values, or caller-supplied keys.

#[cfg(test)]
use rsi_common::agent_control_schema::AgentControlVerbV1;
use rsi_common::rpc::{
    AgentArchiveIssueRequestV1, AgentGetIssueRequestV1, AgentIssueValidationClassV1,
    AgentIssueValidationFieldV1, AgentIssueValidationV1, AgentListIssuesRequestV1,
    AgentRestoreIssueRequestV1, AgentUpdateIssueRequestV1, AgentUpdateIssueStatusRequestV1,
};
use rsi_common::types::IssueEventPageRequestV1;
use serde::de::DeserializeOwned;
use serde_json::Value;

#[derive(Debug, Clone, Copy)]
pub(crate) enum AgentIssueRequestKind {
    List,
    Get,
    Update,
    UpdateStatus,
    Archive,
    Restore,
    ListEvents,
}

impl AgentIssueRequestKind {
    #[cfg(test)]
    const fn verb(self) -> AgentControlVerbV1 {
        match self {
            Self::List => AgentControlVerbV1::ListIssues,
            Self::Get => AgentControlVerbV1::GetIssue,
            Self::Update => AgentControlVerbV1::UpdateIssue,
            Self::UpdateStatus => AgentControlVerbV1::UpdateIssueStatus,
            Self::Archive => AgentControlVerbV1::ArchiveIssue,
            Self::Restore => AgentControlVerbV1::RestoreIssue,
            Self::ListEvents => AgentControlVerbV1::ListIssueEvents,
        }
    }

    const fn allowed(self) -> &'static [&'static str] {
        match self {
            Self::List => &["status", "archive", "cursor", "limit", "ready"],
            Self::Get => &["issue_id"],
            Self::Update => &[
                "issue_id",
                "expected_row_version",
                "idempotency_key",
                "title",
                "body",
                "labels",
                "priority",
                "clear_priority",
                "assignee",
                "clear_assignee",
            ],
            Self::UpdateStatus => &[
                "issue_id",
                "status",
                "expected_row_version",
                "idempotency_key",
            ],
            Self::Archive | Self::Restore => {
                &["issue_id", "expected_row_version", "idempotency_key"]
            }
            Self::ListEvents => &["issue_id", "after_sequence", "limit"],
        }
    }

    const fn required(self) -> &'static [&'static str] {
        match self {
            Self::List => &[],
            Self::Get | Self::ListEvents => &["issue_id"],
            Self::Update | Self::Archive | Self::Restore => {
                &["issue_id", "expected_row_version", "idempotency_key"]
            }
            Self::UpdateStatus => &[
                "issue_id",
                "status",
                "expected_row_version",
                "idempotency_key",
            ],
        }
    }
}

fn field(name: &str) -> Option<AgentIssueValidationFieldV1> {
    match name {
        "status" => Some(AgentIssueValidationFieldV1::Status),
        "archive" => Some(AgentIssueValidationFieldV1::Archive),
        "cursor" => Some(AgentIssueValidationFieldV1::Cursor),
        "limit" => Some(AgentIssueValidationFieldV1::Limit),
        "ready" => Some(AgentIssueValidationFieldV1::Ready),
        "issue_id" => Some(AgentIssueValidationFieldV1::IssueId),
        "expected_row_version" => Some(AgentIssueValidationFieldV1::ExpectedRowVersion),
        "idempotency_key" => Some(AgentIssueValidationFieldV1::IdempotencyKey),
        "title" => Some(AgentIssueValidationFieldV1::Title),
        "body" => Some(AgentIssueValidationFieldV1::Body),
        "labels" => Some(AgentIssueValidationFieldV1::Labels),
        "priority" => Some(AgentIssueValidationFieldV1::Priority),
        "clear_priority" => Some(AgentIssueValidationFieldV1::ClearPriority),
        "assignee" => Some(AgentIssueValidationFieldV1::Assignee),
        "clear_assignee" => Some(AgentIssueValidationFieldV1::ClearAssignee),
        "after_sequence" => Some(AgentIssueValidationFieldV1::AfterSequence),
        _ => None,
    }
}

fn hint(
    class: AgentIssueValidationClassV1,
    field: Option<AgentIssueValidationFieldV1>,
) -> AgentIssueValidationV1 {
    AgentIssueValidationV1 { class, field }
}

/// Decode one existing strict DTO after bounded object/field classification.
/// `List` is the only guarded verb that accepts null as its default request.
pub(crate) fn decode<T: DeserializeOwned>(
    kind: AgentIssueRequestKind,
    value: &Value,
) -> Result<T, AgentIssueValidationV1> {
    if value.is_null() && matches!(kind, AgentIssueRequestKind::List) {
        return serde_json::from_value(Value::Object(Default::default()))
            .map_err(|_| hint(AgentIssueValidationClassV1::InvalidShape, None));
    }
    let Some(object) = value.as_object() else {
        return Err(hint(AgentIssueValidationClassV1::InvalidShape, None));
    };
    if object
        .keys()
        .any(|key| !kind.allowed().contains(&key.as_str()))
    {
        return Err(hint(AgentIssueValidationClassV1::UnknownField, None));
    }
    if let Some(missing) = kind
        .required()
        .iter()
        .find(|name| !object.contains_key(**name))
    {
        return Err(hint(
            AgentIssueValidationClassV1::MissingField,
            field(missing),
        ));
    }

    let encoded = serde_json::to_vec(value)
        .map_err(|_| hint(AgentIssueValidationClassV1::InvalidShape, None))?;
    let mut deserializer = serde_json::Deserializer::from_slice(&encoded);
    match serde_path_to_error::deserialize::<_, T>(&mut deserializer) {
        Ok(decoded) => Ok(decoded),
        Err(error) => {
            let public_field = error
                .path()
                .iter()
                .next()
                .and_then(|segment| match segment {
                    serde_path_to_error::Segment::Map { key } => field(key),
                    serde_path_to_error::Segment::Seq { .. }
                    | serde_path_to_error::Segment::Enum { .. }
                    | serde_path_to_error::Segment::Unknown => None,
                });
            Err(hint(
                AgentIssueValidationClassV1::InvalidField,
                public_field,
            ))
        }
    }
}

pub(crate) fn decode_list(
    value: &Value,
) -> Result<AgentListIssuesRequestV1, AgentIssueValidationV1> {
    decode(AgentIssueRequestKind::List, value)
}
pub(crate) fn decode_get(value: &Value) -> Result<AgentGetIssueRequestV1, AgentIssueValidationV1> {
    decode(AgentIssueRequestKind::Get, value)
}
pub(crate) fn decode_update(
    value: &Value,
) -> Result<AgentUpdateIssueRequestV1, AgentIssueValidationV1> {
    decode(AgentIssueRequestKind::Update, value)
}
pub(crate) fn decode_update_status(
    value: &Value,
) -> Result<AgentUpdateIssueStatusRequestV1, AgentIssueValidationV1> {
    decode(AgentIssueRequestKind::UpdateStatus, value)
}
pub(crate) fn decode_archive(
    value: &Value,
) -> Result<AgentArchiveIssueRequestV1, AgentIssueValidationV1> {
    decode(AgentIssueRequestKind::Archive, value)
}
pub(crate) fn decode_restore(
    value: &Value,
) -> Result<AgentRestoreIssueRequestV1, AgentIssueValidationV1> {
    decode(AgentIssueRequestKind::Restore, value)
}
pub(crate) fn decode_list_events(
    value: &Value,
) -> Result<IssueEventPageRequestV1, AgentIssueValidationV1> {
    decode(AgentIssueRequestKind::ListEvents, value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::rpc::{
        AgentIssueValidationClassV1 as Class, AgentIssueValidationFieldV1 as Field,
    };

    #[test]
    fn list_preserves_null_default_and_other_roots_fail_closed() {
        assert!(decode_list(&Value::Null).is_ok());
        let error = decode_get(&Value::Null).unwrap_err();
        assert_eq!(error.class, Class::InvalidShape);
        assert_eq!(error.field, None);
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn agent_issue_validation_ready_field_decodes_and_invalid_values_receive_typed_hints() {
        let decoded = decode_list(&serde_json::json!({"ready": true})).unwrap();
        assert!(decoded.ready);

        let error = decode_list(&serde_json::json!({"ready": "yes"})).unwrap_err();
        assert_eq!(error.class, Class::InvalidField);
        assert_eq!(error.field, Some(Field::Ready));
    }

    #[test]
    fn classifier_never_echoes_unknown_keys_and_uses_stable_precedence() {
        let error =
            decode_get(&serde_json::json!({"token/secret": "never", "issue_id": 7})).unwrap_err();
        assert_eq!(error.class, Class::UnknownField);
        assert_eq!(error.field, None);
        let encoded = serde_json::to_string(&error).unwrap();
        assert!(!encoded.contains("token/secret"));
        assert!(!encoded.contains("never"));
        let error = decode_update(&serde_json::json!({"title": "x"})).unwrap_err();
        assert_eq!(error.class, Class::MissingField);
        assert_eq!(error.field, Some(Field::IssueId));
    }

    #[test]
    fn all_seven_request_kinds_accept_current_shapes_and_reject_invalid_roots() {
        type Decode = fn(&Value) -> Result<(), AgentIssueValidationV1>;
        let issue_id = uuid::Uuid::new_v4();
        let cases: [(&str, Value, Decode); 7] = [
            ("list", serde_json::json!({}), |value| {
                decode_list(value).map(drop)
            }),
            ("get", serde_json::json!({"issue_id": issue_id}), |value| {
                decode_get(value).map(drop)
            }),
            (
                "update",
                serde_json::json!({
                    "issue_id": issue_id,
                    "expected_row_version": 1,
                    "idempotency_key": "update-1"
                }),
                |value| decode_update(value).map(drop),
            ),
            (
                "update_status",
                serde_json::json!({
                    "issue_id": issue_id,
                    "status": "Open",
                    "expected_row_version": 1,
                    "idempotency_key": "status-1"
                }),
                |value| decode_update_status(value).map(drop),
            ),
            (
                "archive",
                serde_json::json!({
                    "issue_id": issue_id,
                    "expected_row_version": 1,
                    "idempotency_key": "archive-1"
                }),
                |value| decode_archive(value).map(drop),
            ),
            (
                "restore",
                serde_json::json!({
                    "issue_id": issue_id,
                    "expected_row_version": 1,
                    "idempotency_key": "restore-1"
                }),
                |value| decode_restore(value).map(drop),
            ),
            (
                "list_events",
                serde_json::json!({"issue_id": issue_id}),
                |value| decode_list_events(value).map(drop),
            ),
        ];

        for (name, valid, decode) in cases {
            assert!(decode(&valid).is_ok(), "valid {name} request");
            let error = decode(&serde_json::json!([])).unwrap_err();
            assert_eq!(error.class, Class::InvalidShape, "request: {name}");
            assert_eq!(error.field, None, "request: {name}");
        }
    }

    #[test]
    fn all_required_and_known_invalid_fields_map_to_closed_public_names() {
        let issue_id = uuid::Uuid::new_v4();
        for (error, expected_field) in [
            (
                decode_get(&serde_json::json!({})).unwrap_err(),
                Field::IssueId,
            ),
            (
                decode_update(&serde_json::json!({"issue_id": issue_id})).unwrap_err(),
                Field::ExpectedRowVersion,
            ),
            (
                decode_update_status(&serde_json::json!({"issue_id": issue_id})).unwrap_err(),
                Field::Status,
            ),
            (
                decode_archive(&serde_json::json!({"issue_id": issue_id})).unwrap_err(),
                Field::ExpectedRowVersion,
            ),
            (
                decode_restore(&serde_json::json!({
                    "issue_id": issue_id,
                    "expected_row_version": 1
                }))
                .unwrap_err(),
                Field::IdempotencyKey,
            ),
            (
                decode_list_events(&serde_json::json!({})).unwrap_err(),
                Field::IssueId,
            ),
        ] {
            assert_eq!(error.class, Class::MissingField);
            assert_eq!(error.field, Some(expected_field));
        }

        for (error, expected_field) in [
            (
                decode_list(&serde_json::json!({"status": 7})).unwrap_err(),
                Field::Status,
            ),
            (
                decode_get(&serde_json::json!({"issue_id": 7})).unwrap_err(),
                Field::IssueId,
            ),
            (
                decode_update(&serde_json::json!({
                    "issue_id": issue_id,
                    "expected_row_version": "one",
                    "idempotency_key": "update-2"
                }))
                .unwrap_err(),
                Field::ExpectedRowVersion,
            ),
            (
                decode_update_status(&serde_json::json!({
                    "issue_id": issue_id,
                    "status": 7,
                    "expected_row_version": 1,
                    "idempotency_key": "status-2"
                }))
                .unwrap_err(),
                Field::Status,
            ),
            (
                decode_archive(&serde_json::json!({
                    "issue_id": issue_id,
                    "expected_row_version": 1,
                    "idempotency_key": 7
                }))
                .unwrap_err(),
                Field::IdempotencyKey,
            ),
            (
                decode_restore(&serde_json::json!({
                    "issue_id": issue_id,
                    "expected_row_version": "one",
                    "idempotency_key": "restore-2"
                }))
                .unwrap_err(),
                Field::ExpectedRowVersion,
            ),
            (
                decode_list_events(&serde_json::json!({
                    "issue_id": issue_id,
                    "after_sequence": "zero"
                }))
                .unwrap_err(),
                Field::AfterSequence,
            ),
        ] {
            assert_eq!(error.class, Class::InvalidField);
            assert_eq!(error.field, Some(expected_field));
        }
    }

    #[test]
    fn nested_decode_path_collapses_to_allowlisted_top_level_field() {
        let error =
            decode_list(&serde_json::json!({"cursor": {"issue_id": 7, "display_number": 1}}))
                .unwrap_err();
        assert_eq!(error.class, Class::InvalidField);
        assert_eq!(error.field, Some(Field::Cursor));
    }

    #[test]
    fn classifier_metadata_matches_the_common_catalog_schemas() {
        for kind in [
            AgentIssueRequestKind::List,
            AgentIssueRequestKind::Get,
            AgentIssueRequestKind::Update,
            AgentIssueRequestKind::UpdateStatus,
            AgentIssueRequestKind::Archive,
            AgentIssueRequestKind::Restore,
            AgentIssueRequestKind::ListEvents,
        ] {
            let schema = kind.verb().descriptor().parameters();
            let mut properties = schema["properties"]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>();
            let mut allowed = kind.allowed().to_vec();
            properties.sort_unstable();
            allowed.sort_unstable();
            assert_eq!(properties, allowed);

            let required = schema["required"]
                .as_array()
                .map(|fields| {
                    fields
                        .iter()
                        .map(|field| field.as_str().unwrap())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            assert_eq!(required, kind.required());
        }
    }
}
