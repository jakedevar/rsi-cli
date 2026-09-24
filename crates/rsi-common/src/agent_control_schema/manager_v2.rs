//! Composable closed schemas for the shared manager v2 request contract.
use serde_json::{Value, json};
use std::sync::LazyLock;

pub(super) static INSPECT: LazyLock<String> = LazyLock::new(|| inspect().to_string());
pub(super) static UPDATE: LazyLock<String> = LazyLock::new(|| update().to_string());
pub(super) static SUBMIT_REVIEW: LazyLock<String> = LazyLock::new(|| submit_review().to_string());
pub(super) static CONTROL: LazyLock<String> = LazyLock::new(|| control().to_string());
pub(super) static PREPARE_CONTROL: LazyLock<String> =
    LazyLock::new(|| prepare_control().to_string());
pub(super) static COMMIT_PREPARED_CONTROL: LazyLock<String> =
    LazyLock::new(|| commit_prepared_control().to_string());
pub(super) static GET_ACTION: LazyLock<String> = LazyLock::new(|| get_action().to_string());

fn object(fields: &[(&str, Value)], required: &[&str]) -> Value {
    json!({"type":"object", "additionalProperties":false,
        "properties":fields.iter().map(|(k,v)| (k.to_string(),v.clone())).collect::<serde_json::Map<_,_>>(),
        "required":required})
}
fn string() -> Value {
    json!({"type":"string"})
}
fn uuid() -> Value {
    json!({"type":"string","format":"uuid"})
}
fn integer(min: i64, max: i64) -> Value {
    json!({"type":"integer","minimum":min,"maximum":max})
}
fn boolean() -> Value {
    json!({"type":"boolean"})
}
fn enumeration(values: &[&str]) -> Value {
    json!({"type":"string","enum":values})
}
fn optional(value: Value) -> Value {
    json!({"anyOf":[value,{"type":"null"}],"default":null})
}
fn array(value: Value) -> Value {
    json!({"type":"array","items":value})
}
fn fence() -> Value {
    object(
        &[
            ("scope_version", integer(1, i64::MAX)),
            ("policy_version", integer(1, i64::MAX)),
        ],
        &["scope_version", "policy_version"],
    )
}
fn lead_fence() -> Value {
    object(
        &[
            ("lead_session_id", optional(uuid())),
            ("lead_generation", integer(0, i64::MAX)),
            ("event_sequence", integer(0, i64::MAX)),
            ("custody_generation", optional(integer(1, i64::MAX))),
        ],
        &["lead_generation", "event_sequence"],
    )
}
fn succession_fence() -> Value {
    object(
        &[
            ("authority_epoch", integer(1, i64::MAX)),
            ("custody_generation", optional(integer(1, i64::MAX))),
        ],
        &["authority_epoch"],
    )
}
fn committed_handoff() -> Value {
    let oid = json!({"type":"string","pattern":"^([0-9a-f]{40}|[0-9a-f]{64})$"});
    object(
        &[
            ("source_commit", oid.clone()),
            (
                "relative_path",
                json!({"type":"string","minLength":1,"maxLength":1024}),
            ),
            ("blob_oid", oid),
        ],
        &["source_commit", "relative_path", "blob_oid"],
    )
}
fn launch() -> Value {
    object(
        &[
            (
                "provider",
                enumeration(&[
                    "Claude",
                    "Codex",
                    "Pioneer",
                    "OpenRouter",
                    "Bedrock",
                    "Local",
                    "Antigravity",
                    "CodexAppServer",
                    "Harness",
                    "Gemini",
                ]),
            ),
            (
                "model",
                json!({"type":"string","minLength":1,"maxLength":256}),
            ),
            (
                "effort",
                optional(json!({"type":"string","minLength":1,"maxLength":32})),
            ),
        ],
        &["provider", "model"],
    )
}
fn stage() -> Value {
    enumeration(&[
        "planning",
        "implementation",
        "review",
        "verification",
        "integration",
    ])
}
fn evidence() -> Value {
    object(
        &[
            ("source_session_id", uuid()),
            ("source_commit", string()),
            ("artifact_path", string()),
            ("artifact_commit", string()),
            ("closure_evidence_id", optional(uuid())),
        ],
        &[
            "source_session_id",
            "source_commit",
            "artifact_path",
            "artifact_commit",
        ],
    )
}
fn tagged(tag: &str, name: &str, fields: &[(&str, Value)], required: &[&str]) -> Value {
    let mut fields = fields.to_vec();
    fields.insert(0, (tag, json!({"type":"string","enum":[name]})));
    let mut required = required.to_vec();
    required.insert(0, tag);
    object(&fields, &required)
}
fn request(field: &str, variants: Vec<Value>) -> Value {
    object(
        &[
            ("fence", fence()),
            (
                "idempotency_key",
                json!({"type":"string","minLength":1,"maxLength":128}),
            ),
            (field, json!({"oneOf":variants})),
        ],
        &["fence", "idempotency_key", field],
    )
}
fn inspect() -> Value {
    let mut section = enumeration(&[
        "overview",
        "workers",
        "work",
        "requests",
        "decisions",
        "topology",
        "resources",
        "actions",
        "events",
        "archive",
        "health",
    ]);
    section["default"] = json!("overview");
    let mut limit = integer(1, 64);
    limit["default"] = json!(32);
    object(
        &[
            ("section", section),
            ("epic_id", optional(uuid())),
            ("cursor", optional(json!({"type":"string","maxLength":512}))),
            ("limit", limit),
        ],
        &[],
    )
}
fn control() -> Value {
    let mut variants = vec![tagged(
        "action",
        "succeed_manager",
        &[
            ("expected", succession_fence()),
            ("launch", launch()),
            ("handoff", committed_handoff()),
        ],
        &["expected", "launch", "handoff"],
    )];
    for action in ["resume_lead", "pause_lead", "retry_lead", "replace_lead"] {
        let text_field = match action {
            "pause_lead" => "reason",
            "replace_lead" => "query",
            _ => "message",
        };
        let mut fields = vec![
            ("epic_id", uuid()),
            ("expected", lead_fence()),
            (text_field, string()),
        ];
        let mut required = vec!["epic_id", "expected", text_field];
        if action == "retry_lead" {
            fields.push(("launch", optional(launch())));
        }
        if action == "replace_lead" {
            fields.push(("launch", launch()));
            required.push("launch");
        }
        variants.push(tagged("action", action, &fields, &required));
    }
    variants.push(tagged(
        "action",
        "create_container",
        &[
            ("parent_id", optional(uuid())),
            ("kind", enumeration(&["Group", "Epic"])),
            ("name", string()),
            ("tags", array(string())),
        ],
        &["kind", "name", "tags"],
    ));
    for action in [
        "update_container",
        "archive_container",
        "delete_container",
        "restore_container",
    ] {
        let mut fields = vec![
            ("container_id", uuid()),
            (
                "expected_updated_at",
                json!({"type":"string","format":"date-time"}),
            ),
        ];
        let mut required = vec!["container_id", "expected_updated_at"];
        if action == "update_container" {
            fields.extend([("name", string()), ("description", optional(string()))]);
            required.push("name");
        }
        variants.push(tagged("action", action, &fields, &required));
    }
    variants.push(tagged(
        "action",
        "create_session",
        &[
            ("parent_id", uuid()),
            (
                "kind",
                enumeration(&[
                    "Standard", "Story", "Task", "Bug", "Feature", "Refactor", "Research",
                ]),
            ),
            ("query", string()),
            ("launch", launch()),
        ],
        &["parent_id", "kind", "query", "launch"],
    ));
    variants.push(tagged(
        "action",
        "assign_lead",
        &[
            ("epic_id", uuid()),
            ("expected", lead_fence()),
            ("session_id", optional(uuid())),
        ],
        &["epic_id", "expected"],
    ));
    variants.push(tagged(
        "action",
        "settle_uncertain_action",
        &[
            ("operation_id", uuid()),
            ("expected_row_version", integer(1, i64::MAX)),
        ],
        &["operation_id", "expected_row_version"],
    ));
    variants.push(tagged(
        "action",
        "retire_lead_continuations",
        &[("epic_id", uuid()), ("expected", lead_fence())],
        &["epic_id", "expected"],
    ));
    for action in ["archive_session", "restore_session", "update_session"] {
        let mut fields = vec![
            ("session_id", uuid()),
            (
                "expected_updated_at",
                json!({"type":"string","format":"date-time"}),
            ),
        ];
        let mut required = vec!["session_id", "expected_updated_at"];
        if action == "update_session" {
            fields.push(("patch", session_patch()));
            required.push("patch");
        }
        variants.push(tagged("action", action, &fields, &required));
    }
    variants.push(tagged(
        "action",
        "operator_call",
        &[
            ("call", operator_call()),
            (
                "expected",
                optional(object(
                    &[(
                        "session_updated_at",
                        json!({"type":"string","format":"date-time"}),
                    )],
                    &["session_updated_at"],
                )),
            ),
        ],
        &["call"],
    ));
    request("operation", variants)
}

