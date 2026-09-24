//! Version-bound, independent generated-schema checks. Fixtures are copied or
//! narrowly projected from codex-cli 0.153.4; see fixtures/.../provenance.json.
use super::*;
use sha2::{Digest, Sha256};

const COMMAND: &str =
    include_str!("fixtures/approval-0.153.4/CommandExecutionRequestApprovalResponse.json");
const FILE: &str = include_str!("fixtures/approval-0.153.4/FileChangeRequestApprovalResponse.json");
const ID: &str = include_str!("fixtures/approval-0.153.4/RequestId.json");
const METHODS: &str = include_str!("fixtures/approval-0.153.4/ServerRequest-methods.json");
const PARAMS: &str = include_str!("fixtures/approval-0.153.4/ApprovalParams-required.json");
const PROVENANCE: &str = include_str!("fixtures/approval-0.153.4/provenance.json");

// Evaluate only the JSON Schema keywords present in these generated response
// and request-ID fixtures. No production response constants supply this oracle.
fn conforms(root: &Value, schema: &Value, value: &Value) -> bool {
    if let Some(reference) = schema["$ref"].as_str() {
        return root
            .pointer(reference.strip_prefix('#').unwrap())
            .is_some_and(|s| conforms(root, s, value));
    }
    if let Some(variants) = schema["oneOf"].as_array() {
        return variants.iter().filter(|s| conforms(root, s, value)).count() == 1;
    }
    if let Some(variants) = schema["anyOf"].as_array() {
        return variants.iter().any(|s| conforms(root, s, value));
    }
    if let Some(values) = schema["enum"].as_array() {
        if !values.contains(value) {
            return false;
        }
    }
    if let Some(kind) = schema["type"].as_str() {
        if !match kind {
            "object" => value.is_object(),
            "string" => value.is_string(),
            "integer" => value.is_i64(),
            "array" => value.is_array(),
            "boolean" => value.is_boolean(),
            "null" => value.is_null(),
            _ => panic!("fixture contains unevaluated schema type {kind}"),
        } {
            return false;
        }
    }
    if let Some(required) = schema["required"].as_array() {
        if !required
            .iter()
            .all(|key| value.get(key.as_str().unwrap()).is_some())
        {
            return false;
        }
    }
    if let Some(properties) = schema["properties"].as_object() {
        if !properties
            .iter()
            .all(|(key, s)| value.get(key).is_none_or(|v| conforms(root, s, v)))
        {
            return false;
        }
        if schema["additionalProperties"] == false
            && value
                .as_object()
                .is_some_and(|v| v.keys().any(|key| !properties.contains_key(key)))
        {
            return false;
        }
    }
    true
}

pub(crate) fn params() -> Value {
    json!({"threadId":"approval-transport-thread","turnId":"turn-controlled","itemId":"item-controlled","startedAtMs":1788820000000_i64,
        "command":"inspect exact source","reason":"Approve the same operation"})
}

pub(crate) fn assert_response(method: &str, request_id: &Value, response: &Value) {
    let id_schema: Value = serde_json::from_str(ID).unwrap();
    assert!(
        conforms(&id_schema, &id_schema, request_id),
        "ID must conform to generated RequestId"
    );
    let methods: Value = serde_json::from_str(METHODS).unwrap();
    let source = match methods[method].as_str() {
        Some("#/definitions/CommandExecutionRequestApprovalParams") => COMMAND,
        Some("#/definitions/FileChangeRequestApprovalParams") => FILE,
        other => panic!("unsupported response oracle {other:?}"),
    };
    let schema: Value = serde_json::from_str(source).unwrap();
    assert_eq!(
        response["id"], *request_id,
        "JSON ID value and type are exact"
    );
    assert_eq!(response["jsonrpc"], "2.0");
    assert!(
        conforms(&schema, &schema, &response["result"]),
        "response violates generated {method} schema: {response}"
    );
}

pub(crate) fn assert_request(method: &str, request_id: &Value, params: &Value) {
    let methods: Value = serde_json::from_str(METHODS).unwrap();
    let params_ref = methods[method]
        .as_str()
        .unwrap()
        .strip_prefix("#/definitions/")
        .unwrap();
    let schemas: Value = serde_json::from_str(PARAMS).unwrap();
    let schema = &schemas[format!("{params_ref}.json")];
    assert!(schema.is_object());
    assert!(
        conforms(schema, schema, params),
        "request must carry current generated required identity fields"
    );
    let schema: Value = serde_json::from_str(ID).unwrap();
    assert!(conforms(&schema, &schema, request_id));
}

