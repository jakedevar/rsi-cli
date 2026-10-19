//! Diagnostic walk of the closed catalog's JSON Schema vocabulary. This does
//! not replace DTO validation or runtime authorization.
use super::AgentControlParamErrorV1;
use serde_json::{Value, json};

fn failure(
    path: &str,
    keyword: &str,
    limit: &Value,
    value: Option<&Value>,
) -> AgentControlParamErrorV1 {
    let mut error = AgentControlParamErrorV1::params();
    error.path = Some(path.into());
    error.constraint = Some(format!("{keyword} {limit}"));
    error.value = value.filter(|v| !v.is_object() && !v.is_array()).cloned();
    if let Some(Value::String(text)) = &mut error.value {
        if text.chars().count() > 128 {
            *text = text.chars().take(128).collect::<String>() + "…";
            error.value_truncated = true;
        }
    }
    error
}

fn child_path(path: &str, key: &str) -> String {
    if key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        format!("{path}.{key}")
    } else {
        format!("{path}[{}]", json!(key))
    }
}

fn matches_type(value: &Value, ty: &str) -> bool {
    match ty {
        "null" => value.is_null(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "number" => value.is_number(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        _ => panic!("unsupported catalog schema type: {ty}"),
    }
}

fn compatible_branch(value: &Value, schema: &Value) -> bool {
    if let Some(ty) = schema["type"].as_str() {
        if !matches_type(value, ty) {
            return false;
        }
    }
    schema["properties"].as_object().is_none_or(|properties| {
        properties.iter().all(|(key, child)| {
            value.get(key).is_none_or(|value| {
                child.get("const").is_none_or(|constant| constant == value)
                    && child["enum"]
                        .as_array()
                        .is_none_or(|options| options.contains(value))
            })
        })
    })
}

/// #2636: the flat (provider-safe: no top-level `oneOf`/`not`) way a schema
/// says a canonical parameter and its deprecated alias are interchangeable but
/// never sent together. `x-exactly-one-of` groups need one member (a required
/// parameter that has an alias); `x-at-most-one-of` groups allow none (an
/// optional parameter that has an alias). Returns the offending group's
/// `(keyword, members)`. Standard validators ignore the `x-` keywords, so
/// they accept an alias-only request, which the DTO decodes.
pub(super) fn name_group_violation<'a>(
    object: &serde_json::Map<String, Value>,
    schema: &'a Value,
) -> Option<(&'static str, &'a Value)> {
    for (keyword, minimum) in [("x-exactly-one-of", 1), ("x-at-most-one-of", 0)] {
        for group in schema[keyword].as_array().into_iter().flatten() {
            let present = group
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter(|name| object.contains_key(*name))
                .count();
            if present < minimum || present > 1 {
                return Some((keyword, group));
            }
        }
    }
    None
}

pub(super) fn validate(
    value: &Value,
    schema: &Value,
    path: &str,
) -> Result<(), AgentControlParamErrorV1> {
    let fail = |key: &str| failure(path, key, &schema[key], Some(value));
    if let Some(ty) = schema.get("type") {
        let matches = ty.as_str().is_some_and(|ty| matches_type(value, ty))
            || ty.as_array().is_some_and(|types| {
                types
                    .iter()
                    .any(|ty| matches_type(value, ty.as_str().expect("schema type")))
            });
        if !matches {
            return Err(fail("type"));
        }
    }
    for keyword in ["enum", "const"] {
        if let Some(limit) = schema.get(keyword) {
            let matches = if keyword == "enum" {
                limit.as_array().expect("enum array").contains(value)
            } else {
                limit == value
            };
            if !matches {
                return Err(fail(keyword));
            }
        }
    }
    if let Some(object) = value.as_object() {
        if let Some((keyword, group)) = name_group_violation(object, schema) {
            let members = group
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str);
            let canonical = group[0].as_str().expect("name group member");
            let none_sent = !members.into_iter().any(|name| object.contains_key(name));
            return Err(if none_sent {
                // Nothing sent: the canonical name is the missing one.
                failure(
                    &child_path(path, canonical),
                    "required",
                    &json!(canonical),
                    None,
                )
            } else {
                failure(path, keyword, group, None)
            });
        }
        if let Some(required) = schema["required"].as_array() {
            for key in required {
                let key = key.as_str().expect("required property");
                if !object.contains_key(key) {
                    return Err(failure(
                        &child_path(path, key),
                        "required",
                        &json!(key),
                        None,
                    ));
                }
            }
        }
        for (key, value) in object {
            if let Some(child) = schema["properties"].get(key) {
                validate(value, child, &child_path(path, key))?;
            } else if schema["additionalProperties"] == false {
                return Err(failure(
                    &child_path(path, key),
                    "additionalProperties",
                    &json!(false),
                    Some(value),
                ));
            } else if schema["additionalProperties"].is_object() {
                validate(
                    value,
                    &schema["additionalProperties"],
                    &child_path(path, key),
                )?;
            }
        }
    }
    if let Some(array) = value.as_array() {
        for (key, violated) in [("minItems", true), ("maxItems", false)] {
            if let Some(limit) = schema[key].as_u64() {
                if if violated {
                    (array.len() as u64) < limit
                } else {
                    (array.len() as u64) > limit
                } {
                    return Err(fail(key));
                }
            }
        }
        if schema["uniqueItems"] == true
            && array
                .iter()
                .enumerate()
                .any(|(i, item)| array[..i].contains(item))
        {
            return Err(fail("uniqueItems"));
        }
        if let Some(child) = schema.get("items") {
            for (i, item) in array.iter().enumerate() {
                validate(item, child, &format!("{path}[{i}]"))?;
            }
        }
    }
    if let Some(text) = value.as_str() {
        for (key, minimum) in [("minLength", true), ("maxLength", false)] {
            if let Some(limit) = schema[key].as_u64() {
                // Agent DTO bounds are UTF-8 byte caps; these diagnostics
                // explain the existing refusal rather than applying new policy.
                let length = text.len() as u64;
                if if minimum {
                    length < limit
                } else {
                    length > limit
                } {
                    return Err(fail(key));
                }
            }
        }
        if let Some(pattern) = schema["pattern"].as_str() {
            if !regex::Regex::new(pattern)
                .expect("catalog regex")
                .is_match(text)
            {
                return Err(fail("pattern"));
            }
        }
        let valid = match schema["format"].as_str() {
            Some("uuid") => uuid::Uuid::parse_str(text).is_ok(),
            Some("date-time") => chrono::DateTime::parse_from_rfc3339(text).is_ok(),
            _ => true,
        };
        if !valid {
            return Err(fail("format"));
        }
    }
    if let Some(number) = value.as_f64() {
        for (key, minimum) in [("minimum", true), ("maximum", false)] {
            if let Some(limit) = schema[key].as_f64() {
                if if minimum {
                    number < limit
                } else {
                    number > limit
                } {
                    return Err(fail(key));
                }
            }
        }
    }
    if let Some(children) = schema["allOf"].as_array() {
        for child in children {
            validate(value, child, path)?;
        }
    }
    if let Some(condition) = schema.get("if") {
        let branch = if validate(value, condition, path).is_ok() {
            "then"
        } else {
            "else"
        };
        if let Some(child) = schema.get(branch) {
            validate(value, child, path)?;
        }
    }
    for keyword in ["anyOf", "oneOf"] {
        if let Some(children) = schema[keyword].as_array() {
            let results: Vec<_> = children
                .iter()
                .map(|child| validate(value, child, path))
                .collect();
            let matches = results.iter().filter(|result| result.is_ok()).count();
            if matches == 0 {
                // Follow the matching tagged branch instead of reporting a
                // missing property from an unrelated operation alternative.
                let preferred = children.iter().any(|child| compatible_branch(value, child));
                return Err(results
                    .into_iter()
                    .zip(children)
                    .filter(|(_, child)| !preferred || compatible_branch(value, child))
                    .filter_map(|(result, _)| result.err())
                    .max_by_key(|error| error.path.as_ref().map_or(0, String::len))
                    .unwrap_or_else(|| fail(keyword)));
            }
            if keyword == "oneOf" && matches != 1 {
                return Err(fail(keyword));
            }
        }
    }
    Ok(())
}