/// K14 (#672): one method of the closed delegated operator allowlist with its
/// closed params. The daemon refuses every other method with a typed code.
fn operator_call() -> Value {
    let session = || object(&[("session_id", uuid())], &["session_id"]);
    let call = |method: &str, params: Value, required: &[&str]| {
        object(
            &[("method", enumeration(&[method])), ("params", params)],
            required,
        )
    };
    let statuses = enumeration(&[
        "Starting",
        "Running",
        "WaitingApproval",
        "Completed",
        "Failed",
        "Interrupted",
        "Archived",
        "Deleted",
    ]);
    let list = object(
        &[
            (
                "status_in",
                json!({"type":"array","maxItems":8,"items":statuses}),
            ),
            (
                "terminal_before",
                optional(json!({"type":"string","format":"date-time"})),
            ),
            (
                "after",
                optional(object(
                    &[
                        (
                            "updated_at",
                            json!({"type":"string","minLength":1,"maxLength":40}),
                        ),
                        ("id", uuid()),
                    ],
                    &["updated_at", "id"],
                )),
            ),
            ("limit", optional(integer(1, 64))),
        ],
        &[],
    );
    json!({"oneOf":[
        call("ArchiveSession", session(), &["method", "params"]),
        call("GetArchiveCleanupStatus", session(), &["method", "params"]),
        call("ListSessions", list, &["method"]),
        call("UnarchiveSession", session(), &["method", "params"]),
    ]})
}

