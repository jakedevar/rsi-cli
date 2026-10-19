//! Schema-validated final handoffs (#2759, gap G08 of #2748).
//!
//! A topology node marked `structured_handoff` asks the harness to validate the
//! handoff's shape before RSI parses it: Claude Code's `--json-schema` (print
//! mode returns the validated `structured_output`) and Codex's
//! `--output-schema <FILE>` with `-o <FILE>` (the last message, constrained to
//! the schema). This module owns the one schema both CLIs are given and the
//! bridge back to the text grammar the parser already enforces:
//! [`render_structured_handoff`] turns a payload into the canonical
//! `PIPELINE HANDOFF — <STAGE>:` block, and [`normalize_structured_handoff`]
//! lets `parse_pipeline_handoff` accept a JSON-object last message as that
//! block. A free-text last message is returned untouched, so a node that did
//! not use the schema parses exactly as before.
//!
//! The schema is deliberately strict-mode compatible (every property listed in
//! `required`, optional values nullable, `additionalProperties: false`): the
//! Codex structured-output constraint rejects anything looser.

use serde_json::{Value, json};
use std::borrow::Cow;

/// Handoff fields in the order they are rendered, as `(key, kind)`. `kind` is
/// the JSON type of the property; every one is also nullable in the schema.
const FIELDS: &[(&str, FieldKind)] = &[
    ("status", FieldKind::Status),
    ("doc_path", FieldKind::Text),
    ("manifest_path", FieldKind::Text),
    ("summary", FieldKind::Text),
    ("commit", FieldKind::Text),
    ("scoped_test_job", FieldKind::Text),
    ("scoped_test_exit_code", FieldKind::Integer),
    ("tests_added", FieldKind::Text),
    ("evidence_commit", FieldKind::Text),
    ("blocker", FieldKind::Text),
    ("blocker_class", FieldKind::BlockerClass),
    ("blocker_evidence", FieldKind::Text),
    ("blocker_question", FieldKind::Text),
    ("capability_class", FieldKind::Text),
    ("friction", FieldKind::Text),
];

#[derive(Clone, Copy)]
enum FieldKind {
    Text,
    Integer,
    Status,
    BlockerClass,
}

/// The stage words a handoff marker may carry.
const STAGES: &[&str] = &[
    "RESEARCH",
    "PLAN",
    "IMPLEMENTATION",
    "REVIEW",
    "FIX",
    "VERIFY",
    "SMOKE",
    "DOCUMENTATION",
];

/// The closed `status` values of a strict V2 handoff.
const STATUSES: &[&str] = &[
    "complete",
    "partial",
    "blocked",
    "human_gate",
    "already_satisfied",
];

/// The closed `blocker_class` values.
const BLOCKER_CLASSES: &[&str] = &[
    "authority",
    "forbidden_scope",
    "destructive",
    "production",
    "resource",
    "technical_impasse",
];

