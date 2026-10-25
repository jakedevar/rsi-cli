//! #2313: one parameter name per concept across the agent verb set.
//!
//! Canonical names (the inventory table is in
//! `docs/agents/agent-verb-parameter-names.md`):
//!
//! | concept | canonical | accepted deprecated alias |
//! | --- | --- | --- |
//! | session a verb acts on | `session_id` | `target_session_id` |
//! | set of sessions | `session_ids` | none |
//! | session whose sandbox/branch is run or landed | `source_session_id` | `sandbox_session_id` |
//!
//! These tests fail when a schema introduces a session-identifying name that is
//! neither canonical, nor a registered deprecated alias, nor one of the closed
//! set of names that carry a distinct concept.

use super::{AgentControlVerbV1, DEPRECATED_PARAMETER_ALIASES, agent_control_catalog_v1};
use serde_json::{Value, json};
use std::collections::BTreeSet;

/// `*_session_id` names that name a different concept than "the session this
/// verb acts on". A new `*_session_id` name must join this list on purpose
/// (and the docs table), or reuse `session_id` / `source_session_id`.
const DISTINCT_SESSION_ID_CONCEPTS: &[&str] = &[
    // Fences (the value the caller last observed), not the target.
    "expected_target_session_id",
    "expected_tip_session_id",
    // A satellite's own session id, not a hub session.
    "remote_session_id",
    // The lead session of an Epic (a manager-control selector).
    "lead_session_id",
    // The manager session that owns a decision.
    "owner_manager_session_id",
    // The root session of a manager seat.
    "seat_root_session_id",
    // The session a wake watches for completion.
    "watch_session_id",
];

/// Canonical names for the concepts above.
const CANONICAL_SESSION_ID_NAMES: &[&str] = &["session_id", "source_session_id"];

/// Synonyms that must never appear as a live (non-deprecated) property.
const FORBIDDEN_SYNONYMS: &[(&str, &str)] = &[
    ("target_session_id", "session_id"),
    ("sandbox_session_id", "source_session_id"),
    ("child_session_id", "session_id"),
    ("worker_session_id", "session_id"),
    ("session", "session_id"),
];

/// Every `(property name, property schema, owning properties object)` below
/// `schema`, recursing through nested objects, arrays and combinators.
fn collect<'a>(schema: &'a Value, out: &mut Vec<(String, &'a Value, &'a Value)>) {
    let Some(object) = schema.as_object() else {
        return;
    };
    if let Some(properties) = object.get("properties").and_then(Value::as_object) {
        for (name, child) in properties {
            out.push((name.clone(), child, &object["properties"]));
            collect(child, out);
        }
    }
    for key in ["items", "additionalProperties", "if", "then", "else"] {
        if let Some(child) = object.get(key) {
            collect(child, out);
        }
    }
    for key in ["oneOf", "anyOf", "allOf"] {
        for child in object
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            collect(child, out);
        }
    }
}

fn is_deprecated(property: &Value) -> bool {
    property["deprecated"] == json!(true)
}

fn registered(verb: AgentControlVerbV1, name: &str) -> bool {
    DEPRECATED_PARAMETER_ALIASES
        .iter()
        .any(|(v, alias, _)| *v == verb && *alias == name)
}

#[test]
fn session_identifying_parameter_names_are_canonical_or_distinct() {
    for descriptor in agent_control_catalog_v1() {
        let schema = descriptor.parameters();
        let mut properties = Vec::new();
        collect(&schema, &mut properties);
        for (name, property, _) in properties {
            if let Some((_, canonical)) = FORBIDDEN_SYNONYMS.iter().find(|(bad, _)| *bad == name) {
                assert!(
                    is_deprecated(property) && registered(descriptor.verb, &name),
                    "{}: parameter `{name}` is a non-canonical name; use `{canonical}` (or, to \
                     rename a shipped parameter, register a deprecated alias in \
                     DEPRECATED_PARAMETER_ALIASES)",
                    descriptor.method
                );
                continue;
            }
            if name.ends_with("_session_id") {
                assert!(
                    CANONICAL_SESSION_ID_NAMES.contains(&name.as_str())
                        || DISTINCT_SESSION_ID_CONCEPTS.contains(&name.as_str()),
                    "{}: new session-id parameter `{name}`; use `session_id` (the session the \
                     verb acts on) or `source_session_id` (the session whose sandbox/branch is \
                     run or landed), or add it to DISTINCT_SESSION_ID_CONCEPTS with its concept \
                     and the docs table",
                    descriptor.method
                );
            }
            // Plural `_ids` parameters are lists; singular `_id` ones are not.
            let types = property["type"]
                .as_str()
                .map(|t| vec![t.to_string()])
                .or_else(|| {
                    property["type"].as_array().map(|a| {
                        a.iter()
                            .filter_map(|t| t.as_str().map(String::from))
                            .collect()
                    })
                })
                .unwrap_or_default();
            if name.ends_with("_ids") {
                assert!(
                    types.iter().any(|t| t == "array"),
                    "{}: `{name}` must be an array",
                    descriptor.method
                );
            } else if name.ends_with("_id") {
                assert!(
                    !types.iter().any(|t| t == "array"),
                    "{}: `{name}` is an array; name lists `<concept>_ids`",
                    descriptor.method
                );
            }
        }
    }
}

