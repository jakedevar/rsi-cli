use super::*;
use crate::harness_manager_v2::{AgentManagerControlRequestV2, AgentManagerUpdateRequestV2};

// Check the advertised structural vocabulary against real DTO decoding; no
// runtime permission or evidence acceptance is inferred from a schema match.
fn accepts(schema: &Value, value: &Value) -> bool {
    if let Some(choices) = schema.get("oneOf").and_then(Value::as_array) {
        return choices.iter().filter(|s| accepts(s, value)).count() == 1;
    }
    if let Some(choices) = schema.get("anyOf").and_then(Value::as_array) {
        return choices.iter().any(|s| accepts(s, value));
    }
    if let Some(options) = schema.get("enum").and_then(Value::as_array) {
        if !options.contains(value) {
            return false;
        }
    }
    match schema["type"].as_str().unwrap() {
        "object" => value.as_object().is_some_and(|v| {
            let properties = schema["properties"].as_object().unwrap();
            schema["required"]
                .as_array()
                .unwrap()
                .iter()
                .all(|k| v.contains_key(k.as_str().unwrap()))
                && v.iter()
                    .all(|(k, v)| properties.get(k).is_some_and(|s| accepts(s, v)))
        }),
        "array" => value
            .as_array()
            .is_some_and(|v| v.iter().all(|v| accepts(&schema["items"], v))),
        "string" => value.is_string(),
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "integer" => value.as_i64().is_some_and(|v| {
            schema["minimum"].as_i64().is_none_or(|min| v >= min)
                && schema["maximum"].as_i64().is_none_or(|max| v <= max)
        }),
        other => panic!("unhandled schema vocabulary {other}"),
    }
}
fn objects(value: &Value, path: String, out: &mut Vec<String>) {
    match value {
        Value::Object(values) => {
            out.push(path.clone());
            for (key, value) in values {
                objects(value, format!("{path}/{key}"), out);
            }
        }
        Value::Array(values) => {
            for (i, value) in values.iter().enumerate() {
                objects(value, format!("{path}/{i}"), out)
            }
        }
        _ => {}
    }
}