/// The JSON Schema handed to the harness. Strict-mode compatible: all
/// properties required, optional ones nullable, no additional properties.
#[must_use]
pub fn structured_handoff_schema() -> Value {
    let mut properties = serde_json::Map::new();
    properties.insert(
        "stage".to_string(),
        json!({"type": "string", "enum": STAGES}),
    );
    let mut required = vec![Value::from("stage")];
    for (key, kind) in FIELDS {
        let schema = match kind {
            FieldKind::Text => json!({"type": ["string", "null"]}),
            FieldKind::Integer => json!({"type": ["integer", "null"]}),
            FieldKind::Status => json!({"type": "string", "enum": STATUSES}),
            FieldKind::BlockerClass => {
                let mut values: Vec<Value> = BLOCKER_CLASSES.iter().map(|c| json!(c)).collect();
                values.push(Value::Null);
                json!({"type": ["string", "null"], "enum": values})
            }
        };
        properties.insert((*key).to_string(), schema);
        required.push(Value::from(*key));
    }
    json!({
        "type": "object",
        "title": "RSI pipeline handoff",
        "description": "The worker's final handoff. Use null for a field that does not apply.",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

/// [`structured_handoff_schema`] as compact JSON text (the `--json-schema`
/// argument and the `--output-schema` file body).
#[must_use]
pub fn structured_handoff_schema_text() -> String {
    structured_handoff_schema().to_string()
}

/// Render a structured payload as the canonical handoff block, or `None` when
/// the value is not a handoff payload (not an object, no known `stage`, no
/// `status`). Null and empty fields are omitted; a multi-line value is folded
/// onto one line because the grammar is one `key: value` per line.
#[must_use]
pub fn render_structured_handoff(value: &Value) -> Option<String> {
    let object = value.as_object()?;
    let stage = object.get("stage")?.as_str()?.trim().to_ascii_uppercase();
    if !STAGES.contains(&stage.as_str()) {
        return None;
    }
    // A bare `{"stage": ...}` is not a handoff: the status is the one field the
    // strict parser cannot do without.
    object.get("status")?.as_str()?;
    let mut out = format!("PIPELINE HANDOFF — {stage}:\n");
    for (key, _) in FIELDS {
        let Some(field) = object.get(*key) else {
            continue;
        };
        let text = match field {
            Value::Null => continue,
            Value::String(text) => text.split_whitespace().collect::<Vec<_>>().join(" "),
            Value::Number(number) => number.to_string(),
            Value::Bool(flag) => flag.to_string(),
            _ => continue,
        };
        if text.is_empty() {
            continue;
        }
        out.push_str(key);
        out.push_str(": ");
        out.push_str(&text);
        out.push('\n');
    }
    Some(out)
}

/// The text the parser should read: a last message that is one JSON object in
/// the structured shape (optionally inside a single ```json fence, as some
/// models wrap it) becomes the rendered block; anything else is returned
/// unchanged.
#[must_use]
pub fn normalize_structured_handoff(reply: &str) -> Cow<'_, str> {
    let trimmed = reply.trim();
    let body = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|rest| rest.strip_suffix("```"))
        .map_or(trimmed, str::trim);
    if !body.starts_with('{') {
        return Cow::Borrowed(reply);
    }
    match serde_json::from_str::<Value>(body)
        .ok()
        .as_ref()
        .and_then(render_structured_handoff)
    {
        Some(rendered) => Cow::Owned(rendered),
        None => Cow::Borrowed(reply),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_contract::{PipelineStatusV2, parse_pipeline_handoff_v2};

    fn payload() -> Value {
        json!({
            "stage": "IMPLEMENTATION",
            "status": "complete",
            "doc_path": "inline",
            "manifest_path": "inline",
            "summary": "added the thing\nand the other thing",
            "commit": "0123456789abcdef0123456789abcdef01234567",
            "scoped_test_job": "job-1",
            "scoped_test_exit_code": 0,
            "tests_added": "t1, t2",
            "evidence_commit": null,
            "blocker": null,
            "blocker_class": null,
            "blocker_evidence": null,
            "blocker_question": null,
            "capability_class": "implementer",
            "friction": "none"
        })
    }

    #[test]
    fn schema_is_strict_mode_shaped_and_lists_every_rendered_field() {
        let schema = structured_handoff_schema();
        assert_eq!(schema["additionalProperties"], false);
        let properties = schema["properties"].as_object().unwrap();
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(required.len(), properties.len());
        for key in properties.keys() {
            assert!(required.contains(&key.as_str()), "{key} must be required");
        }
        for (key, _) in FIELDS {
            assert!(properties.contains_key(*key), "{key}");
        }
        assert!(properties.contains_key("stage"));
        // The text form is the same document.
        assert_eq!(
            serde_json::from_str::<Value>(&structured_handoff_schema_text()).unwrap(),
            schema
        );
    }

    #[test]
    fn a_payload_renders_as_the_canonical_block_and_parses() {
        let rendered = render_structured_handoff(&payload()).unwrap();
        assert!(rendered.starts_with("PIPELINE HANDOFF — IMPLEMENTATION:\nstatus: complete\n"));
        assert!(rendered.contains("summary: added the thing and the other thing\n"));
        assert!(rendered.contains("scoped_test_exit_code: 0\n"));
        assert!(!rendered.contains("blocker"), "null fields are omitted");
        let parsed = parse_pipeline_handoff_v2(&rendered, "").unwrap();
        assert_eq!(parsed.strict_status, PipelineStatusV2::Complete);
        assert_eq!(parsed.handoff.doc_path, "inline");
    }

    #[test]
    fn json_text_and_a_json_fence_parse_as_the_handoff() {
        // Codex `-o`: the last message is the JSON text.
        let text = payload().to_string();
        let parsed = parse_pipeline_handoff_v2(&text, "").unwrap();
        assert_eq!(parsed.strict_status, PipelineStatusV2::Complete);
        // A model that fences the object is read the same way.
        let fenced = format!("```json\n{text}\n```");
        assert!(parse_pipeline_handoff_v2(&fenced, "").is_ok());
    }

    #[test]
    fn a_blocked_payload_carries_its_blocker_group() {
        let mut blocked = payload();
        blocked["status"] = json!("blocked");
        blocked["blocker"] = json!("cannot reach the registry");
        blocked["blocker_class"] = json!("resource");
        blocked["blocker_evidence"] = json!("curl exit 7");
        blocked["blocker_question"] = json!("may I vendor the crate?");
        let parsed = parse_pipeline_handoff_v2(&blocked.to_string(), "").unwrap();
        assert_eq!(parsed.strict_status, PipelineStatusV2::Blocked);
        assert_eq!(
            parsed.blocker_question.as_deref(),
            Some("may I vendor the crate?")
        );
    }

    #[test]
    fn free_text_is_returned_untouched() {
        let free = "PIPELINE HANDOFF — IMPLEMENTATION:\nstatus: complete\ndoc_path: inline\nmanifest_path: inline\n";
        assert!(matches!(
            normalize_structured_handoff(free),
            Cow::Borrowed(text) if text == free
        ));
        for other in ["", "{not json", "{\"a\":1}", "[1]", "{\"stage\":\"PLAN\"}"] {
            assert!(matches!(
                normalize_structured_handoff(other),
                Cow::Borrowed(_)
            ));
        }
        // Prose before the marker is still refused, as it is today.
        assert!(parse_pipeline_handoff_v2("Done!\n".to_string().as_str(), "").is_err());
    }

    #[test]
    fn an_unknown_stage_or_missing_status_is_not_a_handoff() {
        assert!(
            render_structured_handoff(&json!({"stage": "NOPE", "status": "complete"})).is_none()
        );
        assert!(render_structured_handoff(&json!({"stage": "PLAN"})).is_none());
        assert!(render_structured_handoff(&json!("text")).is_none());
    }
}