#[test]
fn generated_approval_schema_fixtures_are_pinned_and_reject_old_boolean_responses() {
    let provenance: Value = serde_json::from_str(PROVENANCE).unwrap();
    for (name, raw) in [
        ("CommandExecutionRequestApprovalResponse.json", COMMAND),
        ("FileChangeRequestApprovalResponse.json", FILE),
        ("RequestId.json", ID),
    ] {
        assert_eq!(
            format!("{:x}", Sha256::digest(raw.as_bytes())),
            provenance["source_sha256"][name]
        );
    }
    for raw in [COMMAND, FILE] {
        let schema: Value = serde_json::from_str(raw).unwrap();
        assert!(
            !conforms(
                &schema,
                &schema,
                &json!({"approved":true,"approveForSession":false})
            ),
            "the prior boolean response must fail the independent schema"
        );
        for decision in ["accept", "decline", "acceptForSession", "cancel"] {
            assert!(conforms(&schema, &schema, &json!({"decision":decision})));
        }
    }
}

#[test]
fn generated_server_request_inventory_separates_unsupported_protocols() {
    let methods: Value = serde_json::from_str(METHODS).unwrap();
    assert_eq!(
        APPROVAL_METHODS,
        [
            "item/commandExecution/requestApproval",
            "item/fileChange/requestApproval"
        ]
    );
    for method in APPROVAL_METHODS {
        assert!(methods.get(*method).is_some());
    }
    for method in [
        "item/permissions/requestApproval",
        "item/tool/requestUserInput",
        "mcpServer/elicitation/request",
        "execCommandApproval",
        "applyPatchApproval",
    ] {
        assert!(methods.get(method).is_some());
        assert!(
            approval_response(
                &json!("opaque-id"),
                method,
                &params(),
                ApprovalDecision::Approve
            )
            .is_err()
        );
    }
}

#[tokio::test]
async fn both_provider_and_writer_paths_conform_for_all_supported_methods_ids_and_answers() {
    let (write_tx, mut write_rx) = mpsc::channel(16);
    let (_, event_rx) = mpsc::channel(1);
    let mut session = CodexAppServerSession {
        thread_id: "approval-transport-thread".into(),
        working_dir: PathBuf::from("/var/tmp"),
        write_tx,
        event_rx,
        next_id: Arc::new(AtomicI64::new(1)),
        turn_control: Arc::new(crate::model_control::call_control::NoopModelCallControl),
        current_turn_call: None,
    };
    for method in [
        "item/commandExecution/requestApproval",
        "item/fileChange/requestApproval",
    ] {
        for id in [json!(42), json!("42")] {
            for (choice, expected) in [
                (ApprovalDecision::Approve, "accept"),
                (ApprovalDecision::Deny, "decline"),
                (ApprovalDecision::ApproveForSession, "acceptForSession"),
            ] {
                let params = params();
                assert_request(method, &id, &params);
                for direct in [true, false] {
                    if direct {
                        session
                            .send_approval(&id, method, &params, choice.clone())
                            .await
                            .unwrap();
                    } else {
                        session
                            .writer()
                            .send_approval(&id, method, &params, choice.clone())
                            .await
                            .unwrap();
                    }
                    let response: Value =
                        serde_json::from_slice(&write_rx.recv().await.unwrap()).unwrap();
                    assert_response(method, &id, &response);
                    assert_eq!(
                        response,
                        json!({"jsonrpc":"2.0","id":id,"result":{"decision":expected}})
                    );
                }
            }
        }
    }
}

#[test]
fn offered_decisions_constrain_replies_without_inventing_cancel_or_policy_grants() {
    let mut p = params();
    p["availableDecisions"] = json!(["decline"]);
    assert!(
        approval_response(
            &json!("request"),
            APPROVAL_METHODS[0],
            &p,
            ApprovalDecision::Approve
        )
        .is_err()
    );
    assert_eq!(
        approval_response(
            &json!("request"),
            APPROVAL_METHODS[0],
            &p,
            ApprovalDecision::Deny
        )
        .unwrap()["result"],
        json!({"decision":"decline"})
    );
    for available in [
        json!([]),
        json!({}),
        json!(["cancel"]),
        json!([{"acceptWithExecpolicyAmendment":{"execpolicy_amendment":["echo"]}}]),
    ] {
        p["availableDecisions"] = available;
        assert!(
            approval_response(
                &json!("request"),
                APPROVAL_METHODS[0],
                &p,
                ApprovalDecision::Approve
            )
            .is_err()
        );
        assert!(
            approval_response(
                &json!("request"),
                APPROVAL_METHODS[0],
                &p,
                ApprovalDecision::Deny
            )
            .is_err()
        );
    }
}