#[test]
fn every_manager_action_and_update_roundtrips_and_rejects_nested_authority_injection() {
    let id = "5d73c05d-1040-49f7-92ab-0123456789ab";
    let expected =
        json!({"lead_session_id":id,"lead_generation":2,"event_sequence":3,"custody_generation":1});
    let launch = json!({"provider":"Codex","model":"configured-model","effort":"high"});
    let evidence = json!({"source_session_id":id,"source_commit":"a".repeat(40),"artifact_path":"proof.md","artifact_commit":"b".repeat(40),"closure_evidence_id":id});
    let actions = vec![
        json!({"action":"succeed_manager","expected":{"authority_epoch":3,"custody_generation":1},"launch":launch,"handoff":{"source_commit":"a".repeat(40),"relative_path":"thoughts/shared/handoffs/manager.md","blob_oid":"b".repeat(40)}}),
        json!({"action":"resume_lead","epic_id":id,"expected":expected,"message":"resume"}),
        json!({"action":"pause_lead","epic_id":id,"expected":expected,"reason":"hold"}),
        json!({"action":"retry_lead","epic_id":id,"expected":expected,"message":"retry","launch":launch}),
        json!({"action":"replace_lead","epic_id":id,"expected":expected,"query":"continue","launch":launch}),
        json!({"action":"create_container","parent_id":id,"kind":"Epic","name":"Identity","tags":["scope"]}),
        json!({"action":"update_container","container_id":id,"expected_updated_at":"2026-09-07T00:00:00.000000000Z","name":"Identity","description":"description"}),
        json!({"action":"archive_container","container_id":id,"expected_updated_at":"2026-09-07T00:00:00.000000000Z"}),
        json!({"action":"delete_container","container_id":id,"expected_updated_at":"2026-09-07T00:00:00.000000000Z"}),
        json!({"action":"restore_container","container_id":id,"expected_updated_at":"2026-09-07T00:00:00.000000000Z"}),
        json!({"action":"create_session","parent_id":id,"kind":"Task","query":"implement","launch":launch}),
        json!({"action":"assign_lead","epic_id":id,"expected":expected,"session_id":id}),
        json!({"action":"settle_uncertain_action","operation_id":id,"expected_row_version":3}),
        json!({"action":"retire_lead_continuations","epic_id":id,"expected":expected}),
        json!({"action":"archive_session","session_id":id,"expected_updated_at":"2026-09-07T00:00:00.000000000Z"}),
        json!({"action":"restore_session","session_id":id,"expected_updated_at":"2026-09-07T00:00:00.000000000Z"}),
        json!({"action":"update_session","session_id":id,"expected_updated_at":"2026-09-07T00:00:00.000000000Z","patch":{"title":"Renamed","description":{"set":"why"},"rating":"clear","active_task":{"set":"task"},"label":{"set":id},"tags":["k7"]}}),
        json!({"action":"operator_call","call":{"method":"ListSessions","params":{"status_in":["Completed"],"after":{"updated_at":"2026-09-07T00:00:00.000000000Z","id":id},"limit":10}},"expected":{"session_updated_at":"2026-09-07T00:00:00.000000000Z"}}),
    ];
    let updates = vec![
        json!({"update":"work","key":"w","expected_row_version":0,"epic_id":id,"title":"Deliverable","kind":"product","priority":1,"weight":1,"required_gates":["verification"]}),
        json!({"update":"stage","key":"w","expected_row_version":1,"stage":"verification","state":"passed","note":"proof","evidence":evidence}),
        json!({"update":"dependency","key":"w","expected_row_version":1,"prerequisite":"base","require_integrated":true,"enabled":true}),
        json!({"update":"ownership","key":"w","expected_row_version":1,"domain":"domain","mode":"exclusive","files":["file.rs"],"active":true}),
        json!({"update":"migration","key":"w","expected_row_version":1,"version":105,"baseline_commit":"a".repeat(40),"inventory_digest":"digest"}),
        json!({"update":"migration_transfer","key":"new-holder","expected_row_version":1,"version":105}),
        json!({"update":"migration_release","key":"w","expected_row_version":2,"version":105}),
        json!({"update":"request_review","key":"w","expected_row_version":1,"source_commit":"a".repeat(40),"query":"Review exact source","launch":launch}),
        json!({"update":"accept","key":"w","expected_row_version":1}),
        json!({"update":"integration","key":"w","expected_row_version":1,"source_commit":"a".repeat(40),"target_commit":"b".repeat(40),"verification":evidence}),
        json!({"update":"request","request_id":id,"expected_row_version":1,"state":"accepted","message":"accepted","work_key":"w"}),
        json!({"update":"decision","key":"d","expected_row_version":0,"epic_id":id,"question":"Select destination","request_id":id,"work_key":"w"}),
        json!({"update":"handoff","summary":"Paused by operator","next_actions":["await exact decision"]}),
    ];
    for (field, variants, schema) in [
        ("operation", actions, control()),
        ("change", updates, update()),
    ] {
        assert_eq!(
            schema["properties"][field]["oneOf"]
                .as_array()
                .unwrap()
                .len(),
            variants.len()
        );
        for variant in variants {
            let value = json!({"fence":{"scope_version":1,"policy_version":2},"idempotency_key":"one",field:variant});
            assert!(accepts(&schema, &value), "{value}");
            let decode = |value: Value| -> Result<Value, serde_json::Error> {
                if field == "operation" {
                    serde_json::from_value::<AgentManagerControlRequestV2>(value)
                        .and_then(serde_json::to_value)
                } else {
                    serde_json::from_value::<AgentManagerUpdateRequestV2>(value)
                        .and_then(serde_json::to_value)
                }
            };
            let roundtrip = decode(value.clone()).unwrap();
            assert!(accepts(&schema, &roundtrip), "{roundtrip}");
            let mut paths = vec![];
            objects(&value, String::new(), &mut paths);
            for path in paths {
                for injected in ["caller_session_id", "permissions", "admin", "project_id"] {
                    let mut forged = value.clone();
                    forged
                        .pointer_mut(&path)
                        .unwrap()
                        .as_object_mut()
                        .unwrap()
                        .insert(injected.into(), json!(id));
                    assert!(!accepts(&schema, &forged), "{path}/{injected}");
                    assert!(decode(forged).is_err(), "{field}: {path}/{injected}");
                }
            }
            let mut missing = value.clone();
            missing.as_object_mut().unwrap().remove("fence");
            assert!(decode(missing).is_err());
            let mut unknown = value;
            unknown[field][if field == "operation" {
                "action"
            } else {
                "update"
            }] = json!("grant_operator");
            assert!(decode(unknown).is_err());
        }
    }
}