pub(super) fn add_job_schemas(schema: &mut Value) {
    use crate::agent_jobs::*;
    let token = json!({"type":["string","null"],"minLength":1,"maxLength":JOB_MAX_TOKEN_BYTES});
    let oid = json!({"type":"string","pattern":"^[0-9a-f]{40}$"});
    let filters = |count, length| json!({"type":"array","maxItems":count,"items":{"type":"string","minLength":1,"maxLength":length}});
    let test = json!({"type":"object","additionalProperties":false,"properties":{
        "package":token,"shard":token,"filterset":{"type":["string","null"]},
        "recipe":token,"candidate_receipt":token,
        "lib_only":{"type":"boolean"},"exact":{"type":"boolean"},
        "test_target":{"type":["string","null"],"minLength":1,"maxLength":JOB_MAX_TOKEN_BYTES,"pattern":"^[A-Za-z0-9_][A-Za-z0-9_-]*$","description":"Integration test name for cargo --test; package only, requires exact:true and nonempty filters, mutually exclusive with lib_only"},
        "filters":filters(JOB_MAX_TEST_FILTERS, JOB_MAX_TOKEN_BYTES),
        "cpu_quota_percent":{"type":["integer","null"],"minimum":JOB_CPU_QUOTA_MIN_PERCENT,"maximum":JOB_CPU_QUOTA_MAX_PERCENT},
        "timeout_minutes":{"type":["integer","null"],"minimum":JOB_TEST_TIMEOUT_MIN_MINS,"maximum":JOB_TEST_TIMEOUT_MAX_MINS},
        "qa_lane":{"type":["object","null"],"additionalProperties":false,"required":["sha"],"properties":{"sha":oid}},
        "scoped_test":{"type":["object","null"],"additionalProperties":false,"required":["base"],"properties":{"base":token,"head":token}}
    },"allOf":[
        {"if":{"required":["recipe"],"properties":{"recipe":{"type":"string"}}},"then":{"properties":{"filters":filters(JOB_RECIPE_MAX_FILTERS, crate::rolling_queue::ROLLING_QUEUE_MAX_FILTER_BYTES)}}},
        {"if":{"required":["qa_lane"],"properties":{"qa_lane":{"type":"object"}}},"then":{"properties":{"timeout_minutes":{"maximum":JOB_QA_LANE_TIMEOUT_MAX_MINS}}}}
    ]});
    // Recipe filters have a different byte cap than package filters. Apply
    // the general item cap only in the non-recipe branch.
    let mut test = test;
    test["properties"]["filters"] = json!({"type":"array"});
    test["allOf"][0]["else"] =
        json!({"properties":{"filters":filters(JOB_MAX_TEST_FILTERS, JOB_MAX_TOKEN_BYTES)}});
    let build = json!({"type":"object","additionalProperties":false,"required":["command"],"properties":{
        "command":{"type":"string","enum":["check","build","test_no_run"]},"package":token,
        "workspace":{"type":"boolean"},"all_targets":{"type":"boolean"},"release":{"type":"boolean"},
        "no_default_features":{"type":"boolean"},
        "features":{"type":"array","maxItems":JOB_MAX_SHARD_SWEEP,"items":{"type":"string","minLength":1,"maxLength":JOB_MAX_TOKEN_BYTES}},
        "shards":{"type":"array","maxItems":JOB_MAX_SHARD_SWEEP,"items":{"type":"string","minLength":1,"maxLength":JOB_MAX_TOKEN_BYTES}},
        "sha":{"type":["string","null"],"pattern":"^[0-9a-f]{40}$"},
        "cpu_quota_percent":{"type":["integer","null"],"minimum":JOB_CPU_QUOTA_MIN_PERCENT,"maximum":JOB_CPU_QUOTA_MAX_PERCENT}
    }});
    let landing = json!({"type":"object","additionalProperties":false,"required":["accepted"],"properties":{
        "accepted":oid,"test_filters":filters(JOB_MAX_TEST_FILTERS, crate::rolling_queue::ROLLING_QUEUE_MAX_FILTER_BYTES)
    }});
    let sweep = json!({"type":"object","additionalProperties":false,"required":["sha"],"properties":{"sha":oid}});
    schema["allOf"] = Value::Array(
        [
            ("test", test),
            ("build", build),
            ("landing", landing.clone()),
            ("cloud_gate", landing),
            ("cloud_sweep", sweep),
        ]
        .into_iter()
        .map(|(kind, params)| {
            json!({
                "if":{"required":["kind"],"properties":{"kind":{"const":kind}}},
                "then":{"properties":{"params":params}}
            })
        })
        .collect(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_control_schema::{AgentControlVerbV1 as Verb, agent_control_catalog_v1};

    #[test]
    fn schema_refusals_name_numeric_limit_and_nested_array_item() {
        let request = json!({"sha":"a".repeat(40),"idempotency_key":"deploy","max_wait_secs":5400});
        let error = Verb::RequestDeploy.validate_params(&request).unwrap_err();
        assert_eq!(error.path.as_deref(), Some("params.max_wait_secs"));
        assert_eq!(error.constraint.as_deref(), Some("maximum 3600"));
        assert_eq!(error.value, Some(json!(5400)));
        assert_eq!((error.class, error.field), ("invalid_input", "params"));

        let request = json!({"source_commit":"a".repeat(40),"idempotency_key":"land",
            "test_filters":["rsi=one", "rsi=two", "rsi=three", "x".repeat(257)]});
        let error = Verb::EnqueueLandingSource
            .validate_params(&request)
            .unwrap_err();
        assert_eq!(error.path.as_deref(), Some("params.test_filters[3]"));
        assert_eq!(error.constraint.as_deref(), Some("maxLength 256"));
        assert_eq!(error.value, Some(json!("x".repeat(128) + "…")));
        assert!(error.value_truncated);
    }

    #[test]
    fn submit_job_nested_limits_survive_cli_validation() {
        for (params, path, constraint, value) in [
            (
                json!({"package":"rsi","timeout_minutes":181}),
                "params.params.timeout_minutes",
                "maximum 180",
                json!(181),
            ),
            (
                json!({"package":"rsi","filters":["valid", "x".repeat(201)]}),
                "params.params.filters[1]",
                "maxLength 200",
                json!("x".repeat(128) + "…"),
            ),
            (
                json!({"shard":"other-01","qa_lane":{"sha":"a".repeat(40)},"timeout_minutes":91}),
                "params.params.timeout_minutes",
                "maximum 90",
                json!(91),
            ),
            (
                json!({"package":"rsi","cpu_quota_percent":1601}),
                "params.params.cpu_quota_percent",
                "maximum 1600",
                json!(1601),
            ),
            (
                json!({"package":"rsi","exact":"yes"}),
                "params.params.exact",
                "type \"boolean\"",
                json!("yes"),
            ),
        ] {
            let error = Verb::SubmitJob
                .validate_params(&json!({"kind":"test","params":params}))
                .unwrap_err();
            assert_eq!(error.path.as_deref(), Some(path));
            assert_eq!(error.constraint.as_deref(), Some(constraint));
            assert_eq!(error.value, Some(value));
        }
        for params in [
            json!({"package":"rsi","filters":["x".repeat(200)],"timeout_minutes":180}),
            json!({"recipe":"check-cpu","filters":["rsi=".to_string() + &"x".repeat(252)]}),
            json!({"scoped_test":{"base":"origin/rolling"},"timeout_minutes":20}),
        ] {
            let request = json!({"kind":"test","params":params});
            assert!(Verb::SubmitJob.validate_params(&request).is_ok());
            assert_eq!(Verb::SubmitJob.schema_error(&request), None);
        }
    }

    #[test]
    fn catalog_schema_diagnostics_cover_required_type_enum_pattern_and_format() {
        for descriptor in agent_control_catalog_v1() {
            let error = descriptor.verb.schema_error(&json!(42)).unwrap();
            assert_eq!(error.path.as_deref(), Some("params"));
            assert_eq!(error.constraint.as_deref(), Some("type \"object\""));
            assert_eq!(error.value, Some(json!(42)));
        }
        for (request, path, constraint) in [
            (
                json!({"kind":"test"}),
                "params.params",
                "required \"params\"",
            ),
            (
                json!({"kind":"unknown","params":{}}),
                "params.kind",
                "enum [\"test\",\"build\",\"landing\",\"cloud_gate\",\"cloud_sweep\"]",
            ),
            (
                json!({"kind":"landing","params":{"accepted":"BAD"}}),
                "params.params.accepted",
                "pattern \"^[0-9a-f]{40}$\"",
            ),
            (
                json!({"kind":"test","params":{},"project_id":"bad"}),
                "params.project_id",
                "format \"uuid\"",
            ),
        ] {
            let error = Verb::SubmitJob.schema_error(&request).unwrap();
            assert_eq!(error.path.as_deref(), Some(path));
            assert_eq!(error.constraint.as_deref(), Some(constraint));
        }
        let error = validate(&json!([1, 2]), &json!({"maxItems":1}), "params.items").unwrap_err();
        assert_eq!(error.value, None);
        let error = validate(
            &json!("é".repeat(200)),
            &json!({"maxLength":128}),
            "params.text",
        )
        .unwrap_err();
        assert_eq!(error.value, Some(json!("é".repeat(128) + "…")));
        let error = validate(
            &json!("é".repeat(100)),
            &json!({"maxLength":128}),
            "params.text",
        )
        .unwrap_err();
        assert_eq!(error.constraint.as_deref(), Some("maxLength 128"));
        let schema = json!({"oneOf":[
            {"type":"object","required":["action","long_other_field"],"properties":{"action":{"enum":["other"]}}},
            {"type":"object","required":["action","limit"],"properties":{"action":{"enum":["selected"]},"limit":{"maximum":1}}}
        ]});
        let error = validate(
            &json!({"action":"selected","limit":2}),
            &schema,
            "params.operation",
        )
        .unwrap_err();
        assert_eq!(error.path.as_deref(), Some("params.operation.limit"));
        assert_eq!(error.constraint.as_deref(), Some("maximum 1"));
    }

    #[test]
    fn diagnostics_preserve_existing_envelope_keys() {
        let error = Verb::ListWakes.schema_error(&json!({"limit":257})).unwrap();
        let mut envelope =
            json!({"code":"legacy_code","class":"legacy_class","field":"limit","retryable":false});
        error.enrich(&mut envelope);
        assert_eq!(envelope["code"], "legacy_code");
        assert_eq!(envelope["class"], "legacy_class");
        assert_eq!(envelope["field"], "limit");
        assert_eq!(envelope["retryable"], false);
        assert_eq!(envelope["path"], "params.limit");
        assert_eq!(envelope["constraint"], "maximum 256");
        assert_eq!(envelope["value"], 257);
    }
}