/// `{"set": value}` or the literal `"clear"`.
fn field_patch(value: Value) -> Value {
    json!({"oneOf":[object(&[("set", value)], &["set"]), enumeration(&["clear"])]})
}

/// All fields optional; the daemon refuses an empty patch.
fn session_patch() -> Value {
    object(
        &[
            (
                "title",
                json!({"type":"string","minLength":1,"maxLength":512}),
            ),
            (
                "description",
                field_patch(json!({"type":"string","maxLength":8192})),
            ),
            ("rating", field_patch(integer(1, 10))),
            (
                "active_task",
                field_patch(json!({"type":"string","minLength":1,"maxLength":8192})),
            ),
            ("label", field_patch(uuid())),
            (
                "tags",
                json!({"type":"array","minItems":1,"maxItems":32,"items":string()}),
            ),
        ],
        &[],
    )
}

fn prepare_control() -> Value {
    let mut variants = Vec::new();
    for action in ["resume_lead", "pause_lead", "retry_lead", "replace_lead"] {
        let text_field = match action {
            "pause_lead" => "reason",
            "replace_lead" => "query",
            _ => "message",
        };
        let mut fields = vec![("epic_id", uuid()), (text_field, string())];
        let mut required = vec!["epic_id", text_field];
        if action == "retry_lead" {
            fields.push(("launch", optional(launch())));
        }
        if action == "replace_lead" {
            fields.push(("launch", launch()));
            required.push("launch");
        }
        variants.push(tagged("action", action, &fields, &required));
    }
    variants.push(tagged(
        "action",
        "create_session",
        &[
            ("parent_id", uuid()),
            (
                "kind",
                enumeration(&[
                    "Standard", "Story", "Task", "Bug", "Feature", "Refactor", "Research",
                ]),
            ),
            ("query", string()),
            ("launch", launch()),
        ],
        &["parent_id", "kind", "query", "launch"],
    ));
    variants.push(tagged(
        "action",
        "assign_lead",
        &[("epic_id", uuid()), ("session_id", optional(uuid()))],
        &["epic_id"],
    ));
    object(&[("operation", json!({"oneOf":variants}))], &["operation"])
}