#[test]
fn deprecated_aliases_are_registered_documented_and_sit_beside_their_canonical_name() {
    let mut seen = BTreeSet::new();
    for descriptor in agent_control_catalog_v1() {
        let schema = descriptor.parameters();
        let mut properties = Vec::new();
        collect(&schema, &mut properties);
        for (name, property, siblings) in properties {
            if !is_deprecated(property) {
                continue;
            }
            let entry = DEPRECATED_PARAMETER_ALIASES
                .iter()
                .find(|(v, alias, _)| *v == descriptor.verb && *alias == name)
                .unwrap_or_else(|| {
                    panic!(
                        "{}: deprecated parameter `{name}` is not in DEPRECATED_PARAMETER_ALIASES",
                        descriptor.method
                    )
                });
            seen.insert((entry.0.descriptor().method, entry.1));
            let canonical = &siblings[entry.2];
            assert!(
                canonical.is_object() && !is_deprecated(canonical),
                "{}: alias `{name}` needs a live canonical `{}` property",
                descriptor.method,
                entry.2
            );
            assert_eq!(
                property["format"], canonical["format"],
                "{}: alias `{name}` must describe the same value as `{}`",
                descriptor.method, entry.2
            );
            let description = property["description"].as_str().unwrap_or_default();
            assert!(
                description.contains(entry.2) && description.contains("Deprecated"),
                "{}: alias `{name}` description must carry the deprecation note naming `{}`",
                descriptor.method,
                entry.2
            );
        }
    }
    for (verb, alias, _) in DEPRECATED_PARAMETER_ALIASES {
        assert!(
            seen.contains(&(verb.descriptor().method, *alias)),
            "{}: registered alias `{alias}` is missing from the schema",
            verb.descriptor().method
        );
    }
}

#[test]
fn published_examples_use_canonical_names_only() {
    for descriptor in agent_control_catalog_v1() {
        let example = descriptor.verb.example();
        for (verb, alias, _) in DEPRECATED_PARAMETER_ALIASES {
            if *verb == descriptor.verb {
                assert!(
                    example.get(*alias).is_none(),
                    "{} example uses deprecated `{alias}`",
                    descriptor.method
                );
            }
        }
    }
}

#[test]
fn deprecated_aliases_decode_to_the_same_request_as_the_canonical_name() {
    for (verb, alias, canonical) in DEPRECATED_PARAMETER_ALIASES {
        let mut new = verb.example();
        if *verb == AgentControlVerbV1::SubmitJob {
            // A source session is only accepted for landing and cloud_gate jobs.
            new = json!({"kind": "landing", "params": {"accepted": "a".repeat(40)}});
        }
        let mut with_alias = new.clone();
        let object = with_alias.as_object_mut().unwrap();
        let value = object.remove(*canonical);
        // The canonical name may be optional in the example (SubmitJob).
        let value = value.unwrap_or_else(|| json!("5d73c05d-1040-49f7-92ab-0123456789ab"));
        let mut canonical_request = new;
        canonical_request
            .as_object_mut()
            .unwrap()
            .insert((*canonical).to_string(), value.clone());
        object.insert((*alias).to_string(), value);
        assert_eq!(
            verb.validate_params(&with_alias),
            Ok(()),
            "{}: alias `{alias}` must still validate",
            verb.descriptor().method
        );
        assert_eq!(
            verb.validate_params(&canonical_request),
            Ok(()),
            "{}: canonical `{canonical}` must validate",
            verb.descriptor().method
        );
        // Both names together are a duplicate field, never silently merged.
        let mut both = canonical_request.clone();
        both.as_object_mut()
            .unwrap()
            .insert((*alias).to_string(), with_alias[*alias].clone());
        assert!(
            verb.validate_params(&both).is_err(),
            "{}: sending both `{alias}` and `{canonical}` must be refused",
            verb.descriptor().method
        );
    }
}

/// #2636: the schema itself (what schema-driven clients and `schema_error`
/// see, not `validate_params`' canonicalizing path) must accept an alias-only
/// request and refuse both names together. A deprecated alias must never leave
/// its canonical name in `required`, and every registered pair must sit in an
/// `x-exactly-one-of` (canonical was required) or `x-at-most-one-of` group.
#[test]
fn schemas_accept_alias_only_requests_and_refuse_both_names() {
    for (verb, alias, canonical) in DEPRECATED_PARAMETER_ALIASES {
        let method = verb.descriptor().method;
        let schema = verb.descriptor().parameters();
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(
            !required.contains(canonical) && !required.contains(alias),
            "{method}: `{canonical}`/`{alias}` must be expressed as a name group, not `required`"
        );
        let in_group = |keyword: &str| {
            schema[keyword].as_array().is_some_and(|groups| {
                groups.iter().any(|group| {
                    group == &json!([canonical, alias]) || group == &json!([alias, canonical])
                })
            })
        };
        assert!(
            in_group("x-exactly-one-of") || in_group("x-at-most-one-of"),
            "{method}: alias `{alias}` of `{canonical}` is not in a name group"
        );
        let exactly_one = in_group("x-exactly-one-of");

        let id = json!("5d73c05d-1040-49f7-92ab-0123456789ab");
        let mut base = verb.example();
        base.as_object_mut().unwrap().remove(*canonical);
        base.as_object_mut().unwrap().remove(*alias);
        let with = |name: &str| {
            let mut request = base.clone();
            request[name] = id.clone();
            request
        };
        assert_eq!(
            verb.schema_error(&with(canonical)),
            None,
            "{method}: canonical"
        );
        assert_eq!(
            verb.schema_error(&with(alias)),
            None,
            "{method}: alias-only request must satisfy the schema"
        );
        let mut both = with(canonical);
        both[*alias] = id.clone();
        assert!(
            verb.schema_error(&both).is_some(),
            "{method}: `{canonical}` and `{alias}` together must be refused by the schema"
        );
        assert_eq!(
            verb.schema_error(&base).is_some(),
            exactly_one,
            "{method}: neither name is an error only when the canonical name is mandatory"
        );
        if exactly_one {
            let error = verb.schema_error(&base).unwrap();
            assert_eq!(error.path.as_deref(), Some(&*format!("params.{canonical}")));
        }
    }
}