#[test]
fn inspect_defaults_pages_and_provider_alias_follow_the_shared_contract() {
    use crate::harness_manager_v2::AgentManagerInspectRequestV2;
    let default: AgentManagerInspectRequestV2 = serde_json::from_value(json!({})).unwrap();
    assert!(default.validate().is_ok());
    assert!(accepts(&inspect(), &serde_json::to_value(default).unwrap()));
    assert!(accepts(&inspect(), &json!({"section":"archive","limit":1})));
    assert!(accepts(&inspect(), &json!({"section":"health","limit":1})));
    let health =
        serde_json::from_value::<AgentManagerInspectRequestV2>(json!({"section":"health"}))
            .map(|request| request.section);
    assert_eq!(
        health.ok(),
        Some(crate::harness_manager_v2::ManagerInspectSectionV2::Health)
    );
    for limit in [0, 65] {
        let value = json!({"limit":limit});
        assert!(!accepts(&inspect(), &value));
        assert!(
            serde_json::from_value::<AgentManagerInspectRequestV2>(value)
                .unwrap()
                .validate()
                .is_err()
        );
    }
    for provider in launch()["properties"]["provider"]["enum"]
        .as_array()
        .unwrap()
    {
        let value = json!({"provider":provider,"model":"configured-model"});
        assert!(
            serde_json::from_value::<crate::harness_manager_v2::ManagerLaunchChoiceV2>(value)
                .is_ok()
        );
    }
}

/// K7 lead decision (option B): per-session housekeeping is published only on
/// `AgentManagerControl` (like the container actions), under `SessionControl`;
/// `AgentManagerPrepareControl` stays the six lead-lifecycle actions, whose kinds
/// are also pinned by the released V116 `manager_prepared_actions` CHECK.
#[test]
#[allow(clippy::unwrap_used)]
fn housekeeping_actions_are_control_only_and_prepared_subset_is_lifecycle_only() {
    use crate::harness_manager_v2::{ManagerCapabilityV2, PreparedManagerActionV2};
    let actions = |schema: &Value| -> Vec<String> {
        let mut names = schema["properties"]["operation"]["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .map(|variant| {
                variant["properties"]["action"]["enum"][0]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect::<Vec<_>>();
        names.sort();
        names
    };
    let lifecycle = [
        "assign_lead",
        "create_session",
        "pause_lead",
        "replace_lead",
        "resume_lead",
        "retry_lead",
    ];
    assert_eq!(actions(&prepare_control()), lifecycle);
    // The prepared DTO decodes a lifecycle semantic action with the same tag.
    let assign = json!({"action":"assign_lead","epic_id":"5d73c05d-1040-49f7-92ab-0123456789ab"});
    assert!(matches!(
        serde_json::from_value::<PreparedManagerActionV2>(assign),
        Ok(PreparedManagerActionV2::AssignLead { .. })
    ));

    let id = "5d73c05d-1040-49f7-92ab-0123456789ab";
    let at = "2026-09-07T00:00:00.000000000Z";
    let control_actions = actions(&control());
    for (name, operation) in [
        (
            "archive_session",
            json!({"action":"archive_session","session_id":id,"expected_updated_at":at}),
        ),
        (
            "restore_session",
            json!({"action":"restore_session","session_id":id,"expected_updated_at":at}),
        ),
        (
            "update_session",
            json!({"action":"update_session","session_id":id,"expected_updated_at":at,"patch":{"title":"Renamed"}}),
        ),
    ] {
        assert!(
            control_actions.iter().any(|action| action == name),
            "{name}"
        );
        let request: AgentManagerControlRequestV2 = serde_json::from_value(json!({
            "fence":{"scope_version":1,"policy_version":1},
            "idempotency_key":"k7",
            "operation":operation,
        }))
        .unwrap();
        assert_eq!(
            request.operation.capability(),
            ManagerCapabilityV2::SessionControl
        );
        assert!(request.operation.housekeeping_session().is_some());
        // Control-only: the prepared DTO decodes exactly the lifecycle subset.
        let semantic = json!({"action":name,"session_id":id});
        assert!(serde_json::from_value::<PreparedManagerActionV2>(semantic).is_err());
    }
}