fn commit_prepared_control() -> Value {
    object(
        &[
            ("prepared_id", uuid()),
            (
                "target_digest",
                json!({"type":"string","pattern":"^sha256:[0-9a-f]{64}$"}),
            ),
            (
                "idempotency_key",
                json!({"type":"string","minLength":1,"maxLength":128}),
            ),
        ],
        &["prepared_id", "target_digest", "idempotency_key"],
    )
}

fn get_action() -> Value {
    object(&[("operation_id", uuid())], &["operation_id"])
}

fn submit_review() -> Value {
    let finding = object(
        &[
            (
                "key",
                json!({"type":"string","minLength":1,"maxLength":64,"pattern":"^[A-Za-z0-9_-]+$"}),
            ),
            ("severity", enumeration(&["info", "warning", "error"])),
            (
                "summary",
                json!({"type":"string","minLength":1,"maxLength":2048}),
            ),
            (
                "location",
                optional(json!({"type":"string","minLength":1,"maxLength":1024})),
            ),
            ("blocking", boolean()),
        ],
        &["key", "severity", "summary", "blocking"],
    );
    object(
        &[
            ("assignment_id", uuid()),
            (
                "verdict",
                enumeration(&["accepted", "changes_requested", "blocked"]),
            ),
            (
                "findings",
                json!({"type":"array","maxItems":64,"items":finding}),
            ),
            (
                "idempotency_key",
                json!({"type":"string","minLength":1,"maxLength":128}),
            ),
        ],
        &["assignment_id", "verdict", "findings", "idempotency_key"],
    )
}

#[cfg(test)]
mod succession_tests {
    use super::*;

    #[test]
    fn root_succession_schema_exposes_only_content_and_observed_fences() {
        let schema = control();
        let operation = schema["properties"]["operation"]["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .find(|variant| variant["properties"]["action"]["enum"][0] == "succeed_manager")
            .unwrap();
        assert_eq!(
            operation["required"],
            json!(["action", "expected", "launch", "handoff"])
        );
        assert_eq!(operation["properties"].as_object().unwrap().len(), 4);
        for node in [
            operation,
            &operation["properties"]["expected"],
            &operation["properties"]["launch"],
            &operation["properties"]["handoff"],
        ] {
            assert_eq!(node["additionalProperties"], false);
        }
        let expected = &operation["properties"]["expected"];
        assert_eq!(expected["properties"].as_object().unwrap().len(), 2);
        assert_eq!(expected["properties"]["authority_epoch"]["minimum"], 1);
        assert_eq!(expected["required"], json!(["authority_epoch"]));
        let handoff = &operation["properties"]["handoff"];
        assert_eq!(
            handoff["required"],
            json!(["source_commit", "relative_path", "blob_oid"])
        );
        assert_eq!(handoff["properties"].as_object().unwrap().len(), 3);
        assert_eq!(handoff["properties"]["relative_path"]["maxLength"], 1024);
    }

    #[test]
    fn prepared_control_schema_exposes_semantics_without_observed_fences() {
        let schema = prepare_control();
        assert_eq!(schema["required"], json!(["operation"]));
        let variants = schema["properties"]["operation"]["oneOf"]
            .as_array()
            .unwrap();
        assert_eq!(variants.len(), 6);
        let encoded = schema.to_string();
        for forbidden in [
            "scope_version",
            "policy_version",
            "lead_session_id",
            "lead_generation",
            "event_sequence",
            "custody_generation",
        ] {
            assert!(!encoded.contains(forbidden));
        }
    }
}

fn update() -> Value {
    let mut variants = vec![];
    for name in [
        "work",
        "stage",
        "dependency",
        "ownership",
        "migration",
        "migration_transfer",
        "migration_release",
        "request_review",
        "accept",
        "integration",
        "decision",
    ] {
        let mut fields = vec![
            ("key", string()),
            ("expected_row_version", integer(0, i64::MAX)),
        ];
        let mut required = vec!["key", "expected_row_version"];
        let (extra, required_extra): (Vec<(&str, Value)>, Vec<&str>) = match name {
            "work" => (
                vec![
                    ("epic_id", uuid()),
                    ("title", string()),
                    ("kind", enumeration(&["program", "product"])),
                    ("priority", integer(0, 255)),
                    ("weight", integer(0, 65535)),
                    ("required_gates", array(stage())),
                ],
                vec![
                    "epic_id",
                    "title",
                    "kind",
                    "priority",
                    "weight",
                    "required_gates",
                ],
            ),
            "stage" => (
                vec![
                    ("stage", stage()),
                    (
                        "state",
                        enumeration(&[
                            "unknown", "pending", "running", "partial", "passed", "failed",
                            "blocked",
                        ]),
                    ),
                    ("note", string()),
                    ("evidence", optional(evidence())),
                ],
                vec!["stage", "state", "note"],
            ),
            "dependency" => (
                vec![
                    ("prerequisite", string()),
                    ("require_integrated", boolean()),
                    ("enabled", boolean()),
                ],
                vec!["prerequisite", "require_integrated", "enabled"],
            ),
            "ownership" => (
                vec![
                    ("domain", string()),
                    ("mode", enumeration(&["shared", "exclusive"])),
                    ("files", array(string())),
                    ("active", boolean()),
                ],
                vec!["domain", "mode", "files", "active"],
            ),
            "migration" => (
                vec![
                    ("version", integer(0, u32::MAX.into())),
                    ("baseline_commit", string()),
                    ("inventory_digest", string()),
                ],
                vec!["version", "baseline_commit", "inventory_digest"],
            ),
            "migration_transfer" | "migration_release" => (
                vec![("version", integer(0, u32::MAX.into()))],
                vec!["version"],
            ),
            "request_review" => (
                vec![
                    (
                        "source_commit",
                        json!({"type":"string","pattern":"^[0-9a-f]{40}$"}),
                    ),
                    ("query", string()),
                    ("launch", launch()),
                ],
                vec!["source_commit", "query", "launch"],
            ),
            "accept" => (vec![], vec![]),
            "integration" => (
                vec![
                    ("source_commit", string()),
                    ("target_commit", string()),
                    ("verification", optional(evidence())),
                ],
                vec!["source_commit", "target_commit", "verification"],
            ),
            "decision" => (
                vec![
                    ("epic_id", uuid()),
                    ("question", string()),
                    ("request_id", optional(uuid())),
                    ("work_key", optional(string())),
                ],
                vec!["epic_id", "question"],
            ),
            _ => unreachable!(),
        };
        fields.extend(extra);
        required.extend(required_extra);
        variants.push(tagged("update", name, &fields, &required));
    }
    variants.push(tagged(
        "update",
        "request",
        &[
            ("request_id", uuid()),
            ("expected_row_version", integer(0, i64::MAX)),
            (
                "state",
                enumeration(&[
                    "queued",
                    "retrieved",
                    "accepted",
                    "declined",
                    "running",
                    "completed",
                    "failed",
                    "blocked",
                ]),
            ),
            ("message", string()),
            ("work_key", optional(string())),
        ],
        &["request_id", "expected_row_version", "state", "message"],
    ));
    variants.push(tagged(
        "update",
        "handoff",
        &[("summary", string()), ("next_actions", array(string()))],
        &["summary", "next_actions"],
    ));
    request("change", variants)
}

#[cfg(test)]
mod tests;
